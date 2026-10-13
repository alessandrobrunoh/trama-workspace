//! Visual system: colors, spacing, type and radii in one place.
//!
//! Views read [`palette`] instead of writing `rgb(..)` literals. [`apply`]
//! also pushes the palette into the GPUI Kit theme so stock components
//! (buttons, inputs, dialogs, the command palette) match the custom chrome.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::*;

/// Semantic color tokens for one appearance.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Center canvas behind the transcript.
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
        bg: hex(0x161718),
        panel: hex(0x1b1c1e),
        elevated: hex(0x212225),
        hover: hex(0x26282b),
        selected: hex(0x2c2f34),
        border: hex(0x2a2c30),
        border_strong: hex(0x383b40),
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
        bg: hex(0xffffff),
        panel: hex(0xf7f7f8),
        elevated: hex(0xffffff),
        hover: hex(0xefeff1),
        selected: hex(0xe6e7ea),
        border: hex(0xe4e5e8),
        border_strong: hex(0xd3d5d9),
        text: hex(0x1b1c1f),
        text_muted: hex(0x5d6168),
        text_faint: hex(0x989ca3),
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

/// Palette for the active appearance.
pub fn palette(cx: &App) -> Palette {
    if cx.theme().is_dark() {
        dark()
    } else {
        light()
    }
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

// Layout.
pub fn sidebar_width() -> Pixels {
    px(264.)
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
pub fn status_bar_height() -> Pixels {
    px(24.)
}

/// Switch appearance and restyle the Kit theme from the palette.
pub fn apply(mode: ThemeMode, cx: &mut App) {
    Theme::change(mode, None, cx);
    let p = if mode.is_dark() { dark() } else { light() };
    Theme::update(cx, |theme| {
        theme.radius = radius();
        theme.radius_lg = radius_lg();
        theme.mono_font_family = MONO_FONT.into();
        theme.shadow = false;
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
