//! Colour palette resolution for the TUI.
//!
//! A [`Palette`] is derived from the global [`AppConfig`] on every `draw` call
//! (it's a plain struct copy — no heap allocation). All views receive a
//! `&Palette` so every colour in the UI flows through one place.
//!
//! Accent names are validated against [`ACCENTS`]; unknown strings fall back to
//! the green mapping so a typo in `config.json` never breaks the UI.

use ratatui::style::Color;
use crate::model::app_config::{AppConfig, ThemeMode};

/// All colour roles used by the views.
#[derive(Clone, Copy, PartialEq)]
pub struct Palette {
    /// Primary text colour.
    pub fg: Color,
    /// Secondary / status / dim text.
    pub dim: Color,
    /// User messages, active field highlight, selection background.
    pub accent: Color,
    /// Foreground on a selected list row (overlaid on `sel_bg`).
    pub sel_fg: Color,
    /// Background for the selected list row.
    pub sel_bg: Color,
}

/// The ordered list of valid accent names exposed to users and the `/settings`
/// UI. Unknown strings in `config.json` fall back to "green".
///
/// Consumed by the `/settings` dashboard to cycle the accent draft.
pub const ACCENTS: &[&str] = &["green", "cyan", "blue", "magenta", "yellow", "red", "white", "orange", "pink"];

/// Resolve an accent name + theme into a concrete [`Color`].
///
/// Exposed crate-wide so the settings view can colour the accent name in its
/// own resolved tint without duplicating the mapping.
pub(crate) fn resolve_accent(name: &str, dark: bool) -> Color {
    match (name, dark) {
        ("green",   true)  => Color::Rgb(57, 255, 20),
        ("green",   false) => Color::Rgb(0, 128, 0),
        ("cyan",    true)  => Color::Rgb(0, 255, 255),
        ("cyan",    false) => Color::Rgb(0, 128, 128),
        ("blue",    true)  => Color::Rgb(90, 160, 255),
        ("blue",    false) => Color::Rgb(0, 0, 200),
        ("magenta", true)  => Color::Rgb(255, 90, 255),
        ("magenta", false) => Color::Rgb(160, 0, 160),
        ("yellow",  true)  => Color::Rgb(255, 225, 60),
        ("yellow",  false) => Color::Rgb(160, 120, 0),
        ("red",     true)  => Color::Rgb(255, 90, 90),
        ("red",     false) => Color::Rgb(200, 0, 0),
        ("white",   true)  => Color::White,
        ("white",   false) => Color::Rgb(20, 20, 20),
        ("orange",  true)  => Color::Rgb(255, 140, 0),
        ("orange",  false) => Color::Rgb(200, 100, 0),
        ("pink",    true)  => Color::Rgb(255, 105, 180),
        ("pink",    false) => Color::Rgb(200, 60, 120),
        // Unknown accent string → fall back to the green mapping for the theme.
        (_,         true)  => Color::Rgb(57, 255, 20),
        (_,         false) => Color::Rgb(0, 128, 0),
    }
}

/// Build a [`Palette`] from the current [`AppConfig`].
///
/// Called once per frame at the top of `view::draw`. The result is stack-only
/// (all `Color` values are `Copy`) so passing `&palette` to sub-draws is zero cost.
pub fn palette(cfg: &AppConfig) -> Palette {
    let dark = cfg.theme == ThemeMode::Dark;
    let accent = resolve_accent(&cfg.accent, dark);

    let (fg, dim) = if dark {
        (Color::White, Color::Rgb(173, 173, 173))
    } else {
        (Color::Rgb(20, 20, 20), Color::Gray)
    };

    // On dark backgrounds the accent is bright/neon so black text is readable.
    // On light backgrounds the accent is muted so white text is readable.
    let sel_fg = if dark { Color::Black } else { Color::White };
    let sel_bg = accent;

    Palette { fg, dim, accent, sel_fg, sel_bg }
}
