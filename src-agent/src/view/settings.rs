//! View – in-app settings dashboard (Settings mode).
//!
//! Two-pane layout: a narrow sidebar lists the [`SETTING_CATEGORIES`]; the
//! detail pane on the right shows all fields for the selected category.  Focus
//! travels left→right (sidebar → detail) and back.  A context-sensitive footer
//! at the bottom shows key hints.
//!
//! Border convention (strict, matches project rules):
//! - Header: `Borders::BOTTOM` only.
//! - Sidebar/detail divider: `Borders::RIGHT` on the sidebar pane.
//! - Footer: plain dim line (no full box anywhere).
//!
//! Layout:
//! ```text
//!  settings
//! ─────────────────────────────────────────────────────────
//! │ Connection  │  API key       sk-or-v1-abc…
//! │ Appearance  │  Model         openai/gpt-oss-120b
//! │ Session     │  Provider      groq
//!               │
//!  ↑/↓ category · →/Enter fields · Esc save & close
//! ```
//!
//! All draft mutation lives in [`app::mode::SettingsState`]; key handling lives
//! in [`controller::input::handle_settings`].

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use crate::app::mode::{SETTING_CATEGORIES, SettingField, SettingsState};
use crate::model::app_config::ThemeMode;
use crate::view::theme::{resolve_accent, Palette};

/// Sidebar column width in terminal columns (includes the RIGHT border char).
const SIDEBAR_W: u16 = 18;

/// Truncate `s` to at most `max` chars, appending `…` if cut.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let cut = max.saturating_sub(1);
        chars[..cut].iter().collect::<String>() + "…"
    }
}

/// Render the settings dashboard for `st` using the given colour `palette`.
///
/// All colours flow through `palette` — no hardcoded `Color::` values except
/// the per-accent tint resolved via [`resolve_accent`].
pub fn draw(frame: &mut Frame, st: &SettingsState, palette: &Palette) {
    let dark = st.theme == ThemeMode::Dark;

    // Outer vertical zones: header | body | footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header text + BOTTOM border
            Constraint::Min(0),    // sidebar + detail
            Constraint::Length(1), // footer key hints
        ])
        .split(frame.area());

    // --- Header ---
    // "settings" in dim, with a BOTTOM border rule — same idiom as chat.rs.
    let header_block = Block::new()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim));
    let header_inner = header_block.inner(outer[0]);
    frame.render_widget(header_block, outer[0]);
    frame.render_widget(
        Paragraph::new(Span::styled("settings", Style::default().fg(palette.dim)))
            .style(Style::default()),
        header_inner.inner(Margin { horizontal: 2, vertical: 0 }),
    );

    // --- Body: horizontal split into sidebar + detail ---
    let body_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(SIDEBAR_W), // sidebar with RIGHT border as column divider
            Constraint::Min(0),            // detail pane
        ])
        .split(outer[1]);

    // Sidebar block: RIGHT border acts as the column divider.
    let sidebar_block = Block::new()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(palette.dim));
    let sidebar_inner = sidebar_block.inner(body_cols[0]);
    frame.render_widget(sidebar_block, body_cols[0]);

    // Sidebar content: one line per category; inset by 1 col on the left.
    let sidebar_content = sidebar_inner.inner(Margin { horizontal: 1, vertical: 1 });
    let sidebar_lines: Vec<Line> = SETTING_CATEGORIES
        .iter()
        .enumerate()
        .map(|(i, cat)| {
            let is_selected = i == st.cat;
            let (marker, color) = if is_selected {
                // Show marker regardless of which pane has focus; dim slightly
                // when focus is in the detail pane to signal the sidebar is passive.
                let c = if st.in_detail {
                    palette.dim
                } else {
                    palette.accent
                };
                ("› ", c)
            } else {
                ("  ", palette.dim)
            };
            Line::from(vec![
                Span::styled(marker, Style::default().fg(color)),
                Span::styled(cat.name, Style::default().fg(color)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(sidebar_lines), sidebar_content);

    // Detail pane: inset by 1 col on each side, 1 row on top.
    let detail_inner = body_cols[1].inner(Margin { horizontal: 2, vertical: 1 });
    let cat_fields = SETTING_CATEGORIES[st.cat].fields;

    // Available width for value column: detail width minus label column (14) minus
    // marker (2).
    let detail_w = detail_inner.width as usize;
    let value_w = detail_w.saturating_sub(16);

    let detail_lines: Vec<Line> = cat_fields
        .iter()
        .enumerate()
        .map(|(i, &f)| {
            let is_selected = st.in_detail && i == st.field;

            // Marker: only shown when detail pane has focus.
            let marker = Span::styled(
                if is_selected { "› " } else { "  " },
                Style::default().fg(palette.accent),
            );

            // Label: left-padded to 14 cols.
            let label_text = format!("{:<14}", f.label());
            let label_color = if is_selected { palette.accent } else { palette.dim };
            let label_span = Span::styled(label_text, Style::default().fg(label_color));

            // Value span(s).
            let value_spans: Vec<Span> = match f {
                SettingField::Theme => {
                    let mode_str = match st.theme {
                        ThemeMode::Dark  => "dark",
                        ThemeMode::Light => "light",
                    };
                    vec![Span::styled(mode_str, Style::default().fg(palette.accent))]
                }
                SettingField::Accent => {
                    // Show the accent name coloured in its resolved tint.
                    let tint: Color = resolve_accent(&st.accent, dark);
                    vec![Span::styled(st.accent.as_str(), Style::default().fg(tint))]
                }
                SettingField::AwarenessEnabled => {
                    // Boolean toggle: on/off.
                    let v = if st.awareness_enabled { "on" } else { "off" };
                    vec![Span::styled(v, Style::default().fg(palette.accent))]
                }
                SettingField::ClassifierEnabled => {
                    // Boolean toggle: on/off (master switch for the harness).
                    let v = if st.classifier_enabled { "on" } else { "off" };
                    vec![Span::styled(v, Style::default().fg(palette.accent))]
                }
                SettingField::AwarenessSource => {
                    // Boolean toggle: inherit the session model, or a custom one.
                    let v = if st.awareness_inherit {
                        "inherit parent"
                    } else {
                        "custom"
                    };
                    vec![Span::styled(v, Style::default().fg(palette.accent))]
                }
                SettingField::AwarenessModel | SettingField::AwarenessProvider
                    if st.awareness_inherit =>
                {
                    // Irrelevant while inheriting → dimmed "(inherited)".
                    vec![Span::styled("(inherited)", Style::default().fg(palette.dim))]
                }
                _ => {
                    // Text field: show draft with optional cursor block.
                    let raw: &str = match f {
                        SettingField::ApiKey   => st.api_key.as_str(),
                        SettingField::Model    => st.model.as_str(),
                        SettingField::Provider => {
                            if st.provider.is_empty() {
                                // placeholder shown in dim — handled specially below
                                ""
                            } else {
                                st.provider.as_str()
                            }
                        }
                        SettingField::Name    => st.name.as_str(),
                        SettingField::Workdir => st.workdir.as_str(),
                        // Reached only when source == "custom" (the inherit case
                        // is handled in the arm above).
                        SettingField::AwarenessModel    => st.awareness_model.as_str(),
                        SettingField::AwarenessProvider => st.awareness_provider.as_str(),
                        SettingField::ClassifierModel    => st.classifier_model.as_str(),
                        SettingField::ClassifierProvider => st.classifier_provider.as_str(),
                        SettingField::AllowedFolders     => st.allowed_folders.as_str(),
                        // Theme, Accent, and the toggles are handled above; this
                        // arm is unreachable for them.
                        _ => "",
                    };
                    let editing_here = st.editing && is_selected;
                    let truncate_w = if editing_here {
                        value_w.saturating_sub(1)
                    } else {
                        value_w
                    };
                    // Provider placeholder when empty.
                    if f == SettingField::Provider && raw.is_empty() && !editing_here {
                        return Line::from(vec![
                            marker,
                            label_span,
                            Span::styled("default", Style::default().fg(palette.dim)),
                        ]);
                    }
                    // ApiKey: truncate to max 40 chars.
                    let display_raw = if f == SettingField::ApiKey {
                        truncate(raw, truncate_w.min(40))
                    } else {
                        truncate(raw, truncate_w)
                    };
                    let mut shown = display_raw;
                    if editing_here {
                        shown.push('█');
                    }
                    vec![Span::styled(shown, Style::default().fg(palette.fg))]
                }
            };

            let mut spans = vec![marker, label_span];
            spans.extend(value_spans);
            Line::from(spans)
        })
        .collect();

    frame.render_widget(Paragraph::new(detail_lines), detail_inner);

    // --- Footer ---
    // Plain dim hint line; no border (matches the flat style in the original).
    let footer_area = outer[2].inner(Margin { horizontal: 2, vertical: 0 });
    let hint = if st.editing {
        "type to edit · Enter/Esc done"
    } else if st.in_detail {
        "↑/↓ field · Enter edit/toggle · ←/→ accent · ← back"
    } else {
        "↑/↓ category · →/Enter fields · Esc save & close"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(palette.dim)),
        footer_area,
    );
}
