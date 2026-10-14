//! Caret triggers in the composer: `@` mentions, `/` commands at the start
//! of the prompt, and `!` to run a shell command in an inline terminal.
//!
//! Pure functions over the text and the caret's UTF-8 byte offset, so the
//! rules are testable without a window.

use std::ops::Range;

/// Longest `@` query that still counts as one being typed.
const MAX_MENTION_QUERY: usize = 120;

/// What the text around the caret is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// `@query`: `start` is the byte offset of the `@`.
    Mention { start: usize, query: String },
    /// `/query` at the start of the prompt: `start` is the offset of the `/`.
    Slash { start: usize, query: String },
    /// `!command` as the whole prompt: run it in a terminal.
    Bang { command: String },
}

impl Trigger {
    /// Byte range the popup replaces when an item is picked (`start..caret`).
    pub fn replace_range(&self, caret: usize) -> Option<Range<usize>> {
        match self {
            Trigger::Mention { start, .. } | Trigger::Slash { start, .. } => Some(*start..caret),
            Trigger::Bang { .. } => None,
        }
    }

    pub fn start(&self) -> usize {
        match self {
            Trigger::Mention { start, .. } | Trigger::Slash { start, .. } => *start,
            Trigger::Bang { .. } => 0,
        }
    }
}

/// The trigger at `caret`, if any. `tokens` are the byte ranges of mention
/// chips already in the text; an `@` inside one never starts a new mention.
pub fn detect(text: &str, caret: usize, tokens: &[Range<usize>]) -> Option<Trigger> {
    let caret = floor_char_boundary(text, caret.min(text.len()));
    if let Some(command) = bang_command(text) {
        return Some(Trigger::Bang { command });
    }
    if let Some(trigger) = slash(text, caret) {
        return Some(trigger);
    }
    mention(text, caret, tokens)
}

/// `!command` when the prompt starts with `!`: the command (possibly empty,
/// meaning an interactive shell). `"Hello!"` and `" !x"` do not count.
pub fn bang_command(text: &str) -> Option<String> {
    let rest = text.strip_prefix('!')?;
    if rest.contains('\n') {
        return None;
    }
    Some(rest.trim().to_string())
}

fn slash(text: &str, caret: usize) -> Option<Trigger> {
    let lead = text.len() - text.trim_start().len();
    if !text[lead..].starts_with('/') || caret <= lead {
        return None;
    }
    let query = &text[lead + 1..caret];
    if !query.chars().all(is_command_char) {
        return None;
    }
    // The caret must still be inside the command word.
    let after = &text[caret..];
    if after.chars().next().is_some_and(is_command_char) {
        return None;
    }
    Some(Trigger::Slash {
        start: lead,
        query: query.to_string(),
    })
}

/// Characters of a skill or command name (`ns:name`, `my-skill`, `v1.2`).
pub fn is_command_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')
}

fn mention(text: &str, caret: usize, tokens: &[Range<usize>]) -> Option<Trigger> {
    let before = &text[..caret];
    let at = before.rfind('@')?;
    if tokens
        .iter()
        .any(|range| range.start <= at && at < range.end)
    {
        return None;
    }
    let query = &before[at + 1..];
    if query.len() > MAX_MENTION_QUERY || query.chars().any(char::is_whitespace) {
        return None;
    }
    // `user@example.com` and `path/@scope` are not mentions.
    if let Some(prev) = before[..at].chars().next_back()
        && !(prev.is_whitespace() || matches!(prev, '(' | '[' | '{' | '"' | '\'' | '`'))
    {
        return None;
    }
    Some(Trigger::Mention {
        start: at,
        query: query.to_string(),
    })
}

fn floor_char_boundary(text: &str, mut ix: usize) -> usize {
    while ix > 0 && !text.is_char_boundary(ix) {
        ix -= 1;
    }
    ix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_end(text: &str) -> Option<Trigger> {
        detect(text, text.len(), &[])
    }

    #[test]
    fn at_sign_after_whitespace_or_start_opens_mentions() {
        assert_eq!(
            at_end("@"),
            Some(Trigger::Mention {
                start: 0,
                query: String::new()
            })
        );
        assert_eq!(
            at_end("look at @src/li"),
            Some(Trigger::Mention {
                start: 8,
                query: "src/li".into()
            })
        );
        assert_eq!(
            at_end("see (@lib"),
            Some(Trigger::Mention {
                start: 5,
                query: "lib".into()
            })
        );
        // Caret in the middle of the word: the query stops at the caret.
        assert_eq!(
            detect("ping @alessandro now", 9, &[]),
            Some(Trigger::Mention {
                start: 5,
                query: "ale".into()
            })
        );
    }

    #[test]
    fn emails_paths_and_finished_words_do_not_trigger() {
        assert_eq!(at_end("mail me at someone@example.com"), None);
        assert_eq!(at_end("node_modules/@types"), None);
        assert_eq!(at_end("@src/lib.rs and more"), None);
        assert_eq!(at_end("trailing @ "), None);
    }

    #[test]
    fn at_sign_inside_a_mention_chip_is_ignored() {
        let text = "@src/lib.rs";
        assert_eq!(detect(text, text.len(), &[0..text.len()]), None);
        // A new `@` after the chip still works.
        let text = "@src/lib.rs @Ca";
        assert_eq!(
            detect(text, text.len(), &[0..11]),
            Some(Trigger::Mention {
                start: 12,
                query: "Ca".into()
            })
        );
    }

    #[test]
    fn slash_only_at_the_start_of_the_prompt() {
        assert_eq!(
            at_end("/"),
            Some(Trigger::Slash {
                start: 0,
                query: String::new()
            })
        );
        assert_eq!(
            at_end("  /rev"),
            Some(Trigger::Slash {
                start: 2,
                query: "rev".into()
            })
        );
        assert_eq!(
            at_end("/ns:my-skill"),
            Some(Trigger::Slash {
                start: 0,
                query: "ns:my-skill".into()
            })
        );
        assert_eq!(at_end("/review the parser"), None, "arguments typed");
        assert_eq!(at_end("use src/lib.rs"), None, "a path mid-sentence");
        assert_eq!(at_end("/usr/bin"), None, "an absolute path");
        assert_eq!(at_end("hello\n/cmd"), None, "not the first line");
        assert_eq!(detect("/review", 3, &[]), None, "caret inside the word");
    }

    #[test]
    fn bang_only_as_the_first_character() {
        assert_eq!(
            at_end("!"),
            Some(Trigger::Bang {
                command: String::new()
            })
        );
        assert_eq!(
            at_end("!cargo test -p x"),
            Some(Trigger::Bang {
                command: "cargo test -p x".into()
            })
        );
        assert_eq!(at_end("Hello!"), None);
        assert_eq!(at_end(" !ls"), None);
        assert_eq!(at_end("!ls\nmore"), None);
    }

    #[test]
    fn caret_is_clamped_to_a_char_boundary() {
        let text = "é @x";
        assert!(detect(text, 1, &[]).is_none());
        assert_eq!(
            detect(text, 99, &[]),
            Some(Trigger::Mention {
                start: 3,
                query: "x".into()
            })
        );
    }

    #[test]
    fn replace_range_spans_trigger_to_caret() {
        let trigger = at_end("hi @ab").unwrap();
        assert_eq!(trigger.replace_range(6), Some(3..6));
        assert_eq!(
            Trigger::Bang {
                command: String::new()
            }
            .replace_range(1),
            None
        );
    }
}
