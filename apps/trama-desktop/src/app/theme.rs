//! Visual system: colors, spacing, type and radii in one place.
//!
//! Views read [`palette`] instead of writing `rgb(..)` literals. [`apply`]
//! also pushes the palette into the GPUI Kit theme so stock components
//! (buttons, inputs, dialogs, the command palette) match the custom chrome.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::h_flex;
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::*;

/// Semantic color tokens for one appearance.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Window backdrop: the area around the content card (sidebar, title
    /// and status bars sit directly on it).
    pub canvas: Hsla,
    /// Content card behind the transcript, lists and panes.
    pub bg: Hsla,
    /// Sidebar, drawer, title and status bars.
    pub panel: Hsla,
    /// Cards: composer, tool rows, user turns, popovers.
    pub elevated: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    pub accent: Hsla,
    pub accent_soft: Hsla,
    pub accent_text: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub diff_add_bg: Hsla,
    pub diff_del_bg: Hsla,
    pub diff_add_text: Hsla,
    pub diff_del_text: Hsla,
    pub diff_hunk_bg: Hsla,
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn hexa(value: u32, alpha: f32) -> Hsla {
    hex(value).opacity(alpha)
}

pub fn dark() -> Palette {
    Palette {
        canvas: hex(0x101112),
        bg: hex(0x18191b),
        panel: hex(0x101112),
        elevated: hex(0x1f2023),
        hover: hex(0x232427),
        selected: hex(0x2a2b2f),
        border: hex(0x26272a),
        border_strong: hex(0x34363a),
        text: hex(0xe6e7e9),
        text_muted: hex(0xa1a5ab),
        text_faint: hex(0x6d7177),
        accent: hex(0x7a8cff),
        accent_soft: hexa(0x7a8cff, 0.16),
        accent_text: hex(0xaab6ff),
        success: hex(0x4cc38a),
        warning: hex(0xe2b55a),
        danger: hex(0xef6f6c),
        diff_add_bg: hexa(0x4cc38a, 0.11),
        diff_del_bg: hexa(0xef6f6c, 0.11),
        diff_add_text: hex(0x7ad9a8),
        diff_del_text: hex(0xf39a97),
        diff_hunk_bg: hexa(0x7a8cff, 0.08),
    }
}

pub fn light() -> Palette {
    Palette {
        canvas: hex(0xf4f4f5),
        bg: hex(0xffffff),
        panel: hex(0xf4f4f5),
        elevated: hex(0xffffff),
        hover: hex(0xeaeaec),
        selected: hex(0xe3e3e6),
        border: hex(0xe6e6e9),
        border_strong: hex(0xd6d6da),
        text: hex(0x1f2023),
        text_muted: hex(0x5f6168),
        text_faint: hex(0x8f9299),
        accent: hex(0x4e66e8),
        accent_soft: hexa(0x4e66e8, 0.12),
        accent_text: hex(0x3b52d1),
        success: hex(0x1f9d61),
        warning: hex(0xb7811c),
        danger: hex(0xd8443f),
        diff_add_bg: hexa(0x1f9d61, 0.10),
        diff_del_bg: hexa(0xd8443f, 0.09),
        diff_add_text: hex(0x16794a),
        diff_del_text: hex(0xb3322e),
        diff_hunk_bg: hexa(0x4e66e8, 0.06),
    }
}

/// Light, dark, or follow the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppearancePref {
    #[default]
    System,
    Light,
    Dark,
}

/// Accent hues offered by the Appearance setting.
#[allow(dead_code)] // Picked by the Appearance setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Accent {
    #[default]
    Indigo,
    Blue,
    Violet,
    Green,
    Orange,
    Pink,
}

#[allow(dead_code)] // Read by the Appearance setting.
impl Accent {
    pub const ALL: [Accent; 6] = [
        Accent::Indigo,
        Accent::Blue,
        Accent::Violet,
        Accent::Green,
        Accent::Orange,
        Accent::Pink,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Accent::Indigo => "Indigo",
            Accent::Blue => "Blue",
            Accent::Violet => "Violet",
            Accent::Green => "Green",
            Accent::Orange => "Orange",
            Accent::Pink => "Pink",
        }
    }

    /// (accent, accent_text) for the given appearance.
    pub fn colors(self, dark: bool) -> (Hsla, Hsla) {
        let (light_hex, dark_hex) = match self {
            Accent::Indigo => (0x5e6ad2, 0x8b95f0),
            Accent::Blue => (0x2f6fde, 0x6ea0ff),
            Accent::Violet => (0x8b4fd6, 0xb38cf2),
            Accent::Green => (0x1f8f5a, 0x4cc38a),
            Accent::Orange => (0xd2691e, 0xf0a05a),
            Accent::Pink => (0xd1427f, 0xf07aab),
        };
        if dark {
            (hex(dark_hex), hex(dark_hex))
        } else {
            (hex(light_hex), hex(light_hex))
        }
    }
}

/// User-facing theme preferences. Store it with [`set_prefs`]; every
/// [`palette`] read reflects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThemePrefs {
    pub appearance: AppearancePref,
    pub accent: Accent,
}

impl Global for ThemePrefs {}

/// Current preferences (defaults when never set).
pub fn prefs(cx: &App) -> ThemePrefs {
    cx.try_global::<ThemePrefs>().copied().unwrap_or_default()
}

/// Store preferences and re-apply the theme. `os` is the window's current
/// appearance, used when the preference is [`AppearancePref::System`].
pub fn set_prefs(prefs: ThemePrefs, os: WindowAppearance, cx: &mut App) {
    cx.set_global(prefs);
    apply(resolve_mode(prefs.appearance, os), cx);
    cx.refresh_windows();
}

/// The mode a preference resolves to under the given OS appearance.
pub fn resolve_mode(pref: AppearancePref, os: WindowAppearance) -> ThemeMode {
    match pref {
        AppearancePref::System => mode_for(os),
        AppearancePref::Light => ThemeMode::Light,
        AppearancePref::Dark => ThemeMode::Dark,
    }
}

fn with_accent(mut p: Palette, accent: Accent, dark: bool) -> Palette {
    let (a, text) = accent.colors(dark);
    p.accent = a;
    p.accent_text = text;
    p.accent_soft = a.opacity(if dark { 0.16 } else { 0.12 });
    p.diff_hunk_bg = a.opacity(if dark { 0.08 } else { 0.06 });
    p
}

/// Palette for a mode with the stored accent.
pub fn palette_for(mode: ThemeMode, cx: &App) -> Palette {
    let is_dark = mode.is_dark();
    let base = if is_dark { dark() } else { light() };
    with_accent(base, prefs(cx).accent, is_dark)
}

/// Palette for the active appearance.
pub fn palette(cx: &App) -> Palette {
    palette_for(
        if cx.theme().is_dark() {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        cx,
    )
}

#[allow(dead_code)] // Shared building block for label chips.
/// A small rounded label chip with a colored dot (issue-label style).
pub fn pill(label: impl Into<SharedString>, color: Hsla, p: &Palette) -> Div {
    h_flex()
        .flex_shrink_0()
        .h(px(20.))
        .px(sp(2.))
        .gap(sp(1.5))
        .items_center()
        .rounded_full()
        .border_1()
        .border_color(p.border_strong)
        .bg(p.bg)
        .text_size(text_xs())
        .text_color(p.text_muted)
        .child(div().size(px(6.)).rounded_full().bg(color))
        .child(label.into())
}

/// Short form of an id for dense lists: the last 6 characters (ULIDs
/// start with a timestamp, so their tail is what tells them apart).
pub fn short_id(id: &str) -> String {
    let chars: Vec<char> = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    chars[chars.len().saturating_sub(6)..].iter().collect()
}

/// A compact mono identifier (thread ids, short hashes).
pub fn mono_id(id: impl Into<SharedString>, p: &Palette) -> Div {
    div()
        .flex_shrink_0()
        .font_family(MONO_FONT)
        .text_size(text_xs())
        .text_color(p.text_faint)
        .child(id.into())
}

// Type. UI chrome uses the system face at small sizes; transcript prose is
// larger with looser leading; code and numbers use the mono face.
pub const UI_FONT: &str = ".SystemUIFont";
pub const PROSE_FONT: &str = ".SystemUIFont";
pub const MONO_FONT: &str = "Menlo";

pub fn text_xs() -> Pixels {
    px(11.)
}
pub fn text_sm() -> Pixels {
    px(12.)
}
pub fn text_ui() -> Pixels {
    px(13.)
}
pub fn text_prose() -> Pixels {
    px(14.)
}
pub fn text_title() -> Pixels {
    px(18.)
}

// Spacing: a 4px scale.
pub fn sp(steps: f32) -> Pixels {
    px(4. * steps)
}

// Radii.
pub fn radius_sm() -> Pixels {
    px(4.)
}
pub fn radius() -> Pixels {
    px(6.)
}
pub fn radius_lg() -> Pixels {
    px(10.)
}
/// Corner radius of the main content card.
pub fn radius_card() -> Pixels {
    px(8.)
}
/// Gap between the window edge / sidebar and the content card.
pub fn card_inset() -> Pixels {
    px(8.)
}
/// Height of list rows (sidebar nav, thread list).
pub fn row_height() -> Pixels {
    px(28.)
}

// Layout.
pub fn sidebar_width() -> Pixels {
    px(240.)
}
pub fn drawer_width() -> Pixels {
    px(440.)
}
pub fn transcript_width() -> Pixels {
    px(760.)
}
pub fn title_bar_height() -> Pixels {
    px(38.)
}
/// Height of the header at the top of the content card.
pub fn card_header_height() -> Pixels {
    px(40.)
}
/// Room left of the title bar content for the macOS traffic lights.
pub fn traffic_light_inset() -> Pixels {
    px(80.)
}
pub fn status_bar_height() -> Pixels {
    px(24.)
}

/// Switch appearance and restyle the Kit theme from the palette.
pub fn apply(mode: ThemeMode, cx: &mut App) {
    Theme::change(mode, None, cx);
    let p = palette_for(mode, cx);
    Theme::update(cx, |theme| {
        theme.radius = radius();
        theme.radius_lg = radius_lg();
        theme.mono_font_family = MONO_FONT.into();
        theme.shadow = true;
        theme.font_size = text_ui();

        let c = &mut theme.colors;
        c.background = p.bg;
        c.foreground = p.text;
        c.border = p.border;
        c.input = p.border_strong;
        c.ring = p.accent;
        c.caret = p.accent;
        c.selection = p.accent.opacity(0.35);
        c.muted = p.hover;
        c.muted_foreground = p.text_muted;
        c.accent = p.hover;
        c.accent_foreground = p.text;
        c.primary = p.accent;
        c.primary_hover = p.accent.opacity(0.9);
        c.primary_active = p.accent.opacity(0.8);
        c.primary_foreground = hex(0xffffff);
        c.button_primary = p.accent;
        c.button_primary_hover = p.accent.opacity(0.9);
        c.button_primary_active = p.accent.opacity(0.8);
        c.button_primary_foreground = hex(0xffffff);
        c.secondary = p.elevated;
        c.secondary_hover = p.hover;
        c.secondary_active = p.selected;
        c.secondary_foreground = p.text;
        c.popover = p.elevated;
        // Dialogs: a light scrim so the card behind stays readable.
        c.overlay = if mode.is_dark() {
            hsla(0., 0., 0., 0.45)
        } else {
            hsla(240. / 360., 0.1, 0.1, 0.18)
        };
        c.popover_foreground = p.text;
        c.list = p.elevated;
        c.list_hover = p.hover;
        c.list_active = p.accent_soft;
        c.list_active_border = p.accent.opacity(0.0);
        c.sidebar = p.panel;
        c.sidebar_foreground = p.text;
        c.sidebar_border = p.border;
        c.sidebar_accent = p.selected;
        c.sidebar_accent_foreground = p.text;
        c.title_bar = p.panel;
        c.title_bar_border = p.border;
        c.status_bar = p.panel;
        c.status_bar_border = p.border;
        c.success = p.success;
        c.warning = p.warning;
        c.danger = p.danger;
        c.link = p.accent_text;
        c.link_hover = p.accent;
        c.link_active = p.accent;
    });
}

/// Follow the window's light or dark appearance.
pub fn mode_for(appearance: WindowAppearance) -> ThemeMode {
    match appearance {
        WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeMode::Light,
        WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeMode::Dark,
    }
}

/// One of the 16 ANSI terminal colors in this palette (8..16 are the bright
/// variants). Matches the colors command output uses in the transcript.
pub fn ansi(n: u8, p: &Palette) -> Hsla {
    let base = match n % 8 {
        0 => p.text_faint,
        1 => p.danger,
        2 => p.success,
        3 => p.warning,
        4 => p.accent_text,
        5 => hsla(300. / 360., 0.45, 0.65, 1.),
        6 => hsla(187. / 360., 0.45, 0.55, 1.),
        _ => p.text_muted,
    };
    match n {
        8 => p.text_muted,
        15 => p.text,
        9..=14 => {
            let mut bright = base;
            bright.l = (bright.l + 0.08).min(0.92);
            bright
        }
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` brings GPUI's own `test` attribute into scope.
    use core::prelude::v1::test;

    #[test]
    fn short_id_keeps_the_random_tail() {
        assert_eq!(short_id("01JZ8QK3M4ABCDEF"), "ABCDEF");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn appearance_pref_resolves_against_the_os() {
        assert!(resolve_mode(AppearancePref::System, WindowAppearance::Dark).is_dark());
        assert!(!resolve_mode(AppearancePref::System, WindowAppearance::Light).is_dark());
        assert!(resolve_mode(AppearancePref::Dark, WindowAppearance::Light).is_dark());
        assert!(!resolve_mode(AppearancePref::Light, WindowAppearance::Dark).is_dark());
    }

    #[test]
    fn accent_overrides_only_accent_tokens() {
        let base = light();
        let tinted = with_accent(base, Accent::Green, false);
        assert_eq!(tinted.accent, Accent::Green.colors(false).0);
        assert_eq!(tinted.bg, base.bg);
        assert_eq!(tinted.text, base.text);
        assert_eq!(Accent::ALL.len(), 6);
    }
}
