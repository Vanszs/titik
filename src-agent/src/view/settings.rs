//! View – in-app settings dashboard (Settings mode).
//!
//! A flat (borderless) two-pane dashboard opened with `/settings` (alias
//! `/config`). The left sidebar lists the five editable sections; the right
//! pane shows the draft value of the selected section. A header line labels the
//! screen and a footer shows context-sensitive key hints.
//!
//! Layout:
//! ```text
//! settings                                   ← header (dim)
//! › API key      | sk-or-...                  ← sidebar | detail
//!   Model        |                            (selected row in accent;
//!   Provider     |                             others dim)
//!   Theme        |
//!   Session name |
//! ↑/↓ move · Enter edit/toggle · …            ← footer (dim)
//! ```
//!
//! This screen is purely presentational; all draft mutation lives in
//! [`app::mode::SettingsState`] and the key handling in
//! [`controller::input::handle_settings`].

use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use crate::app::mode::SettingsState;
use crate::model::app_config::ThemeMode;
use crate::view::theme::Palette;

/// Section labels in display order; index matches `SettingsState::selected`.
const SECTIONS: &[&str] = &["API key", "Model", "Provider", "Theme", "Session name"];

/// Render the settings dashboard for `st` using the given colour `palette`.
///
/// The selected sidebar row and the active detail label are drawn in
/// `palette.accent`; everything else uses `palette.fg` / `palette.dim`. All
/// colours flow through `palette` — no hardcoded `Color::` values.
pub fn draw(frame: &mut Frame, st: &SettingsState, palette: &Palette) {
    // Outer vertical split: header (1) | body (flex) | footer (1).
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(0),    // body (sidebar + detail)
            Constraint::Length(1), // footer / key hints
        ])
        .split(frame.area());

    // Header.
    frame.render_widget(
        Paragraph::new("settings").style(Style::default().fg(palette.dim)),
        outer[0],
    );

    // Body: fixed-width sidebar on the left, detail pane fills the rest.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(22), Constraint::Min(0)])
        .split(outer[1]);

    draw_sidebar(frame, st, palette, body[0]);
    draw_detail(frame, st, palette, body[1]);

    // Footer: hints differ between navigating and editing.
    let footer = if st.editing {
        "type to edit · Enter/Esc done"
    } else {
        "↑/↓ move · Enter edit/toggle · ←/→ accent · Esc save & close"
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(palette.dim)),
        outer[2],
    );
}

/// Render the section list. The selected row is prefixed `› ` and drawn in
/// `palette.accent`; the rest are dim.
fn draw_sidebar(frame: &mut Frame, st: &SettingsState, palette: &Palette, area: ratatui::layout::Rect) {
    let lines: Vec<Line> = SECTIONS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            if i == st.selected {
                Line::from(Span::styled(
                    format!("› {label}"),
                    Style::default().fg(palette.accent),
                ))
            } else {
                Line::from(Span::styled(
                    format!("  {label}"),
                    Style::default().fg(palette.dim),
                ))
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the detail pane for the selected section.
fn draw_detail(frame: &mut Frame, st: &SettingsState, palette: &Palette, area: ratatui::layout::Rect) {
    let lines: Vec<Line> = match st.selected {
        // Theme row: two lines (theme mode + accent), neither is free-text.
        3 => {
            let mode = match st.theme {
                ThemeMode::Dark => "dark",
                ThemeMode::Light => "light",
            };
            vec![
                Line::from(vec![
                    Span::styled("theme: ", Style::default().fg(palette.fg)),
                    Span::styled(mode, Style::default().fg(palette.accent)),
                ]),
                Line::from(vec![
                    Span::styled("accent: ", Style::default().fg(palette.fg)),
                    Span::styled(st.accent.as_str(), Style::default().fg(palette.accent)),
                ]),
                Line::from(Span::styled(
                    "Enter toggles theme · ←/→ cycle accent",
                    Style::default().fg(palette.dim),
                )),
            ]
        }
        // Text rows: show the draft, appending a cursor while editing this row.
        sel => {
            let (label, value) = match sel {
                0 => ("API key", st.api_key.as_str()),
                1 => ("Model", st.model.as_str()),
                2 => ("Provider", st.provider.as_str()),
                _ => ("Session name", st.name.as_str()),
            };
            let editing_this = st.editing; // selected == sel by construction here
            let label_color = if editing_this { palette.accent } else { palette.fg };
            let shown = if editing_this {
                format!("{value}█")
            } else {
                value.to_string()
            };
            vec![
                Line::from(Span::styled(label, Style::default().fg(label_color))),
                Line::from(Span::styled(shown, Style::default().fg(palette.fg))),
            ]
        }
    };
    frame.render_widget(Paragraph::new(lines), area);
}
