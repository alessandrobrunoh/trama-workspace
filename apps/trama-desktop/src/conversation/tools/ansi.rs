// -- commands -------------------------------------------------------------

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    Running,
    Success,
    Code(i32),
    Failed,
    Interrupted,
}

/// Status and remaining text of a Bash result. Claude Code reports a
/// non-zero exit as an error whose text starts with "Exit code N".
pub fn command_result(finished: bool, is_error: bool, text: &str) -> (ExitStatus, String) {
    if !finished {
        return (ExitStatus::Running, text.to_string());
    }
    let trimmed = text.trim_start();
    if trimmed.starts_with("[Request interrupted") {
        return (ExitStatus::Interrupted, String::new());
    }
    let (first, rest) = trimmed.split_once('\n').unwrap_or((trimmed, ""));
    let code = first
        .trim()
        .trim_start_matches("Error: ")
        .strip_prefix("Exit code ")
        .and_then(|code| code.trim().parse::<i32>().ok());
    match code {
        Some(0) => (ExitStatus::Success, rest.to_string()),
        Some(code) => (ExitStatus::Code(code), rest.to_string()),
        None if is_error => (ExitStatus::Failed, text.to_string()),
        None => (ExitStatus::Success, text.to_string()),
    }
}

/// A terminal color from an SGR sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnsiColor {
    /// 0–7 normal, 8–15 bright.
    Basic(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnsiStyle {
    pub fg: Option<AnsiColor>,
    pub bold: bool,
    pub dim: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiSpan {
    pub text: String,
    pub style: AnsiStyle,
}

pub type AnsiLine = Vec<AnsiSpan>;

/// Split terminal output into styled lines. SGR colors and weights are
/// kept; other escapes (cursor moves, erase, OSC titles and links) are
/// dropped; a carriage return keeps the text written after it.
pub fn parse_ansi(text: &str) -> Vec<AnsiLine> {
    let mut style = AnsiStyle::default();
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let raw = raw.rsplit('\r').next().unwrap_or(raw);
        let mut line: AnsiLine = Vec::new();
        let mut current = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let mut fin = None;
                        for next in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&next) {
                                fin = Some(next);
                                break;
                            }
                            params.push(next);
                        }
                        if fin == Some('m') {
                            let next_style = apply_sgr(style, &params);
                            if next_style != style && !current.is_empty() {
                                line.push(AnsiSpan {
                                    text: std::mem::take(&mut current),
                                    style,
                                });
                            }
                            style = next_style;
                        }
                    }
                    Some(']') => {
                        // OSC: up to BEL or ESC \.
                        while let Some(next) = chars.next() {
                            if next == '\u{7}' {
                                break;
                            }
                            if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                                chars.next();
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                '\t' => current.push_str("    "),
                c if c.is_control() => {}
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            line.push(AnsiSpan {
                text: current,
                style,
            });
        }
        lines.push(line);
    }
    while lines
        .last()
        .is_some_and(|line| line_text(line).trim().is_empty())
    {
        lines.pop();
    }
    lines
}

pub fn line_text(line: &AnsiLine) -> String {
    line.iter().map(|span| span.text.as_str()).collect()
}

pub(super) fn apply_sgr(mut style: AnsiStyle, params: &str) -> AnsiStyle {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|code| code.parse().unwrap_or(0))
            .collect()
    };
    let mut index = 0;
    while index < codes.len() {
        match codes[index] {
            0 => style = AnsiStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            code @ 30..=37 => style.fg = Some(AnsiColor::Basic((code - 30) as u8)),
            code @ 90..=97 => style.fg = Some(AnsiColor::Basic((code - 90 + 8) as u8)),
            39 => style.fg = None,
            kind @ (38 | 48) => {
                let color = match codes.get(index + 1).copied() {
                    Some(5) => {
                        let n = codes.get(index + 2).copied().unwrap_or(0).min(255) as u8;
                        index += 2;
                        Some(color_256(n))
                    }
                    Some(2) => {
                        let channel =
                            |k: usize| codes.get(index + k).copied().unwrap_or(0).min(255) as u8;
                        let rgb = AnsiColor::Rgb(channel(2), channel(3), channel(4));
                        index += 4;
                        Some(rgb)
                    }
                    _ => None,
                };
                if kind == 38 {
                    style.fg = color.or(style.fg);
                }
            }
            _ => {}
        }
        index += 1;
    }
    style
}

pub(super) fn color_256(n: u8) -> AnsiColor {
    match n {
        0..=15 => AnsiColor::Basic(n),
        16..=231 => {
            let n = n - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            AnsiColor::Rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let gray = 8 + (n - 232) * 10;
            AnsiColor::Rgb(gray, gray, gray)
        }
    }
}

/// Lines shown by default: the last `limit`. Returns the first visible line
/// and how many are hidden above it.
pub fn visible_tail(total: usize, limit: usize, expanded: bool) -> (usize, usize) {
    if expanded || total <= limit {
        (0, 0)
    } else {
        (total - limit, total - limit)
    }
}

pub const OUTPUT_TAIL: usize = 12;
