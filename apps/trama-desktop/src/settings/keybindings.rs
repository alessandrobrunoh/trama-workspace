//! Rebindable actions: the table shown in Settings > Keybindings, conflict
//! detection, recording a keystroke, and applying overrides live.
//!
//! Keystrokes use GPUI syntax with `secondary` for the platform's primary
//! modifier (⌘ on macOS, Ctrl on Linux and Windows), so a stored binding
//! means the same thing on every OS. `keymap::bind` installs the defaults at
//! startup; [`apply`] layers the user's overrides on top: it masks every key
//! that was or is bound to a rebindable action with `NoAction`, then binds
//! the effective keys. Later bindings win in GPUI, so the result is exactly
//! the effective table, and applying again (after another edit) is safe.

use std::collections::BTreeMap;

use gpui_kit::{App, KeyBinding, Keystroke, NoAction};

use crate::app::keymap::{self, CONTEXT};

gpui_kit::actions!(trama, [OpenSettings]);

/// One rebindable action.
pub struct Binding {
    /// Stable id stored in `settings.json`.
    pub id: &'static str,
    pub label: &'static str,
    pub defaults: &'static [&'static str],
    bind: fn(&str) -> KeyBinding,
}

macro_rules! binding {
    ($id:literal, $label:literal, [$($key:literal),*], $action:expr) => {
        Binding {
            id: $id,
            label: $label,
            defaults: &[$($key),*],
            bind: |keys| KeyBinding::new(keys, $action, Some(CONTEXT)),
        }
    };
}

/// Every action that can be rebound, with the defaults `keymap::bind` and
/// `terminal::view::bind` install.
pub const BINDINGS: &[Binding] = &[
    binding!(
        "new_thread",
        "New thread",
        ["secondary-n"],
        keymap::NewThread
    ),
    binding!(
        "quick_switch",
        "Jump to a thread or run a command",
        ["secondary-k"],
        keymap::QuickSwitch
    ),
    binding!(
        "open_settings",
        "Open settings",
        ["secondary-alt-,"],
        OpenSettings
    ),
    binding!(
        "manage_projects",
        "Manage projects and repositories",
        ["secondary-,"],
        keymap::ManageProjects
    ),
    binding!(
        "toggle_drawer",
        "Toggle the Changes and Copies drawer",
        ["secondary-r"],
        keymap::ToggleDrawer
    ),
    binding!(
        "show_shortcuts",
        "Show keyboard shortcuts",
        ["secondary-/"],
        keymap::ShowShortcuts
    ),
    binding!(
        "send_to_subthread",
        "Send the draft to a new subthread",
        ["secondary-shift-enter"],
        keymap::SendToSubthread
    ),
    binding!(
        "allow_once",
        "Allow the pending tool request once",
        ["secondary-y"],
        keymap::AllowOnce
    ),
    binding!(
        "allow_session",
        "Allow the tool for this session",
        ["secondary-shift-y"],
        keymap::AllowSession
    ),
    binding!(
        "deny_approval",
        "Deny the pending tool request",
        ["secondary-shift-n"],
        keymap::DenyApproval
    ),
    binding!(
        "comment_on_selection",
        "Comment on the selected transcript text",
        ["secondary-alt-m"],
        keymap::CommentOnSelection
    ),
    binding!(
        "share_thread",
        "Share this thread",
        ["secondary-shift-s"],
        keymap::ShareThread
    ),
    binding!(
        "archive_thread",
        "Archive this thread",
        ["secondary-w"],
        keymap::ArchiveThread
    ),
    binding!(
        "toggle_terminal",
        "Show or hide the thread's terminals",
        ["ctrl-`"],
        crate::terminal::view::ToggleTerminal
    ),
];

pub fn find(id: &str) -> Option<&'static Binding> {
    BINDINGS.iter().find(|binding| binding.id == id)
}

/// Effective keys of `binding` under `overrides`.
pub fn effective<'a>(
    binding: &'a Binding,
    overrides: &'a BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    match overrides.get(binding.id) {
        Some(keys) => keys.clone(),
        None => binding.defaults.iter().map(|k| (*k).to_owned()).collect(),
    }
}

/// Canonical form for comparisons: `secondary` resolved for the platform,
/// modifiers in a fixed order, lowercase.
pub fn normalize(keys: &str, mac: bool) -> String {
    let lower = keys.trim().to_ascii_lowercase();
    let (mods, key) = match lower.strip_suffix("--") {
        Some(rest) => (rest, "-"),
        None => match lower.rsplit_once('-') {
            Some((mods, key)) => (mods, key),
            None => ("", lower.as_str()),
        },
    };
    let (mut ctrl, mut alt, mut shift, mut cmd, mut func) = (false, false, false, false, false);
    for part in mods.split('-').filter(|p| !p.is_empty()) {
        match part {
            "secondary" if mac => cmd = true,
            "secondary" => ctrl = true,
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" => alt = true,
            "shift" => shift = true,
            "cmd" | "super" | "win" | "platform" => cmd = true,
            "fn" => func = true,
            _ => {}
        }
    }
    let mut out = String::new();
    for (on, name) in [
        (ctrl, "ctrl"),
        (alt, "alt"),
        (shift, "shift"),
        (cmd, "cmd"),
        (func, "fn"),
    ] {
        if on {
            out.push_str(name);
            out.push('-');
        }
    }
    out.push_str(key);
    out
}

/// The other action already using `keys` under `overrides`, if any.
pub fn conflict(
    id: &str,
    keys: &str,
    overrides: &BTreeMap<String, Vec<String>>,
    mac: bool,
) -> Option<&'static Binding> {
    let wanted = normalize(keys, mac);
    BINDINGS.iter().find(|other| {
        other.id != id
            && effective(other, overrides)
                .iter()
                .any(|k| normalize(k, mac) == wanted)
    })
}

/// Every pair of actions sharing a key (shown as warnings).
pub fn all_conflicts(
    overrides: &BTreeMap<String, Vec<String>>,
    mac: bool,
) -> Vec<(&'static str, &'static str, String)> {
    let mut seen: BTreeMap<String, &'static str> = BTreeMap::new();
    let mut out = Vec::new();
    for binding in BINDINGS {
        for key in effective(binding, overrides) {
            let norm = normalize(&key, mac);
            match seen.get(&norm) {
                Some(first) => out.push((*first, binding.id, key)),
                None => {
                    seen.insert(norm, binding.id);
                }
            }
        }
    }
    out
}

/// GPUI syntax for a recorded keystroke, with the platform modifier written
/// as `secondary`. `None` for a lone modifier or an unusable key.
pub fn from_keystroke(keystroke: &Keystroke, mac: bool) -> Option<String> {
    let key = keystroke.key.to_ascii_lowercase();
    if key.is_empty()
        || matches!(
            key.as_str(),
            "shift" | "control" | "ctrl" | "alt" | "option" | "cmd" | "super" | "platform" | "fn"
        )
    {
        return None;
    }
    let m = &keystroke.modifiers;
    let (secondary, ctrl, cmd) = if mac {
        (m.platform, m.control, false)
    } else {
        (m.control, false, m.platform)
    };
    let mut parts: Vec<&str> = Vec::new();
    if secondary {
        parts.push("secondary");
    }
    if ctrl {
        parts.push("ctrl");
    }
    if m.alt {
        parts.push("alt");
    }
    if m.shift {
        parts.push("shift");
    }
    if cmd {
        parts.push("cmd");
    }
    if m.function {
        parts.push("fn");
    }
    let mut out = parts.join("-");
    if !out.is_empty() {
        out.push('-');
    }
    out.push_str(&key);
    Some(out)
}

/// Parses as GPUI keystrokes (a hand-edited file may hold anything).
pub fn valid(keys: &str) -> bool {
    !keys.trim().is_empty() && keys.split_whitespace().all(|k| Keystroke::parse(k).is_ok())
}

/// Install the effective bindings. `previous` are the overrides applied
/// before (their keys are masked too).
pub fn apply(
    previous: &BTreeMap<String, Vec<String>>,
    overrides: &BTreeMap<String, Vec<String>>,
    cx: &mut App,
) {
    let mut masked: Vec<String> = Vec::new();
    for binding in BINDINGS {
        for key in binding
            .defaults
            .iter()
            .map(|k| (*k).to_owned())
            .chain(previous.get(binding.id).cloned().unwrap_or_default())
        {
            if !masked.contains(&key) {
                masked.push(key);
            }
        }
    }
    let mut bindings: Vec<KeyBinding> = masked
        .iter()
        .filter(|key| valid(key))
        .map(|key| KeyBinding::new(key, NoAction, Some(CONTEXT)))
        .collect();
    for binding in BINDINGS {
        for key in effective(binding, overrides) {
            if valid(&key) {
                bindings.push((binding.bind)(&key));
            }
        }
    }
    cx.bind_keys(bindings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use gpui_kit::Modifiers;

    #[test]
    fn ids_are_unique_and_defaults_do_not_conflict() {
        let mut ids: Vec<_> = BINDINGS.iter().map(|b| b.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), BINDINGS.len());
        for mac in [true, false] {
            assert!(all_conflicts(&BTreeMap::new(), mac).is_empty());
        }
    }

    #[test]
    fn normalize_resolves_secondary_per_platform() {
        assert_eq!(normalize("secondary-shift-n", true), "shift-cmd-n");
        assert_eq!(normalize("secondary-shift-n", false), "ctrl-shift-n");
        assert_eq!(normalize("shift-ctrl-N", false), "ctrl-shift-n");
        assert_eq!(normalize("secondary--", false), "ctrl--");
    }

    #[test]
    fn detects_conflicts_with_overrides() {
        let mut overrides = BTreeMap::new();
        assert_eq!(
            conflict("new_thread", "secondary-k", &overrides, false).map(|b| b.id),
            Some("quick_switch")
        );
        assert!(conflict("new_thread", "ctrl-k", &overrides, true).is_none());
        overrides.insert("quick_switch".to_owned(), vec!["secondary-p".to_owned()]);
        assert!(conflict("new_thread", "secondary-k", &overrides, false).is_none());
        overrides.insert("new_thread".to_owned(), vec!["secondary-p".to_owned()]);
        assert_eq!(all_conflicts(&overrides, false).len(), 1);
    }

    #[test]
    fn records_keystrokes_with_platform_modifier() {
        let stroke = |key: &str, control: bool, platform: bool, shift: bool| Keystroke {
            modifiers: Modifiers {
                control,
                platform,
                shift,
                ..Default::default()
            },
            key: key.into(),
            key_char: None,
        };
        assert_eq!(
            from_keystroke(&stroke("n", false, true, true), true).as_deref(),
            Some("secondary-shift-n")
        );
        assert_eq!(
            from_keystroke(&stroke("n", true, false, true), false).as_deref(),
            Some("secondary-shift-n")
        );
        assert_eq!(
            from_keystroke(&stroke("k", true, false, false), true).as_deref(),
            Some("ctrl-k")
        );
        assert_eq!(
            from_keystroke(&stroke("shift", false, false, true), false),
            None
        );
    }
}
