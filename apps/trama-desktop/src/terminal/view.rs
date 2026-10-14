//! Inline terminals above the composer, bound to the selected thread.
//!
//! The daemon owns the PTYs (`OpenTerminal`, `TerminalOutput` base64 chunks,
//! `TerminalInput`, `ResizeTerminal`, `CloseTerminal`); each tab feeds its
//! bytes into a [`VtScreen`] and paints the grid with the mono font. Tabs
//! keep running when the panel is hidden and come back after a reconnect
//! (`ListTerminals` + `ReplayTerminal`). Visible, attached terminals ride
//! along with the next prompt as `SendPrompt.terminals`.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, tooltip::Tooltip, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::ThreadId;
use trama_protocol::{ClientMessage, DaemonMessage, TerminalExitStatus, TerminalSnapshot};

use crate::app::theme::{self, Palette};
use crate::system::net::{DaemonClient, SendStatus};
use crate::terminal::vt::{self, CellStyle, TermColor, VtScreen};

/// Key context of a focused terminal grid.
pub const CONTEXT: &str = "TramaTerminal";

gpui_kit::actions!(
    trama_terminal,
    [
        SendTab,
        SendBackTab,
        SendEscape,
        PasteToTerminal,
        ToggleTerminal
    ]
);

/// ⌘V on macOS; Ctrl+Shift+V elsewhere so Ctrl+V still reaches the PTY.
const PASTE_KEYS: &str = if cfg!(target_os = "macos") {
    "cmd-v"
} else {
    "ctrl-shift-v"
};

/// Keys that other contexts would otherwise take from a focused terminal,
/// and the window-wide ⌃` toggle.
pub fn bind(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", SendTab, Some(CONTEXT)),
        KeyBinding::new("shift-tab", SendBackTab, Some(CONTEXT)),
        KeyBinding::new("escape", SendEscape, Some(CONTEXT)),
        KeyBinding::new(PASTE_KEYS, PasteToTerminal, Some(CONTEXT)),
        KeyBinding::new("ctrl-`", ToggleTerminal, Some(crate::app::keymap::CONTEXT)),
    ]);
}

const FONT_SIZE: f32 = 12.;
const LINE_HEIGHT: f32 = 16.;
const PAD: f32 = 8.;
const ROWS: u16 = 14;
const DEFAULT_COLS: u16 = 100;
const MIN_COLS: u16 = 20;
/// Output buffered for a terminal the panel does not know yet.
const MAX_ORPHAN_BYTES: usize = 512 * 1024;

/// Bytes a key press sends to the PTY. `None` leaves the key to the app
/// (⌘ shortcuts, bare modifiers).
pub fn key_input(keystroke: &Keystroke, app_cursor: bool) -> Option<String> {
    let m = &keystroke.modifiers;
    if m.platform || m.function {
        return None;
    }
    let key = keystroke.key.as_str();
    let arrow = |code: char| {
        if app_cursor && !m.shift && !m.control && !m.alt {
            format!("\x1bO{code}")
        } else {
            format!("\x1b[{code}")
        }
    };
    let named = match key {
        "enter" => Some("\r".to_string()),
        "backspace" if m.alt => Some("\x1b\x7f".to_string()),
        "backspace" => Some("\x7f".to_string()),
        "tab" if m.shift => Some("\x1b[Z".to_string()),
        "tab" => Some("\t".to_string()),
        "escape" => Some("\x1b".to_string()),
        "up" => Some(arrow('A')),
        "down" => Some(arrow('B')),
        "right" if m.alt => Some("\x1bf".to_string()),
        "left" if m.alt => Some("\x1bb".to_string()),
        "right" => Some(arrow('C')),
        "left" => Some(arrow('D')),
        "home" => Some("\x1b[H".to_string()),
        "end" => Some("\x1b[F".to_string()),
        "pageup" => Some("\x1b[5~".to_string()),
        "pagedown" => Some("\x1b[6~".to_string()),
        "delete" => Some("\x1b[3~".to_string()),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if m.control {
        let mut chars = key.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return (key == "space").then(|| "\0".to_string());
        };
        let byte = match c.to_ascii_lowercase() {
            c @ 'a'..='z' => c as u8 - b'a' + 1,
            '@' | '2' | ' ' => 0,
            '[' | '3' => 0x1b,
            '\\' | '4' => 0x1c,
            ']' | '5' => 0x1d,
            '^' | '6' => 0x1e,
            '_' | '-' | '7' => 0x1f,
            '?' | '8' => 0x7f,
            _ => return None,
        };
        return Some((byte as char).to_string());
    }
    if key == "space" {
        return Some(if m.alt { "\x1b " } else { " " }.to_string());
    }
    if m.alt && key.chars().count() == 1 {
        // Option as Meta, like most terminal setups for shells.
        return Some(format!("\x1b{key}"));
    }
    keystroke
        .key_char
        .clone()
        .filter(|text| !text.is_empty())
        .or_else(|| (key.chars().count() == 1).then(|| key.to_string()))
}

/// Pasted text: newlines as carriage returns, bracketed when the program
/// asked for it.
pub fn paste_input(text: &str, bracketed: bool) -> String {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~")
    } else {
        text
    }
}

/// Short label for a tab or a composer chip.
pub fn terminal_label(command: Option<&str>) -> String {
    match command.map(str::trim).filter(|c| !c.is_empty()) {
        Some(command) => {
            let first = command.lines().next().unwrap_or(command);
            if first.chars().count() > 40 {
                format!("{}…", first.chars().take(39).collect::<String>())
            } else {
                first.to_string()
            }
        }
        None => "Shell".to_string(),
    }
}

pub fn exit_label(status: &TerminalExitStatus) -> String {
    match (status.code, status.signal) {
        (Some(code), _) => format!("exited with code {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        _ => "exited".to_string(),
    }
}

struct TermTab {
    id: String,
    command: Option<String>,
    cwd: PathBuf,
    exit: Option<TerminalExitStatus>,
    screen: VtScreen,
    last_seq: u64,
    attached: bool,
}

impl TermTab {
    fn from_snapshot(snapshot: &TerminalSnapshot, cols: u16, rows: u16) -> Self {
        Self {
            id: snapshot.terminal_id.clone(),
            command: snapshot.command.clone(),
            cwd: snapshot.cwd.clone(),
            exit: snapshot.exit_status,
            screen: VtScreen::new(cols, rows),
            last_seq: 0,
            attached: false,
        }
    }

    fn label(&self) -> String {
        terminal_label(self.command.as_deref())
    }
}

/// Requests to the chat panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// The panel was hidden or its last tab closed: focus the composer.
    Hidden,
}

pub struct TerminalPanel {
    client: DaemonClient,
    thread: Option<ThreadId>,
    tabs: Vec<TermTab>,
    active: usize,
    open: bool,
    opening: bool,
    focus_pending: bool,
    focus: FocusHandle,
    grid_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    cell_width: Option<Pixels>,
    size: (u16, u16),
    orphans: HashMap<String, Vec<(u64, Vec<u8>)>>,
    error: Option<String>,
}

impl EventEmitter<TerminalEvent> for TerminalPanel {}

impl TerminalPanel {
    pub fn new(client: DaemonClient, cx: &mut Context<Self>) -> Self {
        Self {
            client,
            thread: None,
            tabs: Vec::new(),
            active: 0,
            open: false,
            opening: false,
            focus_pending: false,
            focus: cx.focus_handle(),
            grid_bounds: Rc::new(Cell::new(None)),
            cell_width: None,
            size: (DEFAULT_COLS, ROWS),
            orphans: HashMap::new(),
            error: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Terminals still running in the background (for the toolbar badge).
    pub fn running_count(&self) -> usize {
        self.tabs.iter().filter(|tab| tab.exit.is_none()).count()
    }

    /// `(terminal id, label)` of the terminals the next prompt carries.
    pub fn attachments(&self) -> Vec<(String, String)> {
        if !self.open {
            return Vec::new();
        }
        self.tabs
            .iter()
            .filter(|tab| tab.attached)
            .map(|tab| (tab.id.clone(), tab.label()))
            .collect()
    }

    pub fn detach(&mut self, terminal_id: &str, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == terminal_id) {
            tab.attached = false;
            cx.notify();
        }
    }

    /// After a send: the snapshot is in the transcript now.
    pub fn clear_attachments(&mut self, cx: &mut Context<Self>) {
        for tab in &mut self.tabs {
            tab.attached = false;
        }
        cx.notify();
    }

    pub fn set_thread(
        &mut self,
        thread: Option<ThreadId>,
        connected: bool,
        cx: &mut Context<Self>,
    ) {
        if self.thread == thread {
            return;
        }
        self.thread = thread;
        self.tabs.clear();
        self.orphans.clear();
        self.active = 0;
        self.open = false;
        self.opening = false;
        self.error = None;
        if connected {
            self.list();
        }
        cx.notify();
    }

    /// The socket is back: terminals kept running, so list and replay them.
    pub fn reconnected(&mut self, cx: &mut Context<Self>) {
        self.opening = false;
        self.list();
        cx.notify();
    }

    fn list(&mut self) {
        if let Some(thread_id) = self.thread.clone() {
            self.client.send(ClientMessage::ListTerminals { thread_id });
        }
    }

    /// Start a terminal (`None`: an interactive shell) and show it.
    pub fn open_terminal(&mut self, command: Option<String>, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread.clone() else {
            self.error = Some("Select a thread to open a terminal in its checkout.".into());
            cx.notify();
            return;
        };
        let (cols, rows) = self.size;
        let sent = self.client.send(ClientMessage::OpenTerminal {
            thread_id,
            repository_id: None,
            command: command.filter(|c| !c.trim().is_empty()),
            cols,
            rows,
        });
        if sent == SendStatus::Enqueued {
            self.opening = true;
            self.open = true;
            self.error = None;
        } else {
            self.error = Some("The daemon is not connected; no terminal was started.".into());
        }
        cx.notify();
    }

    /// ⌃` and the toolbar button: show the terminals (starting a shell when
    /// there is none) or hide them.
    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.hide(cx);
        } else if self.tabs.is_empty() {
            self.open_terminal(None, cx);
        } else {
            self.open = true;
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    fn hide(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.emit(TerminalEvent::Hidden);
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(ix);
        self.client.send(ClientMessage::CloseTerminal {
            terminal_id: tab.id,
        });
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        if self.tabs.is_empty() {
            self.hide(cx);
        }
        cx.notify();
    }

    fn send_input(&mut self, data: String, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.exit.is_some() || data.is_empty() {
            return;
        }
        tab.screen.scroll_to_bottom();
        self.client.send(ClientMessage::TerminalInput {
            terminal_id: tab.id.clone(),
            data,
        });
        cx.notify();
    }

    fn app_cursor(&self) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.screen.app_cursor())
    }

    fn add_tab(&mut self, snapshot: &TerminalSnapshot) -> usize {
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|tab| tab.id == snapshot.terminal_id)
        {
            self.tabs[ix].exit = snapshot.exit_status;
            return ix;
        }
        let (cols, rows) = self.size;
        let mut tab = TermTab::from_snapshot(snapshot, cols, rows);
        if let Some(mut chunks) = self.orphans.remove(&snapshot.terminal_id) {
            chunks.sort_by_key(|(seq, _)| *seq);
            for (seq, bytes) in chunks {
                if seq > tab.last_seq {
                    tab.screen.feed(&bytes);
                    tab.last_seq = seq;
                }
            }
            tab.screen.take_replies();
        }
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    pub fn apply(&mut self, message: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        let current = self.thread.clone();
        let ours = |thread_id: &ThreadId| current.as_ref() == Some(thread_id);
        match message {
            DaemonMessage::TerminalOpened {
                thread_id,
                terminal,
            } => {
                if ours(thread_id) {
                    let ix = self.add_tab(terminal);
                    if self.opening {
                        self.opening = false;
                        self.active = ix;
                        self.open = true;
                        self.focus_pending = true;
                        self.tabs[ix].attached = true;
                    }
                    cx.notify();
                }
                true
            }
            DaemonMessage::TerminalOutput {
                thread_id,
                terminal_id,
                data,
                seq,
                replay,
            } => {
                if !ours(thread_id) {
                    return true;
                }
                let bytes = match vt::decode_output(data) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        self.error = Some(err);
                        cx.notify();
                        return true;
                    }
                };
                let Some(tab) = self.tabs.iter_mut().find(|tab| &tab.id == terminal_id) else {
                    let chunks = self.orphans.entry(terminal_id.clone()).or_default();
                    if *replay {
                        chunks.clear();
                    }
                    if chunks.iter().map(|(_, b)| b.len()).sum::<usize>() < MAX_ORPHAN_BYTES {
                        chunks.push((*seq, bytes));
                    }
                    return true;
                };
                if *replay {
                    tab.screen.reset();
                    tab.screen.feed(&bytes);
                    tab.screen.take_replies();
                    tab.last_seq = *seq;
                } else if *seq > tab.last_seq {
                    tab.screen.feed(&bytes);
                    tab.last_seq = *seq;
                    let terminal_id = tab.id.clone();
                    for data in tab.screen.take_replies() {
                        self.client.send(ClientMessage::TerminalInput {
                            terminal_id: terminal_id.clone(),
                            data,
                        });
                    }
                }
                cx.notify();
                true
            }
            DaemonMessage::TerminalExited {
                thread_id,
                terminal_id,
                status,
            } => {
                if ours(thread_id)
                    && let Some(tab) = self.tabs.iter_mut().find(|tab| &tab.id == terminal_id)
                {
                    tab.exit = Some(*status);
                    cx.notify();
                }
                true
            }
            DaemonMessage::Terminals {
                thread_id,
                terminals,
            } => {
                if ours(thread_id) {
                    self.tabs
                        .retain(|tab| terminals.iter().any(|t| t.terminal_id == tab.id));
                    for snapshot in terminals {
                        let known = self.tabs.iter().any(|t| t.id == snapshot.terminal_id);
                        self.add_tab(snapshot);
                        if !known || snapshot.last_seq > 0 {
                            self.client.send(ClientMessage::ReplayTerminal {
                                terminal_id: snapshot.terminal_id.clone(),
                            });
                        }
                    }
                    self.active = self.active.min(self.tabs.len().saturating_sub(1));
                    if self.tabs.is_empty() {
                        self.open = self.opening;
                    }
                    cx.notify();
                }
                true
            }
            DaemonMessage::Error { message, .. } if self.opening => {
                // Most likely the `OpenTerminal` (no ready checkout, limit
                // reached). The chat banner shows it too.
                self.opening = false;
                self.error = Some(message.clone());
                if self.tabs.is_empty() {
                    self.open = false;
                }
                cx.notify();
                false
            }
            _ => false,
        }
    }

    /// Fit the grid to the panel width once it is known.
    fn sync_size(&mut self, cx: &mut Context<Self>) {
        let (Some(bounds), Some(cell)) = (self.grid_bounds.get(), self.cell_width) else {
            return;
        };
        let usable = f32::from(bounds.size.width) - 2. * PAD;
        let cols = ((usable / f32::from(cell)).floor() as u16).max(MIN_COLS);
        let size = (cols, ROWS);
        let mut changed = self.size != size;
        self.size = size;
        for tab in &mut self.tabs {
            if (tab.screen.cols(), tab.screen.rows()) == size {
                continue;
            }
            tab.screen.resize(cols, ROWS);
            changed = true;
            if tab.exit.is_none() {
                self.client.send(ClientMessage::ResizeTerminal {
                    terminal_id: tab.id.clone(),
                    cols,
                    rows: ROWS,
                });
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn render_tab_bar(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut bar = h_flex()
            .w_full()
            .h(px(32.))
            .px(theme::sp(1.5))
            .gap(theme::sp(1.))
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .bg(p.panel);
        for (ix, tab) in self.tabs.iter().enumerate() {
            let active = ix == self.active;
            let dot = match tab.exit {
                None => p.success,
                Some(TerminalExitStatus { code: Some(0), .. }) => p.text_faint,
                Some(_) => p.danger,
            };
            let tooltip: SharedString = format!(
                "{}\n{}{}",
                tab.command.as_deref().unwrap_or("Interactive shell"),
                tab.cwd.display(),
                tab.exit
                    .as_ref()
                    .map(|status| format!("\n{}", exit_label(status)))
                    .unwrap_or_default()
            )
            .into();
            bar = bar.child(
                h_flex()
                    .id(("terminal-tab", ix))
                    .h(px(24.))
                    .max_w(px(200.))
                    .pl(theme::sp(2.))
                    .pr(theme::sp(1.))
                    .gap(theme::sp(1.5))
                    .items_center()
                    .rounded(theme::radius())
                    .cursor_pointer()
                    .text_size(theme::text_xs())
                    .text_color(if active { p.text } else { p.text_muted })
                    .when(active, |this| {
                        this.bg(p.elevated).border_1().border_color(p.border)
                    })
                    .when(!active, |this| this.hover(|style| style.bg(p.hover)))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.active = ix;
                        this.focus.focus(window, cx);
                        cx.notify();
                    }))
                    .child(div().size(px(6.)).flex_shrink_0().rounded_full().bg(dot))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(theme::MONO_FONT)
                            .child(tab.label()),
                    )
                    .when(tab.attached, |this| {
                        this.child(
                            Icon::new(IconName::Paperclip)
                                .with_size(px(10.))
                                .text_color(p.accent_text),
                        )
                    })
                    .child(
                        div()
                            .id(("terminal-tab-close", ix))
                            .size(px(16.))
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .rounded(theme::radius_sm())
                            .text_color(p.text_faint)
                            .hover(|style| style.bg(p.selected).text_color(p.text))
                            .tooltip(|window, cx| {
                                Tooltip::new("Close (stops the process)").build(window, cx)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, cx);
                            }))
                            .child(Icon::new(IconName::Close).with_size(px(10.))),
                    ),
            );
        }
        if self.opening {
            bar = bar.child(
                div()
                    .px(theme::sp(2.))
                    .text_size(theme::text_xs())
                    .text_color(p.text_faint)
                    .child("Starting…"),
            );
        }
        let icon_button = |id: &'static str, icon: IconName, tip: &'static str, on: bool| {
            div()
                .id(id)
                .size(px(24.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(theme::radius())
                .cursor_pointer()
                .text_color(if on { p.accent_text } else { p.text_muted })
                .when(on, |this| this.bg(p.accent_soft))
                .hover(|style| style.bg(p.hover).text_color(p.text))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .child(Icon::new(icon).with_size(px(13.)))
        };
        let attached = self.tabs.get(self.active).is_some_and(|tab| tab.attached);
        bar.child(div().flex_1())
            .when(!self.tabs.is_empty(), |this| {
                this.child(
                    icon_button(
                        "terminal-attach",
                        IconName::Paperclip,
                        "Attach this terminal's output to the next message",
                        attached,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(tab) = this.tabs.get_mut(this.active) {
                            tab.attached = !tab.attached;
                            cx.notify();
                        }
                    })),
                )
            })
            .child(
                icon_button("terminal-new", IconName::Plus, "New terminal", false)
                    .on_click(cx.listener(|this, _, _, cx| this.open_terminal(None, cx))),
            )
            .child(
                icon_button(
                    "terminal-hide",
                    IconName::ChevronDown,
                    "Hide (terminals keep running) \u{2303}`",
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.hide(cx))),
            )
    }

    fn render_grid(&self, p: &Palette, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let cell = f32::from(self.cell_width.unwrap_or(px(7.2)));
        let height = ROWS as f32 * LINE_HEIGHT + 2. * PAD;
        let measured = self.grid_bounds.clone();
        let measure = canvas(
            move |bounds, window, _| {
                let previous = measured.replace(Some(bounds));
                if previous.map(|b| b.size.width) != Some(bounds.size.width) {
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        let mut grid = div()
            .id("terminal-grid")
            .track_focus(&self.focus)
            .key_context(CONTEXT)
            .relative()
            .w_full()
            .h(px(height))
            .p(px(PAD))
            .overflow_hidden()
            .bg(p.bg)
            .cursor(CursorStyle::IBeam)
            .font_family(theme::MONO_FONT)
            .text_size(px(FONT_SIZE))
            .line_height(px(LINE_HEIGHT))
            .text_color(p.text)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.focus.focus(window, cx);
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if let Some(data) = key_input(&event.keystroke, this.app_cursor()) {
                    cx.stop_propagation();
                    this.send_input(data, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SendTab, _, cx| this.send_input("\t".into(), cx)))
            .on_action(
                cx.listener(|this, _: &SendBackTab, _, cx| this.send_input("\x1b[Z".into(), cx)),
            )
            .on_action(
                cx.listener(|this, _: &SendEscape, _, cx| this.send_input("\x1b".into(), cx)),
            )
            .on_action(cx.listener(|this, _: &PasteToTerminal, _, cx| {
                let text = cx.read_from_clipboard().and_then(|item| item.text());
                let bracketed = this
                    .tabs
                    .get(this.active)
                    .is_some_and(|tab| tab.screen.bracketed_paste());
                if let Some(text) = text {
                    this.send_input(paste_input(&text, bracketed), cx);
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                let delta = event.delta.pixel_delta(window.line_height()).y;
                let lines = (f32::from(delta) / LINE_HEIGHT).round() as i32;
                if lines != 0
                    && let Some(tab) = this.tabs.get_mut(this.active)
                {
                    tab.screen.scroll(lines);
                    cx.notify();
                }
            }))
            .child(measure);

        let Some(tab) = self.tabs.get(self.active) else {
            return grid
                .child(div().text_color(p.text_faint).child(if self.opening {
                    "Starting a terminal in this thread's checkout…"
                } else {
                    "No terminal"
                }))
                .into_any_element();
        };
        for runs in tab.screen.styled_rows() {
            grid = grid.child(
                div()
                    .h(px(LINE_HEIGHT))
                    .whitespace_nowrap()
                    .child(styled_row(&runs, p)),
            );
        }
        if let Some((row, col)) = tab.screen.cursor().filter(|_| tab.exit.is_none()) {
            let caret = div()
                .absolute()
                .left(px(PAD + col as f32 * cell))
                .top(px(PAD + row as f32 * LINE_HEIGHT))
                .w(px(cell))
                .h(px(LINE_HEIGHT));
            grid = grid.child(if focused {
                caret.bg(p.accent.opacity(0.55))
            } else {
                caret.border_1().border_color(p.text_faint)
            });
        }
        grid.into_any_element()
    }
}

fn term_color(color: TermColor, p: &Palette) -> Option<Hsla> {
    match color {
        TermColor::Fg => Some(p.text),
        TermColor::Bg => None,
        TermColor::Ansi(n) => Some(theme::ansi(n, p)),
        TermColor::Rgb(r, g, b) => {
            Some(rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into())
        }
    }
}

fn highlight(style: &CellStyle, p: &Palette) -> HighlightStyle {
    let fg = match style.fg {
        // Inverse of the default colors.
        TermColor::Bg => p.bg,
        other => term_color(other, p).unwrap_or(p.text),
    };
    let bg = match style.bg {
        TermColor::Fg => Some(p.text),
        other => term_color(other, p),
    };
    HighlightStyle {
        color: Some(if style.dim { fg.opacity(0.6) } else { fg }),
        background_color: bg,
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        underline: style.underline.then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(fg),
            wavy: false,
        }),
        strikethrough: style.strikeout.then(|| StrikethroughStyle {
            thickness: px(1.),
            color: Some(fg),
        }),
        ..Default::default()
    }
}

fn styled_row(runs: &[vt::Run], p: &Palette) -> StyledText {
    let mut text = String::new();
    let mut highlights = Vec::new();
    for run in runs {
        let start = text.len();
        text.push_str(&run.text);
        if run.style != CellStyle::default() {
            highlights.push((start..text.len(), highlight(&run.style, p)));
        }
    }
    if text.is_empty() {
        text.push(' ');
    }
    StyledText::new(text).with_highlights(highlights)
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.cell_width.is_none() {
            let font_id = window.text_system().resolve_font(&font(theme::MONO_FONT));
            self.cell_width = window
                .text_system()
                .advance(font_id, px(FONT_SIZE), 'm')
                .ok()
                .map(|size| size.width);
        }
        self.sync_size(cx);
        if self.focus_pending {
            self.focus_pending = false;
            self.focus.focus(window, cx);
        }
        let p = theme::palette(cx);
        let focused = self.focus.is_focused(window);
        let status = self.tabs.get(self.active).and_then(|tab| {
            tab.exit.as_ref().map(|status| {
                (
                    format!("Process {}", exit_label(status)),
                    status.code == Some(0),
                )
            })
        });
        v_flex()
            .w_full()
            .mb(theme::sp(2.))
            .rounded(theme::radius_lg())
            .border_1()
            .border_color(if focused { p.border_strong } else { p.border })
            .bg(p.elevated)
            .overflow_hidden()
            .child(self.render_tab_bar(&p, cx))
            .child(self.render_grid(&p, focused, cx))
            .when_some(status, |this, (label, ok)| {
                this.child(
                    h_flex()
                        .px(theme::sp(3.))
                        .py(theme::sp(1.))
                        .gap(theme::sp(1.5))
                        .items_center()
                        .border_t_1()
                        .border_color(p.border)
                        .text_size(theme::text_xs())
                        .text_color(if ok { p.text_faint } else { p.danger })
                        .child(label),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .px(theme::sp(3.))
                        .py(theme::sp(1.))
                        .border_t_1()
                        .border_color(p.border)
                        .text_size(theme::text_xs())
                        .text_color(p.warning)
                        .child(error),
                )
            })
    }
}

/// The "Attached terminal" chip above the composer text.
pub fn attachment_chip(
    id: impl Into<ElementId>,
    label: &str,
    p: &Palette,
    on_remove: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    h_flex()
        .h(px(22.))
        .max_w(px(320.))
        .pl(theme::sp(2.))
        .pr(theme::sp(1.))
        .gap(theme::sp(1.5))
        .items_center()
        .rounded(theme::radius())
        .border_1()
        .border_color(p.border)
        .bg(p.bg)
        .text_size(theme::text_xs())
        .text_color(p.text_muted)
        .child(
            Icon::new(IconName::SquareTerminal)
                .with_size(px(12.))
                .text_color(p.accent_text),
        )
        .child(div().flex_shrink_0().child("Attached terminal:"))
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_family(theme::MONO_FONT)
                .text_color(p.text)
                .child(label.to_string()),
        )
        .child(
            div()
                .id(id)
                .size(px(16.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(theme::radius_sm())
                .cursor_pointer()
                .text_color(p.text_faint)
                .hover(|style| style.bg(p.hover).text_color(p.text))
                .on_click(on_remove)
                .child(Icon::new(IconName::Close).with_size(px(10.))),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` brings GPUI's own `test` attribute into scope.
    use core::prelude::v1::test;

    fn keys(spec: &str, app_cursor: bool) -> Option<String> {
        let mut keystroke = Keystroke::parse(spec).unwrap();
        if keystroke.key.chars().count() == 1 && !keystroke.modifiers.control {
            keystroke.key_char = Some(if keystroke.modifiers.shift {
                keystroke.key.to_uppercase()
            } else {
                keystroke.key.clone()
            });
        }
        key_input(&keystroke, app_cursor)
    }

    #[test]
    fn keys_map_to_terminal_input() {
        assert_eq!(keys("a", false).as_deref(), Some("a"));
        assert_eq!(keys("shift-a", false).as_deref(), Some("A"));
        assert_eq!(keys("enter", false).as_deref(), Some("\r"));
        assert_eq!(keys("backspace", false).as_deref(), Some("\x7f"));
        assert_eq!(keys("tab", false).as_deref(), Some("\t"));
        assert_eq!(keys("shift-tab", false).as_deref(), Some("\x1b[Z"));
        assert_eq!(keys("escape", false).as_deref(), Some("\x1b"));
        assert_eq!(keys("space", false).as_deref(), Some(" "));
        assert_eq!(keys("ctrl-c", false).as_deref(), Some("\x03"));
        assert_eq!(keys("ctrl-d", false).as_deref(), Some("\x04"));
        assert_eq!(keys("ctrl-[", false).as_deref(), Some("\x1b"));
        assert_eq!(keys("ctrl-space", false).as_deref(), Some("\0"));
        assert_eq!(keys("alt-b", false).as_deref(), Some("\x1bb"));
        assert_eq!(
            keys("cmd-c", false),
            None,
            "app shortcuts stay with the app"
        );
    }

    #[test]
    fn arrows_follow_application_cursor_mode() {
        assert_eq!(keys("up", false).as_deref(), Some("\x1b[A"));
        assert_eq!(keys("left", false).as_deref(), Some("\x1b[D"));
        assert_eq!(keys("up", true).as_deref(), Some("\x1bOA"));
        assert_eq!(keys("pageup", false).as_deref(), Some("\x1b[5~"));
        assert_eq!(keys("delete", false).as_deref(), Some("\x1b[3~"));
    }

    #[test]
    fn paste_uses_carriage_returns_and_brackets() {
        assert_eq!(paste_input("a\nb\r\nc", false), "a\rb\rc");
        assert_eq!(paste_input("ls", true), "\x1b[200~ls\x1b[201~");
    }

    #[test]
    fn labels_are_short() {
        assert_eq!(terminal_label(None), "Shell");
        assert_eq!(terminal_label(Some("  ")), "Shell");
        assert_eq!(terminal_label(Some("cargo test\nmore")), "cargo test");
        assert_eq!(terminal_label(Some(&"x".repeat(60))).chars().count(), 40);
        assert_eq!(
            exit_label(&TerminalExitStatus {
                code: Some(101),
                signal: None
            }),
            "exited with code 101"
        );
        assert_eq!(
            exit_label(&TerminalExitStatus {
                code: None,
                signal: Some(9)
            }),
            "killed by signal 9"
        );
    }
}
