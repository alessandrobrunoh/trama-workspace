//! Debug-build-only startup script for screenshots and manual QA.
//!
//! Environment variables (ignored in release builds):
//! - `TRAMA_DEV_SELECT_THREAD`: select the first thread whose title contains this text
//! - `TRAMA_DEV_OVERLAY`: `palette[:query]`, `projects`, `new-thread`, or `shortcuts`
//! - `TRAMA_DEV_DRAWER=1`: open the Changes drawer
//! - `TRAMA_DEV_DIFF=<path>`: load a unified diff file into the Changes drawer
//! - `TRAMA_DEV_EXPAND_TOOL=<n>`: expand the n-th tool call (0-based)
//! - `TRAMA_DEV_APPEARANCE=dark|light`: force the initial appearance

#[derive(Debug, Default, Clone)]
pub struct DevScript {
    pub select_thread: Option<String>,
    pub overlay: Option<String>,
    pub drawer: bool,
    pub diff: Option<(String, String)>,
    pub expand_tool: Option<usize>,
}

pub fn from_env() -> Option<DevScript> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let var = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let diff = var("TRAMA_DEV_DIFF").and_then(|path| {
        let text = std::fs::read_to_string(&path).ok()?;
        let label = std::path::Path::new(&path)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".into());
        Some((label, text))
    });
    let script = DevScript {
        select_thread: var("TRAMA_DEV_SELECT_THREAD"),
        overlay: var("TRAMA_DEV_OVERLAY"),
        drawer: var("TRAMA_DEV_DRAWER").is_some(),
        diff,
        expand_tool: var("TRAMA_DEV_EXPAND_TOOL").and_then(|n| n.parse().ok()),
    };
    let any = script.select_thread.is_some()
        || script.overlay.is_some()
        || script.drawer
        || script.diff.is_some();
    any.then_some(script)
}

pub fn appearance() -> Option<gpui_kit::component::theme::ThemeMode> {
    use gpui_kit::component::theme::ThemeMode;
    if !cfg!(debug_assertions) {
        return None;
    }
    match std::env::var("TRAMA_DEV_APPEARANCE").ok()?.as_str() {
        "dark" => Some(ThemeMode::Dark),
        "light" => Some(ThemeMode::Light),
        _ => None,
    }
}
