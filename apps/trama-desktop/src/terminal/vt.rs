//! VT emulation for inline terminals: `alacritty_terminal` (the emulator Zed
//! uses) parses the PTY bytes into a grid; this module turns the visible
//! grid into styled runs the view paints with the mono font.

use std::sync::{Arc, Mutex};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, StdSyncHandler};
use base64::Engine as _;

/// Scrollback kept by the emulator (the daemon keeps its own 256 KiB).
const SCROLLBACK_LINES: usize = 5_000;

/// Decode one `TerminalOutput.data` chunk (standard base64, padded).
pub fn decode_output(data: &str) -> Result<Vec<u8>, String> {
    let compact: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(compact.as_bytes())
        .map_err(|err| format!("terminal output is not valid base64: {err}"))
}

/// A cell color after resolving the emulator's palette references.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermColor {
    /// The terminal's default foreground.
    Fg,
    /// The terminal's default background.
    Bg,
    /// One of the 16 ANSI colors (8..16 are the bright variants).
    Ansi(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: TermColor,
    pub bg: TermColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            fg: TermColor::Fg,
            bg: TermColor::Bg,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            strikeout: false,
        }
    }
}

/// Consecutive cells of one row with the same style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub style: CellStyle,
}

/// Collects what the emulator wants written back to the PTY (cursor
/// position and device attribute reports).
#[derive(Clone, Default)]
struct Replies(Arc<Mutex<Vec<String>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event
            && let Ok(mut replies) = self.0.lock()
        {
            replies.push(text);
        }
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// One terminal's emulated screen.
pub struct VtScreen {
    term: Term<Replies>,
    parser: Processor<StdSyncHandler>,
    replies: Replies,
}

impl VtScreen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let replies = Replies::default();
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let size = Size {
            cols: cols.max(2) as usize,
            rows: rows.max(1) as usize,
        };
        Self {
            term: Term::new(config, &size, replies.clone()),
            parser: Processor::new(),
            replies,
        }
    }

    pub fn cols(&self) -> u16 {
        self.term.columns() as u16
    }

    pub fn rows(&self) -> u16 {
        self.term.screen_lines() as u16
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Start over (a replay carries the whole retained scrollback).
    pub fn reset(&mut self) {
        let (cols, rows) = (self.cols(), self.rows());
        *self = Self::new(cols, rows);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Size {
            cols: cols.max(2) as usize,
            rows: rows.max(1) as usize,
        });
    }

    /// Scroll the view into history (`lines > 0`) or back down.
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Arrow keys send `ESC O A` instead of `ESC [ A` (DECCKM).
    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Replies the emulator produced since the last call (to send as input).
    pub fn take_replies(&mut self) -> Vec<String> {
        self.replies
            .0
            .lock()
            .map(|mut replies| std::mem::take(&mut *replies))
            .unwrap_or_default()
    }

    /// Caret cell `(row, col)` in the visible screen, when shown.
    pub fn cursor(&self) -> Option<(usize, usize)> {
        let content = self.term.renderable_content();
        if content.cursor.shape == CursorShape::Hidden {
            return None;
        }
        let row = content.cursor.point.line.0 + content.display_offset as i32;
        (row >= 0 && (row as usize) < self.term.screen_lines())
            .then(|| (row as usize, content.cursor.point.column.0))
    }

    /// The visible screen as styled runs per row. Trailing default blanks
    /// are dropped; wide-character spacers are skipped.
    pub fn styled_rows(&self) -> Vec<Vec<Run>> {
        let grid = self.term.grid();
        let offset = grid.display_offset() as i32;
        let cols = grid.columns();
        (0..grid.screen_lines())
            .map(|row| {
                let line = &grid[Line(row as i32 - offset)];
                let mut runs: Vec<Run> = Vec::new();
                for col in 0..cols {
                    let cell = &line[Column(col)];
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    let style = cell_style(cell.fg, cell.bg, cell.flags);
                    let c = if cell.flags.contains(Flags::HIDDEN) || cell.c == '\t' {
                        ' '
                    } else {
                        cell.c
                    };
                    match runs.last_mut() {
                        Some(run) if run.style == style => run.text.push(c),
                        _ => runs.push(Run {
                            text: c.to_string(),
                            style,
                        }),
                    }
                    for extra in cell.zerowidth().into_iter().flatten() {
                        if let Some(run) = runs.last_mut() {
                            run.text.push(*extra);
                        }
                    }
                }
                trim_trailing_blanks(&mut runs);
                runs
            })
            .collect()
    }

    /// Plain text of the visible screen, one line per row, trailing blanks
    /// trimmed.
    #[cfg(test)]
    pub fn screen_text(&self) -> String {
        self.styled_rows()
            .iter()
            .map(|runs| runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn trim_trailing_blanks(runs: &mut Vec<Run>) {
    while let Some(run) = runs.last_mut() {
        // A colored background is visible even where there is no text.
        if run.style.bg != TermColor::Bg {
            break;
        }
        let trimmed = run.text.trim_end_matches(' ').len();
        if trimmed == 0 {
            runs.pop();
        } else {
            run.text.truncate(trimmed);
            break;
        }
    }
}

fn cell_style(fg: Color, bg: Color, flags: Flags) -> CellStyle {
    let (mut fg, mut bg) = (resolve(fg, TermColor::Fg), resolve(bg, TermColor::Bg));
    let bold = flags.contains(Flags::BOLD);
    // Bold text in a basic color shows in its bright variant, like xterm.
    if bold && let TermColor::Ansi(n @ 0..=7) = fg {
        fg = TermColor::Ansi(n + 8);
    }
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    CellStyle {
        fg,
        bg,
        bold,
        dim: flags.contains(Flags::DIM),
        italic: flags.contains(Flags::ITALIC),
        underline: flags.intersects(Flags::ALL_UNDERLINES),
        strikeout: flags.contains(Flags::STRIKEOUT),
    }
}

fn resolve(color: Color, default: TermColor) -> TermColor {
    match color {
        Color::Spec(rgb) => TermColor::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(n) => indexed(n),
        Color::Named(named) => named_color(named, default),
    }
}

fn named_color(named: NamedColor, default: TermColor) -> TermColor {
    let n = named as usize;
    if n < 16 {
        return TermColor::Ansi(n as u8);
    }
    match named {
        NamedColor::DimBlack => TermColor::Ansi(0),
        NamedColor::DimRed => TermColor::Ansi(1),
        NamedColor::DimGreen => TermColor::Ansi(2),
        NamedColor::DimYellow => TermColor::Ansi(3),
        NamedColor::DimBlue => TermColor::Ansi(4),
        NamedColor::DimMagenta => TermColor::Ansi(5),
        NamedColor::DimCyan => TermColor::Ansi(6),
        NamedColor::DimWhite => TermColor::Ansi(7),
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground => {
            TermColor::Fg
        }
        NamedColor::Background => TermColor::Bg,
        _ => default,
    }
}

/// xterm's 256-color palette beyond the first 16.
fn indexed(n: u8) -> TermColor {
    match n {
        0..=15 => TermColor::Ansi(n),
        16..=231 => {
            let n = n - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            TermColor::Rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let gray = 8 + (n - 232) * 10;
            TermColor::Rgb(gray, gray, gray)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_text(screen: &VtScreen) -> Vec<String> {
        screen
            .styled_rows()
            .iter()
            .map(|runs| runs.iter().map(|r| r.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn plain_text_with_crlf_lands_on_separate_rows() {
        let mut screen = VtScreen::new(20, 4);
        screen.feed(b"hello\r\nworld");
        assert_eq!(rows_text(&screen), vec!["hello", "world", "", ""]);
        assert_eq!(screen.cursor(), Some((1, 5)));
    }

    #[test]
    fn carriage_return_overwrites_the_line() {
        let mut screen = VtScreen::new(20, 2);
        screen.feed(b"progress 10%\rprogress 99%\r\ndone");
        assert_eq!(rows_text(&screen), vec!["progress 99%", "done"]);
    }

    #[test]
    fn bare_line_feed_keeps_the_column() {
        let mut screen = VtScreen::new(10, 3);
        screen.feed(b"ab\ncd");
        assert_eq!(rows_text(&screen), vec!["ab", "  cd", ""]);
    }

    #[test]
    fn sgr_colors_and_attributes_become_styled_runs() {
        let mut screen = VtScreen::new(30, 2);
        screen.feed(
            b"ok \x1b[31mred\x1b[0m \x1b[1;32mbold\x1b[0m \x1b[38;5;196mx\x1b[48;2;1;2;3my\x1b[0m",
        );
        let row = &screen.styled_rows()[0];
        let find = |text: &str| row.iter().find(|r| r.text == text).unwrap().style;
        assert_eq!(find("ok ").fg, TermColor::Fg);
        assert_eq!(find("ok ").bg, TermColor::Bg);
        assert_eq!(find("red").fg, TermColor::Ansi(1));
        let bold = find("bold");
        assert!(bold.bold);
        assert_eq!(bold.fg, TermColor::Ansi(10), "bold basic color brightens");
        assert_eq!(find("x").fg, TermColor::Rgb(255, 0, 0));
        assert_eq!(find("y").bg, TermColor::Rgb(1, 2, 3));
    }

    #[test]
    fn inverse_swaps_colors() {
        let mut screen = VtScreen::new(10, 1);
        screen.feed(b"\x1b[7;34mz");
        let style = screen.styled_rows()[0][0].style;
        assert_eq!(style.bg, TermColor::Ansi(4));
        assert_eq!(
            style.fg,
            TermColor::Bg,
            "inverse defaults paint as reversed"
        );
    }

    #[test]
    fn clear_screen_and_cursor_home() {
        let mut screen = VtScreen::new(10, 3);
        screen.feed(b"one\r\ntwo\r\nthree");
        screen.feed(b"\x1b[2J\x1b[H");
        assert_eq!(rows_text(&screen), vec!["", "", ""]);
        assert_eq!(screen.cursor(), Some((0, 0)));
        screen.feed(b"x\x1b[2;3Hy");
        assert_eq!(rows_text(&screen), vec!["x", "  y", ""]);
    }

    #[test]
    fn erase_line_and_backspace() {
        let mut screen = VtScreen::new(10, 1);
        screen.feed(b"abcdef\x1b[3D\x1b[K");
        assert_eq!(rows_text(&screen), vec!["abc"]);
        screen.feed(b"\x08\x08Z");
        assert_eq!(rows_text(&screen), vec!["aZc"]);
    }

    #[test]
    fn utf8_split_across_chunks_is_reassembled() {
        let mut screen = VtScreen::new(10, 1);
        let bytes = "héllo".as_bytes();
        screen.feed(&bytes[..2]);
        screen.feed(&bytes[2..]);
        assert_eq!(rows_text(&screen), vec!["héllo"]);
    }

    #[test]
    fn scrolling_output_keeps_history_and_resize_reflows() {
        let mut screen = VtScreen::new(10, 2);
        screen.feed(b"1\r\n2\r\n3\r\n4");
        assert_eq!(rows_text(&screen), vec!["3", "4"]);
        screen.scroll(2);
        assert_eq!(rows_text(&screen), vec!["1", "2"]);
        screen.scroll_to_bottom();
        screen.resize(12, 3);
        assert_eq!((screen.cols(), screen.rows()), (12, 3));
        assert_eq!(rows_text(&screen).last().unwrap(), "4");
    }

    #[test]
    fn modes_and_replies_are_reported() {
        let mut screen = VtScreen::new(10, 2);
        assert!(!screen.app_cursor());
        screen.feed(b"\x1b[?1h\x1b[?2004h");
        assert!(screen.app_cursor());
        assert!(screen.bracketed_paste());
        screen.feed(b"ab\x1b[6n");
        assert_eq!(screen.take_replies(), vec!["\x1b[1;3R".to_string()]);
        assert!(screen.take_replies().is_empty());
        screen.feed(b"\x1b[?25l");
        assert_eq!(screen.cursor(), None);
    }

    #[test]
    fn reset_clears_content_but_keeps_size() {
        let mut screen = VtScreen::new(15, 4);
        screen.feed(b"stale");
        screen.reset();
        assert_eq!(screen.screen_text(), "\n\n\n");
        assert_eq!((screen.cols(), screen.rows()), (15, 4));
    }

    #[test]
    fn base64_chunks_decode_and_garbage_is_an_error() {
        assert_eq!(decode_output("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_output("aGVs\nbG8=").unwrap(), b"hello");
        assert_eq!(decode_output("").unwrap(), b"");
        assert_eq!(decode_output("G1szMW0=").unwrap(), b"\x1b[31m");
        assert!(decode_output("not base64!").is_err());
    }
}
