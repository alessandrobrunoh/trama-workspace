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
        SendToSubthread,
        AllowOnce,
        AllowSession,
        DenyApproval,
        CommentOnSelection,
        ShareThread,
        ArchiveThread,
    ]
);

pub fn bind(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-n", NewThread, Some(CONTEXT)),
        KeyBinding::new("secondary-r", ToggleDrawer, Some(CONTEXT)),
        KeyBinding::new("secondary-k", QuickSwitch, Some(CONTEXT)),
        KeyBinding::new("secondary-/", ShowShortcuts, Some(CONTEXT)),
        KeyBinding::new("secondary-,", ManageProjects, Some(CONTEXT)),
        KeyBinding::new("escape", CloseOverlay, Some(CONTEXT)),
        // Not bound by the composer textarea, so it reaches the workspace.
        KeyBinding::new("secondary-shift-enter", SendToSubthread, Some(CONTEXT)),
        KeyBinding::new("secondary-y", AllowOnce, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-y", AllowSession, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-n", DenyApproval, Some(CONTEXT)),
        KeyBinding::new("secondary-alt-m", CommentOnSelection, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-s", ShareThread, Some(CONTEXT)),
        // Delta's shortcut. Nothing else binds secondary-w (there are no tabs and the
        // window is closed with the traffic light or ⌘Q).
        KeyBinding::new("secondary-w", ArchiveThread, Some(CONTEXT)),
    ]);
}

/// One row of the shortcuts sheet. `keys` uses GPUI keystroke syntax;
/// `secondary` is ⌘ on macOS and Ctrl on Linux and Windows.
pub struct Shortcut {
    pub keys: &'static [&'static str],
    pub label: &'static str,
}

pub const SHORTCUTS: &[Shortcut] = &[
    Shortcut {
        keys: &["secondary-n"],
        label: "New thread",
    },
    Shortcut {
        keys: &["enter"],
        label: "Send message",
    },
    Shortcut {
        keys: &["secondary-enter"],
        label: "Send message (alternative)",
    },
    Shortcut {
        keys: &["shift-enter"],
        label: "New line in the composer",
    },
    Shortcut {
        keys: &["secondary-shift-enter"],
        label: "Send the draft to a new subthread",
    },
    Shortcut {
        keys: &["secondary-y"],
        label: "Allow the pending tool request once",
    },
    Shortcut {
        keys: &["secondary-shift-y"],
        label: "Allow the tool for this session",
    },
    Shortcut {
        keys: &["secondary-shift-n"],
        label: "Deny the pending tool request",
    },
    Shortcut {
        keys: &["secondary-alt-m"],
        label: "Comment on the selected transcript text",
    },
    Shortcut {
        keys: &["secondary-shift-s"],
        label: "Share this thread",
    },
    Shortcut {
        keys: &["secondary-w"],
        label: "Archive this thread (removes its checkouts)",
    },
    Shortcut {
        keys: &["ctrl-`"],
        label: "Show or hide the thread's terminals",
    },
    Shortcut {
        keys: &["secondary-k"],
        label: "Jump to a thread or run a command",
    },
    Shortcut {
        keys: &["secondary-r"],
        label: "Toggle the Changes and Copies drawer",
    },
    Shortcut {
        keys: &["secondary-,"],
        label: "Manage projects and repositories",
    },
    Shortcut {
        keys: &["secondary-/"],
        label: "Show keyboard shortcuts",
    },
    Shortcut {
        keys: &["escape"],
        label: "Close the open dialog or drawer",
    },
];

/// Human label for a GPUI keystroke string (`"secondary-shift-n"`), in the
/// current platform's convention: ⌘⌥⇧ glyphs on macOS, `Ctrl+Shift+N`
/// elsewhere. A bare modifier (`"secondary"`) formats as just that modifier.
pub fn label(keys: &str) -> String {
    format_keys(keys, cfg!(target_os = "macos"))
}

/// Space-separated `label`s, e.g. for a tooltip suffix.
pub fn format_keys(keys: &str, mac: bool) -> String {
    let parts: Vec<&str> = if keys == "-" {
        vec!["-"]
    } else if let Some(rest) = keys.strip_suffix("--") {
        rest.split('-').chain(std::iter::once("-")).collect()
    } else {
        keys.split('-').collect()
    };
    let (mut ctrl, mut alt, mut shift, mut cmd) = (false, false, false, false);
    let mut key = None;
    for part in &parts {
        let lower = part.to_ascii_lowercase();
        let modifier = match lower.as_str() {
            "secondary" if mac => Some(&mut cmd),
            "secondary" => Some(&mut ctrl),
            "cmd" | "super" | "win" | "platform" => Some(&mut cmd),
            "ctrl" | "control" => Some(&mut ctrl),
            "alt" | "option" => Some(&mut alt),
            "shift" => Some(&mut shift),
            _ => None,
        };
        match modifier {
            Some(flag) => *flag = true,
            None => key = Some(*part),
        }
    }
    let key_name = key.map(|key| key_name(key, mac));
    if mac {
        let mut out = String::new();
        if ctrl {
            out.push('\u{2303}');
        }
        if alt {
            out.push('\u{2325}');
        }
        if shift {
            out.push('\u{21e7}');
        }
        if cmd {
            out.push('\u{2318}');
        }
        out.push_str(key_name.as_deref().unwrap_or(""));
        out
    } else {
        let mut out: Vec<String> = Vec::new();
        if ctrl {
            out.push("Ctrl".into());
        }
        if alt {
            out.push("Alt".into());
        }
        if shift {
            out.push("Shift".into());
        }
        if cmd {
            out.push(
                if cfg!(target_os = "windows") {
                    "Win"
                } else {
                    "Super"
                }
                .into(),
            );
        }
        out.extend(key_name);
        out.join("+")
    }
}

fn key_name(key: &str, mac: bool) -> String {
    let named = match (key, mac) {
        ("enter", true) => "\u{23ce}",
        ("enter", false) => "Enter",
        ("escape", true) => "\u{238b}",
        ("escape", false) => "Esc",
        ("backspace", true) => "\u{232b}",
        ("backspace", false) => "Backspace",
        ("tab", _) => "Tab",
        ("space", _) => "Space",
        ("up", true) => "\u{2191}",
        ("down", true) => "\u{2193}",
        ("left", true) => "\u{2190}",
        ("right", true) => "\u{2192}",
        ("up", false) => "Up",
        ("down", false) => "Down",
        ("left", false) => "Left",
        ("right", false) => "Right",
        _ => return key.to_uppercase(),
    };
    named.to_string()
}

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
    #[test]
    fn formats_mac_and_pc_labels() {
        use super::format_keys;
        assert_eq!(format_keys("secondary-n", true), "\u{2318}N");
        assert_eq!(format_keys("secondary-n", false), "Ctrl+N");
        assert_eq!(format_keys("secondary-shift-s", true), "\u{21e7}\u{2318}S");
        assert_eq!(format_keys("secondary-shift-s", false), "Ctrl+Shift+S");
        assert_eq!(format_keys("secondary-alt-m", false), "Ctrl+Alt+M");
        assert_eq!(
            format_keys("secondary-shift-enter", false),
            "Ctrl+Shift+Enter"
        );
        assert_eq!(format_keys("secondary-,", false), "Ctrl+,");
        assert_eq!(format_keys("secondary-/", true), "\u{2318}/");
        assert_eq!(format_keys("ctrl-`", true), "\u{2303}`");
        assert_eq!(format_keys("ctrl-`", false), "Ctrl+`");
        assert_eq!(format_keys("secondary", false), "Ctrl");
        assert_eq!(format_keys("alt", true), "\u{2325}");
        assert_eq!(format_keys("escape", false), "Esc");
    }

    #[test]
    fn app_bindings_use_the_secondary_modifier() {
        for shortcut in super::SHORTCUTS {
            for keys in shortcut.keys {
                assert!(!keys.contains("cmd"), "{keys} would be Super on Linux");
            }
        }
    }

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
