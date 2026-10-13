//! Context the daemon adds around a stored prompt: the `Referenced:` list
//! of `@` mentions and `<terminal-output>` snapshots of attached terminals.
//! The transcript shows them as chips and frozen terminal blocks instead of
//! raw text, and keeps the person's own words as the message body.
//!
//! Layout (see `docs/composer.md`): `[context]\n\n<text>`, or for a slash
//! command `<text>\n\n[context]`, where context is an optional
//! `Referenced:` block followed by terminal blocks, separated by blank
//! lines.

use std::collections::HashSet;
use std::ops::Range;

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, tooltip::Tooltip, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::MessageId;

use crate::chat::ChatPanel;
use crate::theme::{self, Palette};

/// Lines a collapsed snapshot shows (its tail).
const COLLAPSED_LINES: usize = 4;
/// Lines an expanded snapshot renders at most.
const MAX_LINES: usize = 400;

const REFERENCED: &str = "Referenced:\n";
const TERMINAL_OPEN: &str = "<terminal-output ";
const TERMINAL_CLOSE: &str = "\n</terminal-output>";
const OMITTED: &str = "[earlier output omitted]\n";

/// One `Referenced:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    File(String),
    Directory(String),
    Thread(String),
    Person(String),
}

impl Reference {
    /// Short chip text: a file name, a folder name, a thread title, a name.
    pub fn short_label(&self) -> String {
        match self {
            Reference::File(path) | Reference::Directory(path) => {
                let trimmed = path.trim_end_matches('/');
                trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
            }
            Reference::Thread(title) | Reference::Person(title) => title.clone(),
        }
    }
}

/// A frozen terminal snapshot from a sent prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalBlock {
    pub command: String,
    pub cwd: String,
    pub status: String,
    pub output: String,
    pub omitted: bool,
}

/// A user message body split into context and the person's text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptContext {
    pub references: Vec<Reference>,
    pub terminals: Vec<TerminalBlock>,
    /// The typed text, as a byte range of the body.
    pub text: Range<usize>,
}

impl PromptContext {
    pub fn is_empty(&self) -> bool {
        self.references.is_empty() && self.terminals.is_empty()
    }
}

/// Split `body` into context blocks and text. A body without context is
/// all text.
pub fn split_context(body: &str) -> PromptContext {
    // Context first.
    if let Some((references, terminals, end)) = parse_blocks(body)
        && (!references.is_empty() || !terminals.is_empty())
    {
        let text_start = if body[end..].starts_with("\n\n") {
            end + 2
        } else if end == body.len() {
            end
        } else {
            return plain(body);
        };
        return PromptContext {
            references,
            terminals,
            text: text_start..body.len(),
        };
    }
    // A slash command: the context follows the first blank line that starts
    // a run of context blocks reaching the end.
    if body.starts_with('/') {
        let mut from = 0;
        while let Some(found) = body[from..].find("\n\n") {
            let at = from + found;
            let rest = &body[at + 2..];
            if let Some((references, terminals, end)) = parse_blocks(rest)
                && (!references.is_empty() || !terminals.is_empty())
                && end == rest.len()
            {
                return PromptContext {
                    references,
                    terminals,
                    text: 0..at,
                };
            }
            from = at + 1;
        }
    }
    plain(body)
}

fn plain(body: &str) -> PromptContext {
    PromptContext {
        text: 0..body.len(),
        ..PromptContext::default()
    }
}

/// Parse context blocks at the start of `text`: returns them and the byte
/// offset just past the last one (before any `\n\n` separator).
fn parse_blocks(text: &str) -> Option<(Vec<Reference>, Vec<TerminalBlock>, usize)> {
    let mut references = Vec::new();
    let mut terminals = Vec::new();
    let mut at = 0;
    if text.starts_with(REFERENCED) {
        let (refs, end) = parse_references(text)?;
        references = refs;
        at = end;
    }
    loop {
        let start = if at == 0 {
            0
        } else if text[at..].starts_with(&format!("\n\n{TERMINAL_OPEN}")) {
            at + 2
        } else {
            break;
        };
        if !text[start..].starts_with(TERMINAL_OPEN) {
            break;
        }
        let Some((block, end)) = parse_terminal(&text[start..]) else {
            break;
        };
        terminals.push(block);
        at = start + end;
    }
    Some((references, terminals, at))
}

fn parse_references(text: &str) -> Option<(Vec<Reference>, usize)> {
    let mut references = Vec::new();
    let mut at = REFERENCED.len();
    let mut end = at;
    for line in text[REFERENCED.len()..].split_inclusive('\n') {
        let content = line.trim_end_matches('\n');
        if let Some(item) = content.strip_prefix("- ") {
            references.push(parse_reference(item));
        } else if content.starts_with("  ") && !references.is_empty() {
            // Continuation of a thread excerpt.
        } else {
            break;
        }
        end = at + content.len();
        at += line.len();
    }
    (!references.is_empty()).then_some((references, end))
}

fn parse_reference(item: &str) -> Reference {
    if let Some(path) = item.strip_prefix("file: ") {
        Reference::File(path.to_string())
    } else if let Some(path) = item.strip_prefix("directory: ") {
        Reference::Directory(path.to_string())
    } else if let Some(name) = item.strip_prefix("person: ") {
        let name = name
            .strip_suffix(" (the user writing this prompt)")
            .unwrap_or(name);
        Reference::Person(name.to_string())
    } else if let Some(rest) = item.strip_prefix("thread \"") {
        let title = rest.split_once("\" (").map_or(rest, |(title, _)| title);
        Reference::Thread(title.to_string())
    } else {
        Reference::Thread(item.to_string())
    }
}

/// One `<terminal-output …>` block at the start of `text`.
fn parse_terminal(text: &str) -> Option<(TerminalBlock, usize)> {
    let header_end = text.find(">\n")?;
    let header = &text[TERMINAL_OPEN.len()..header_end];
    let body_start = header_end + 2;
    // An empty snapshot closes right after the header.
    let (output, end) = if text[header_end + 1..].starts_with(TERMINAL_CLOSE) {
        (String::new(), header_end + 1 + TERMINAL_CLOSE.len())
    } else {
        let close = text[body_start..].find(TERMINAL_CLOSE)? + body_start;
        (
            text[body_start..close].to_string(),
            close + TERMINAL_CLOSE.len(),
        )
    };
    let (omitted, output) = if let Some(rest) = output.strip_prefix(OMITTED) {
        (true, rest.to_string())
    } else if output == OMITTED.trim_end() {
        (true, String::new())
    } else {
        (false, output)
    };
    Some((
        TerminalBlock {
            command: attribute(header, "command").unwrap_or_default(),
            cwd: attribute(header, "cwd").unwrap_or_default(),
            status: attribute(header, "status").unwrap_or_default(),
            output,
            omitted,
        },
        end,
    ))
}

/// `name="value"` from a tag header, unescaping the XML entities the daemon
/// writes.
fn attribute(header: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = header.find(&needle)? + needle.len();
    let end = header[start..].find('"')? + start;
    Some(
        header[start..end]
            .replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

/// Whether a snapshot's status means the command failed.
pub fn status_failed(status: &str) -> bool {
    status.starts_with("killed")
        || (status.starts_with("exited with code") && !status.ends_with(" 0"))
}

impl Reference {
    fn icon(&self) -> IconName {
        match self {
            Reference::File(_) => IconName::File,
            Reference::Directory(_) => IconName::Folder,
            Reference::Thread(_) => IconName::MessageSquare,
            Reference::Person(_) => IconName::User,
        }
    }

    fn full(&self) -> String {
        match self {
            Reference::File(path) | Reference::Directory(path) => path.clone(),
            Reference::Thread(title) => format!("Thread \u{201c}{title}\u{201d}"),
            Reference::Person(name) => name.clone(),
        }
    }
}

/// Mention chips and frozen terminal snapshots of a sent prompt.
pub fn render_context(
    message_id: &MessageId,
    context: &PromptContext,
    open: &HashSet<(MessageId, usize)>,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let mut column = v_flex().w_full().gap(theme::sp(2.));
    if !context.references.is_empty() {
        let mut chips = h_flex().w_full().flex_wrap().gap(theme::sp(1.5));
        for (ix, reference) in context.references.iter().enumerate() {
            let tooltip: SharedString = reference.full().into();
            chips = chips.child(
                h_flex()
                    .id(("prompt-reference", ix))
                    .h(px(22.))
                    .max_w(px(260.))
                    .px(theme::sp(2.))
                    .gap(theme::sp(1.5))
                    .items_center()
                    .rounded(theme::radius())
                    .bg(p.accent_soft)
                    .text_size(theme::text_xs())
                    .text_color(p.accent_text)
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(Icon::new(reference.icon()).with_size(px(11.)))
                    .child(div().min_w_0().truncate().child(reference.short_label())),
            );
        }
        column = column.child(chips);
    }
    for (ix, block) in context.terminals.iter().enumerate() {
        let key = (message_id.clone(), ix);
        let expanded = open.contains(&key);
        column = column.child(render_terminal(block, ix, key, expanded, p, cx));
    }
    column.into_any_element()
}

fn render_terminal(
    block: &TerminalBlock,
    ix: usize,
    key: (MessageId, usize),
    expanded: bool,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let failed = status_failed(&block.status);
    let status_color = if failed {
        p.danger
    } else if block.status == "running" {
        p.accent_text
    } else {
        p.success
    };
    let lines: Vec<&str> = if block.output.is_empty() {
        Vec::new()
    } else {
        block.output.lines().collect()
    };
    let shown = if expanded {
        lines.len().min(MAX_LINES)
    } else {
        lines.len().min(COLLAPSED_LINES)
    };
    let first = if expanded { 0 } else { lines.len() - shown };
    let hidden = lines.len() - shown;
    let tooltip: SharedString = if block.cwd.is_empty() {
        block.command.clone().into()
    } else {
        format!("{}\nin {}", block.command, block.cwd).into()
    };

    let header = h_flex()
        .id(("snapshot-header", ix))
        .w_full()
        .px(theme::sp(2.5))
        .py(theme::sp(1.5))
        .gap(theme::sp(2.))
        .items_center()
        .cursor_pointer()
        .text_size(theme::text_sm())
        .hover(|style| style.bg(p.hover))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_snapshot(key.clone(), cx)))
        .child(
            Icon::new(IconName::SquareTerminal)
                .with_size(px(13.))
                .text_color(p.text_muted),
        )
        .child(
            div()
                .flex_shrink_0()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text)
                .child("Terminal snapshot"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme::MONO_FONT)
                .text_size(theme::text_xs())
                .text_color(p.text_muted)
                .child(block.command.clone()),
        )
        .when(!block.status.is_empty(), |this| {
            this.child(
                div()
                    .flex_shrink_0()
                    .h(px(18.))
                    .px(theme::sp(1.5))
                    .flex()
                    .items_center()
                    .rounded(theme::radius_sm())
                    .bg(status_color.opacity(0.12))
                    .font_family(theme::MONO_FONT)
                    .text_size(theme::text_xs())
                    .text_color(status_color)
                    .child(block.status.clone()),
            )
        })
        .child(
            Icon::new(if expanded {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            })
            .with_size(px(12.))
            .text_color(p.text_faint),
        );

    let mut output = v_flex()
        .flex_shrink_0()
        .min_w_full()
        .px(theme::sp(2.5))
        .py(theme::sp(1.5))
        .font_family(theme::MONO_FONT)
        .text_size(theme::text_xs())
        .line_height(relative(1.5))
        .text_color(p.text_muted);
    if expanded && block.omitted {
        output = output.child(
            div()
                .text_color(p.text_faint)
                .child("[earlier output omitted]"),
        );
    } else if !expanded && (hidden > 0 || block.omitted) {
        output = output.child(div().text_color(p.text_faint).child(if hidden > 0 {
            format!(
                "\u{2026} {}",
                crate::comments::plural(hidden, "earlier line", "earlier lines")
            )
        } else {
            "\u{2026}".to_string()
        }));
    }
    if lines.is_empty() {
        output = output.child(div().text_color(p.text_faint).child("(no output)"));
    }
    for line in &lines[first..first + shown] {
        output = output.child(div().whitespace_nowrap().child(if line.is_empty() {
            " ".to_string()
        } else {
            line.to_string()
        }));
    }
    if expanded && lines.len() > shown {
        output = output.child(div().text_color(p.text_faint).child(format!(
            "\u{2026} {} more lines not shown",
            lines.len() - shown
        )));
    }

    v_flex()
        .w_full()
        .rounded(theme::radius())
        .border_1()
        .border_color(if failed {
            p.danger.opacity(0.4)
        } else {
            p.border
        })
        .bg(p.elevated)
        .overflow_hidden()
        .child(header)
        .child(
            h_flex()
                .id(("snapshot-output", ix))
                .w_full()
                .border_t_1()
                .border_color(p.border)
                .bg(p.bg)
                .overflow_x_scroll()
                .child(output),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` brings GPUI's own `test` attribute into scope.
    use core::prelude::v1::test;

    const SNAP: &str = "<terminal-output command=\"cargo test\" cwd=\"/w/repo\" status=\"exited with code 101\">\n[earlier output omitted]\nerror: test failed\n\nsee above\n</terminal-output>";

    #[test]
    fn plain_prompt_is_all_text() {
        let body = "just a question\n\nwith paragraphs";
        assert_eq!(split_context(body), plain(body));
    }

    #[test]
    fn references_and_terminal_before_the_text() {
        let body = format!(
            "Referenced:\n- file: /w/repo/src/lib.rs\n- directory: /w/repo/src/\n- thread \"Auth refactor\" (Trama thread 01J). Its latest reply:\n  > done\n  It is a separate Trama thread…\n- person: Alessandro Bruno (the user writing this prompt)\n\n{SNAP}\n\nwhy did it fail?"
        );
        let ctx = split_context(&body);
        assert_eq!(
            ctx.references,
            vec![
                Reference::File("/w/repo/src/lib.rs".into()),
                Reference::Directory("/w/repo/src/".into()),
                Reference::Thread("Auth refactor".into()),
                Reference::Person("Alessandro Bruno".into()),
            ]
        );
        assert_eq!(ctx.references[0].short_label(), "lib.rs");
        assert_eq!(ctx.references[1].short_label(), "src");
        assert_eq!(ctx.terminals.len(), 1);
        let term = &ctx.terminals[0];
        assert_eq!(term.command, "cargo test");
        assert_eq!(term.cwd, "/w/repo");
        assert_eq!(term.status, "exited with code 101");
        assert!(term.omitted);
        assert_eq!(term.output, "error: test failed\n\nsee above");
        assert!(status_failed(&term.status));
        assert_eq!(&body[ctx.text], "why did it fail?");
    }

    #[test]
    fn several_terminals_and_no_text() {
        let second = "<terminal-output command=\"interactive shell\" cwd=\"/w\" status=\"running\">\n$ ls\n</terminal-output>";
        let body = format!("{SNAP}\n\n{second}");
        let ctx = split_context(&body);
        assert_eq!(ctx.terminals.len(), 2);
        assert_eq!(ctx.terminals[1].status, "running");
        assert!(!status_failed("running"));
        assert!(!status_failed("exited with code 0"));
        assert_eq!(ctx.text, body.len()..body.len());
    }

    #[test]
    fn slash_command_keeps_context_after_the_text() {
        let body =
            format!("/review the parser\n\nplease\n\nReferenced:\n- file: /w/a.rs\n\n{SNAP}");
        let ctx = split_context(&body);
        assert_eq!(&body[ctx.text.clone()], "/review the parser\n\nplease");
        assert_eq!(ctx.references, vec![Reference::File("/w/a.rs".into())]);
        assert_eq!(ctx.terminals.len(), 1);
    }

    #[test]
    fn escaped_attributes_and_empty_output() {
        let body = "<terminal-output command=\"echo &quot;a &amp; b&quot; &lt;x&gt;\" cwd=\"/w\" status=\"killed by signal 9\">\n</terminal-output>\n\nhi";
        let ctx = split_context(body);
        assert_eq!(ctx.terminals[0].command, "echo \"a & b\" <x>");
        assert_eq!(ctx.terminals[0].output, "");
        assert!(status_failed(&ctx.terminals[0].status));
        assert_eq!(&body[ctx.text], "hi");
    }

    #[test]
    fn lookalike_text_is_not_context() {
        let body = "Referenced:\nnothing here";
        assert_eq!(split_context(body), plain(body));
        let body = "<terminal-output command=\"x\">\nunterminated";
        assert_eq!(split_context(body), plain(body));
        let body = "/cmd\n\nReferenced:\n- file: /a\n\nthen more text";
        assert_eq!(split_context(body), plain(body));
    }
}
