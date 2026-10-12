//! Window actions, their key bindings, and the shortcuts help list.

use gpui_kit::{App, KeyBinding};

/// Key context set on the workspace root. Bindings only fire inside it, so
/// open dialogs (rendered outside the workspace) keep their own keys.
pub const CONTEXT: &str = "TramaWorkspace";

gpui_kit::actions!(
    trama,
    [
        NewThread,
        ToggleDrawer,
        QuickSwitch,
        ShowShortcuts,
        ManageProjects,
        CloseOverlay,
        ToggleAppearance,
    ]
);

pub fn bind(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-n", NewThread, Some(CONTEXT)),
        KeyBinding::new("cmd-r", ToggleDrawer, Some(CONTEXT)),
        KeyBinding::new("cmd-k", QuickSwitch, Some(CONTEXT)),
        KeyBinding::new("cmd-/", ShowShortcuts, Some(CONTEXT)),
        KeyBinding::new("cmd-,", ManageProjects, Some(CONTEXT)),
        KeyBinding::new("escape", CloseOverlay, Some(CONTEXT)),
    ]);
}

/// One row of the shortcuts sheet. `keys` uses GPUI keystroke syntax.
pub struct Shortcut {
    pub keys: &'static [&'static str],
    pub label: &'static str,
}

pub const SHORTCUTS: &[Shortcut] = &[
    Shortcut {
        keys: &["cmd-n"],
        label: "New thread",
    },
    Shortcut {
        keys: &["enter"],
        label: "Send message",
    },
    Shortcut {
        keys: &["cmd-enter"],
        label: "Send message (alternative)",
    },
    Shortcut {
        keys: &["shift-enter"],
        label: "New line in the composer",
    },
    Shortcut {
        keys: &["cmd-k"],
        label: "Jump to a thread or run a command",
    },
    Shortcut {
        keys: &["cmd-r"],
        label: "Toggle the Changes drawer",
    },
    Shortcut {
        keys: &["cmd-,"],
        label: "Manage projects and repositories",
    },
    Shortcut {
        keys: &["cmd-/"],
        label: "Show keyboard shortcuts",
    },
    Shortcut {
        keys: &["escape"],
        label: "Close the open dialog or drawer",
    },
];

/// Subsequence fuzzy match. Higher is better; `None` when `query` is not a
/// subsequence of `candidate` (case-insensitive). Rewards matches at the
/// start, after separators, and in consecutive runs.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if query.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();

    let mut score = 0i32;
    let mut qi = 0;
    let mut last_match: Option<usize> = None;
    for (ci, &c) in lower.iter().enumerate() {
        if qi == query.len() {
            break;
        }
        if c != query[qi] {
            continue;
        }
        let mut bonus = 1;
        if ci == 0 {
            bonus += 8;
        } else {
            let prev = chars[ci - 1];
            if !prev.is_alphanumeric() || (prev.is_lowercase() && chars[ci].is_uppercase()) {
                bonus += 6;
            }
        }
        if let Some(last) = last_match {
            if last + 1 == ci {
                bonus += 5;
            } else {
                bonus -= ((ci - last - 1).min(4)) as i32;
            }
        }
        score += bonus;
        last_match = Some(ci);
        qi += 1;
    }
    if qi < query.len() {
        return None;
    }
    // Prefer shorter candidates on ties.
    Some(score * 4 - chars.len().min(64) as i32 / 8)
}

/// Indices of `candidates` that match `query`, best first. Stable for ties.
pub fn fuzzy_rank<'a>(query: &str, candidates: impl IntoIterator<Item = &'a str>) -> Vec<usize> {
    let mut scored: Vec<(usize, i32)> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(index, candidate)| fuzzy_score(query, candidate).map(|s| (index, s)))
        .collect();
    if !query.trim().is_empty() {
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    }
    scored.into_iter().map(|(index, _)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_matches_and_misses() {
        assert!(fuzzy_score("fa", "Fix auth").is_some());
        assert!(fuzzy_score("fxa", "Fix auth").is_some());
        assert!(fuzzy_score("FIX AUTH", "fix auth").is_some());
        assert!(fuzzy_score("zz", "Fix auth").is_none());
        assert!(fuzzy_score("htua", "auth").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn word_starts_and_runs_rank_above_scattered_matches() {
        let candidates = ["Refactor parser", "Fix auth", "Add flaky test retries"];
        let ranked = fuzzy_rank("fa", candidates);
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0], 1, "word-start match wins");
        let ranked = fuzzy_rank("auth", ["Migrate oauth tokens", "Auth login flow"]);
        assert_eq!(ranked, vec![1, 0]);
    }

    #[test]
    fn empty_query_keeps_original_order() {
        assert_eq!(fuzzy_rank(" ", ["b", "a", "c"]), vec![0, 1, 2]);
    }

    #[test]
    fn every_shortcut_parses_as_a_keystroke() {
        for shortcut in SHORTCUTS {
            for keys in shortcut.keys {
                assert!(
                    gpui_kit::Keystroke::parse(keys).is_ok(),
                    "{keys} should parse"
                );
            }
        }
    }
}
