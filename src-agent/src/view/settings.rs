//! View – in-app settings form (Settings mode).
//!
//! A minimalist settings screen opened with `/settings` (alias `/config`).
//! Every editable field is shown inline — selected row is prefixed with `› `
//! and drawn in accent colour; others are dim. A footer shows context-sensitive
//! key hints.
//!
//! Layout:
//! ```text
//!  settings
//! ──────────────────────────────────────────────────
//!
//!   › API key       sk-or-v1-abc…
//!     Model         openai/gpt-oss-120b
//!     Provider      groq
//!     Theme         dark   ·   accent green
//!     Session name  my project
//!     Workdir       /home/user/project
//!
//!  ↑/↓ move · Enter edit/toggle · ←/→ accent · Esc save & close
//! ```
//!
//! This screen is purely presentational; all draft mutation lives in
//! [`app::mode::SettingsState`] and the key handling in
//! [`controller::input::handle_settings`].

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use crate::app::mode::SettingsState;
use crate::model::app_config::ThemeMode;
use crate::view::theme::Palette;

/// Section labels in display order; index matches `SettingsState::selected`.
const LABELS: &[&str] = &["API key", "Model", "Provider", "Theme", "Session name", "Workdir"];

/// Truncate `s` to at most `max` chars, appending `…` if cut.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        // Reserve one char for the ellipsis
        let cut = max.saturating_sub(1);
        chars[..cut].iter().collect::<String>() + "…"
    }
}

/// Render the settings form for `st` using the given colour `palette`.
///
/// All colours flow through `palette` — no hardcoded `Color::` values.
pub fn draw(frame: &mut Frame, st: &SettingsState, palette: &Palette) {
    // Three-zone vertical split: title row | form (flex) | footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // title + bottom rule
            Constraint::Min(0),    // form rows (flat)
            Constraint::Length(1), // footer / key hints
        ])
        .split(frame.area());

    // Title header: bottom rule only, matching the chat screen's header style.
    let title_block = Block::new()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim))
        .padding(Padding::horizontal(2));

    let title_inner = title_block.inner(outer[0]);
    frame.render_widget(title_block, outer[0]);
    frame.render_widget(
        Paragraph::new(Span::styled("settings", Style::default().fg(palette.dim))),
        title_inner,
    );

    // Form rows — flat, no surrounding block.
    // Inset horizontally to align under the title text.
    let form_area = outer[1].inner(Margin { horizontal: 2, vertical: 0 });

    // Compute available width for values: form width minus marker(2) + label(14) = 16.
    let form_w = form_area.width as usize;
    let value_w = form_w.saturating_sub(16);

    // Build one Line per field.
    let mut lines: Vec<Line> = Vec::with_capacity(LABELS.len());

    for (i, label) in LABELS.iter().enumerate() {
        let selected = i == st.selected;

        // Marker: "› " for selected, "  " otherwise — both in accent colour.
        let marker = Span::styled(
            if selected { "› " } else { "  " },
            Style::default().fg(palette.accent),
        );

        // Label: left-padded to width 14.
        let label_text = format!("{:<14}", label);
        let label_span = Span::styled(
            label_text,
            Style::default().fg(if selected { palette.accent } else { palette.dim }),
        );

        // Value spans differ per field.
        let value_spans: Vec<Span> = if i == 3 {
            // Theme row: "dark/light   ·   accent <name>"
            let mode = match st.theme {
                ThemeMode::Dark => "dark",
                ThemeMode::Light => "light",
            };
            vec![
                Span::styled(mode, Style::default().fg(palette.accent)),
                Span::styled("   ·   accent ", Style::default().fg(palette.dim)),
                Span::styled(st.accent.as_str(), Style::default().fg(palette.accent)),
            ]
        } else {
            // Text rows: show draft value, truncated to fit, cursor if editing this row.
            let raw = match i {
                0 => st.api_key.as_str(),
                1 => st.model.as_str(),
                2 => st.provider.as_str(),
                4 => st.name.as_str(),
                5 => st.workdir.as_str(),
                _ => st.name.as_str(),
            };
            let editing_here = st.editing && selected;
            // Reserve 1 char for cursor when editing so it doesn't push past the edge.
            let truncate_w = if editing_here {
                value_w.saturating_sub(1)
            } else {
                value_w
            };
            let mut shown = truncate(raw, truncate_w);
            if editing_here {
                shown.push('█');
            }
            vec![Span::styled(shown, Style::default().fg(palette.fg))]
        };

        // Compose the full line: marker + label + value(s).
        let mut spans = vec![marker, label_span];
        spans.extend(value_spans);
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), form_area);

    // Footer: hints differ between navigating and editing.
    // Inset to match form alignment.
    let footer_area = outer[2].inner(Margin { horizontal: 2, vertical: 0 });
    let footer = if st.editing {
        "type to edit · Enter/Esc done"
    } else {
        "↑/↓ move · Enter edit/toggle · ←/→ accent · Esc save & close"
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(palette.dim)),
        footer_area,
    );
}
