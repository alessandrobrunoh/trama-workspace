//! The desktop settings file: `settings.json` in the app config dir.
//!
//! macOS `~/Library/Application Support/TramaWorkspace`, Windows
//! `%APPDATA%\TramaWorkspace`, Linux and other Unix
//! `$XDG_CONFIG_HOME/TramaWorkspace` (default `~/.config/TramaWorkspace`).
//! `TRAMA_DATA_DIR` relocates it with the rest of the app data, for isolated
//! instances. Unknown or invalid fields fall back to their defaults so an
//! older or hand-edited file never blocks startup. No secrets live here:
//! provider API keys are in the OS keychain, held by the daemon.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "settings.json";
pub const MIN_FONT_SIZE: f32 = 11.;
pub const MAX_FONT_SIZE: f32 = 18.;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccentChoice {
    #[default]
    Indigo,
    Blue,
    Violet,
    Green,
    Orange,
    Pink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    Compact,
    #[default]
    Comfortable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralSettings {
    /// Reopen the last selected thread at startup.
    pub restore_last_thread: bool,
    /// Project (workspace) selected at startup; `None`: the first one.
    pub default_project: Option<String>,
    /// UI language. Only English exists today.
    pub language: String,
    /// Always off: TramaWorkspace sends no telemetry. Stored so the choice
    /// is explicit if telemetry ever exists.
    pub telemetry: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            restore_last_thread: true,
            default_project: None,
            language: "en".into(),
            telemetry: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub theme: ThemeChoice,
    pub accent: AccentChoice,
    /// UI font size in px.
    pub font_size: f32,
    pub density: Density,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            accent: AccentChoice::Indigo,
            font_size: 13.,
            density: Density::Comfortable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub general: GeneralSettings,
    pub appearance: AppearanceSettings,
    /// Action id → keystrokes, only for actions the user rebound. An empty
    /// list unbinds the action.
    pub keybindings: BTreeMap<String, Vec<String>>,
}

impl Settings {
    /// Clamp values a hand edit could break.
    pub fn sanitized(mut self) -> Self {
        let size = self.appearance.font_size;
        self.appearance.font_size = if size.is_finite() {
            size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
        } else {
            AppearanceSettings::default().font_size
        };
        self.general.telemetry = false;
        self
    }
}

/// The settings directory for this platform (see the module docs).
pub fn config_dir() -> Option<PathBuf> {
    config_dir_from(|key| std::env::var_os(key).filter(|value| !value.is_empty()))
}

fn config_dir_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(dir) = env(trama_protocol::DATA_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    if cfg!(target_os = "macos") {
        Some(PathBuf::from(env("HOME")?).join("Library/Application Support/TramaWorkspace"))
    } else if cfg!(windows) {
        let base = env("APPDATA").or_else(|| {
            env("USERPROFILE").map(|home| PathBuf::from(home).join("AppData/Roaming").into())
        })?;
        Some(PathBuf::from(base).join("TramaWorkspace"))
    } else if let Some(xdg) = env("XDG_CONFIG_HOME") {
        Some(PathBuf::from(xdg).join("TramaWorkspace"))
    } else {
        Some(PathBuf::from(env("HOME")?).join(".config/TramaWorkspace"))
    }
}

pub fn settings_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(FILE_NAME))
}

/// Read `path`; a missing or unreadable file gives the defaults.
pub fn load_from(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .unwrap_or_default()
        .sanitized()
}

/// Write atomically (temp file + rename).
pub fn save_to(path: &Path, settings: &Settings) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

pub fn load() -> Settings {
    settings_path()
        .map(|path| load_from(&path))
        .unwrap_or_default()
}

pub fn save(settings: &Settings) -> Result<(), String> {
    let path = settings_path().ok_or("no settings directory (HOME is not set)")?;
    save_to(&path, settings).map_err(|err| format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn round_trip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(FILE_NAME);
        assert_eq!(load_from(&path), Settings::default());
        let mut settings = Settings::default();
        settings.appearance.theme = ThemeChoice::Dark;
        settings.appearance.font_size = 15.;
        settings
            .keybindings
            .insert("new_thread".into(), vec!["secondary-shift-t".into()]);
        save_to(&path, &settings).unwrap();
        assert_eq!(load_from(&path), settings);
    }

    #[test]
    fn partial_and_invalid_files_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"appearance":{"font_size":99,"accent":"green"},"general":{"telemetry":true}}"#,
        )
        .unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.appearance.font_size, MAX_FONT_SIZE);
        assert_eq!(settings.appearance.accent, AccentChoice::Green);
        assert_eq!(settings.appearance.theme, ThemeChoice::System);
        assert!(!settings.general.telemetry);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_from(&path), Settings::default());
    }

    #[test]
    fn config_dir_follows_platform_and_override() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| OsString::from(*v))
            }
        };
        assert_eq!(
            config_dir_from(env(&[("TRAMA_DATA_DIR", "/tmp/x"), ("HOME", "/h")])),
            Some(PathBuf::from("/tmp/x"))
        );
        let dir = config_dir_from(env(&[
            ("HOME", "/h"),
            ("APPDATA", "C:/Users/u/AppData/Roaming"),
        ]))
        .unwrap();
        if cfg!(target_os = "macos") {
            assert_eq!(
                dir,
                PathBuf::from("/h/Library/Application Support/TramaWorkspace")
            );
        } else if cfg!(windows) {
            assert_eq!(
                dir,
                PathBuf::from("C:/Users/u/AppData/Roaming/TramaWorkspace")
            );
        } else {
            assert_eq!(dir, PathBuf::from("/h/.config/TramaWorkspace"));
        }
    }
}
