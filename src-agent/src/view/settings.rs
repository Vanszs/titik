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
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table},
    Frame,
};
use crate::app::mode::{filter_models, SETTING_CATEGORIES, SettingField, SettingsState};
use crate::app::mode::settings::{ModelModal, ProviderModal};
use crate::dto::openrouter::ModelInfo;
use crate::model::app_config::ThemeMode;
use crate::view::theme::{resolve_accent, Palette};

/// Sidebar column width in terminal columns (includes the RIGHT border char).
const SIDEBAR_W: u16 = 22;

/// Format a per-token USD price string (e.g. `"0.00000015"`, as OpenRouter
/// reports it) as a per-MILLION-token dollar amount: `$0.15`. Returns `$?` when
/// the value is absent or unparseable, so a row always renders something.
fn price_per_million(per_token: Option<&String>) -> String {
    match per_token.and_then(|s| s.trim().parse::<f64>().ok()) {
        Some(v) => format!("${:.2}", v * 1_000_000.0),
        None => "$?".to_string(),
    }
}

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
/// `models_cache` is the cached OpenRouter model catalogue, threaded through so
/// the Models Select modal's omnisearch can render live results (empty slice
/// when the catalogue hasn't been fetched).
///
/// All colours flow through `palette` — no hardcoded `Color::` values except
/// the per-accent tint resolved via [`resolve_accent`].
pub fn draw(frame: &mut Frame, st: &SettingsState, models_cache: &[ModelInfo], palette: &Palette) {
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
    // Group headers are injected whenever the group changes between consecutive
    // categories. Headers are dim, non-selectable; categories are indented under them.
    let sidebar_content = sidebar_inner.inner(Margin { horizontal: 1, vertical: 1 });
    let mut sidebar_lines: Vec<Line> = Vec::new();
    let mut last_group: Option<&str> = None;
    for (i, cat) in SETTING_CATEGORIES.iter().enumerate() {
        if Some(cat.group) != last_group {
            // Spacer before group header (skip before the very first line).
            if last_group.is_some() {
                sidebar_lines.push(Line::from(""));
            }
            sidebar_lines.push(Line::from(vec![
                Span::styled(
                    cat.group,
                    Style::default().fg(palette.dim).add_modifier(Modifier::DIM),
                ),
            ]));
            last_group = Some(cat.group);
        }
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
        // Indent category name by 2 extra spaces so it sits under its group header.
        sidebar_lines.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(color)),
            Span::styled(marker, Style::default().fg(color)),
            Span::styled(cat.name, Style::default().fg(color)),
        ]));
    }
    frame.render_widget(Paragraph::new(sidebar_lines), sidebar_content);

    // Detail pane: inset by 1 col on each side, 1 row on top.
    let detail_inner = body_cols[1].inner(Margin { horizontal: 2, vertical: 1 });
    let cat_fields = SETTING_CATEGORIES[st.cat].fields;

    // Available width for value column: detail width minus label column (14) minus
    // marker (2).
    let detail_w = detail_inner.width as usize;
    let value_w = detail_w.saturating_sub(16);

    // API Providers / Models Select: custom interactive list screens (no
    // SettingField rows).
    if st.is_providers_category() {
        draw_providers(frame, st, palette, detail_inner);
    } else if st.is_models_category() {
        draw_models(frame, st, palette, detail_inner);
    } else if cat_fields.is_empty() {
        // Stub placeholder for other categories with no fields yet.
        let stub_text = "(stub)";
        frame.render_widget(
            Paragraph::new(stub_text).style(Style::default().fg(palette.dim)),
            detail_inner,
        );
        // Skip the field loop entirely for stub categories.
    } else {

    let mut detail_lines: Vec<Line> = Vec::new();
    for (i, &f) in cat_fields.iter().enumerate() {
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

            // PATH LISTS (Workdir / Allowed dirs): a label row, then one
            // line-wrapped row per entry. Each entry hangs under the value
            // column; the highlighted entry (while managing this field) gets a
            // `›` accent marker, the rest are dim. Multiple lines per field, so
            // this is handled before the single-line value logic below.
            if SettingsState::is_path_list(f) {
                let managing = st.list_editing && is_selected;
                // Affordance shown inline with the label when this field is active
                // but not yet being managed (hints how to open it).
                let label_suffix: Vec<Span> = if is_selected && !managing {
                    vec![Span::styled("list", Style::default().fg(palette.dim))]
                } else {
                    Vec::new()
                };
                let mut header = vec![marker, label_span];
                header.extend(label_suffix);
                detail_lines.push(Line::from(header));

                let entries = st.path_list(f).cloned().unwrap_or_default();
                // Entry rows are indented under the value column; wrap to the
                // remaining width so long absolute paths line-wrap instead of
                // truncating. 4 = 2 (entry marker) + 2 (hanging indent base).
                let entry_w = detail_w.saturating_sub(6).max(1);
                for (ei, entry) in entries.iter().enumerate() {
                    let here = managing && ei == st.list_sel;
                    let (emark, ecolor) = if here {
                        ("  › ", palette.accent)
                    } else {
                        ("    ", palette.dim)
                    };
                    let wrapped = crate::view::markdown::wrap_spans(
                        &[Span::styled(entry.clone(), Style::default().fg(ecolor))],
                        entry_w,
                    );
                    if wrapped.is_empty() {
                        detail_lines.push(Line::from(vec![Span::styled(
                            emark,
                            Style::default().fg(ecolor),
                        )]));
                    }
                    for (wi, vline) in wrapped.into_iter().enumerate() {
                        // First visual line carries the entry marker; continuations
                        // get a 4-col hanging indent so wraps align under it.
                        let prefix = if wi == 0 {
                            Span::styled(emark, Style::default().fg(ecolor))
                        } else {
                            Span::raw("    ")
                        };
                        let mut spans = vec![prefix];
                        spans.extend(vline);
                        detail_lines.push(Line::from(spans));
                    }
                }
                continue;
            }

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
                SettingField::ShortSendEnabled => {
                    // Boolean toggle: on/off (kill switch for the token saver).
                    let v = if st.short_send_enabled { "on" } else { "off" };
                    vec![Span::styled(v, Style::default().fg(palette.accent))]
                }
                SettingField::SlidingCache => {
                    // Boolean toggle: on/off (on only for providers with a sliding
                    // prompt cache, e.g. Anthropic).
                    let v = if st.sliding_cache { "on" } else { "off" };
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
                        // Reached only when source == "custom" (the inherit case
                        // is handled in the arm above).
                        SettingField::AwarenessModel    => st.awareness_model.as_str(),
                        SettingField::AwarenessProvider => st.awareness_provider.as_str(),
                        SettingField::ClassifierModel    => st.classifier_model.as_str(),
                        SettingField::ClassifierProvider => st.classifier_provider.as_str(),
                        // Theme, Accent, the toggles, and the PATH LISTS
                        // (Workdir / AllowedFolders) are handled above; this arm
                        // is unreachable for them.
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
                        detail_lines.push(Line::from(vec![
                            marker,
                            label_span,
                            Span::styled("default", Style::default().fg(palette.dim)),
                        ]));
                        continue;
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
            detail_lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(detail_lines), detail_inner);

    } // end else (non-stub category)

    // --- Footer ---
    // Full-width inverse status bar: background fills the entire footer line
    // edge to edge; text is left-padded by 1 space so it doesn't touch the edge.
    // Context-sensitive: deepest active mode wins (picker → list → editing →
    // field nav → sidebar).
    let footer_rect = outer[2];
    if footer_rect.width > 0 {
        let on_list_field = st.in_detail
            && !st.is_providers_category()
            && !SETTING_CATEGORIES[st.cat].fields.is_empty()
            && SettingsState::is_path_list(st.current_field());
        // Is the model modal currently in live-omnisearch mode? (Model field,
        // OpenRouter provider, non-empty query.)
        let cur_mf = st.mm_current_field();
        let model_search = cur_mf == Some(crate::app::mode::settings::ModelField::Model)
            && st.mm_provider_is_openrouter()
            && st.model_modal.as_ref().map(|m| !m.query.is_empty()).unwrap_or(false);
        let on_route = cur_mf == Some(crate::app::mode::settings::ModelField::Route);
        let on_role  = cur_mf == Some(crate::app::mode::settings::ModelField::Role);
        let role_picker_open = st.mm_role_picker_open();
        let hint = if st.model_modal.is_some() {
            if role_picker_open {
                // The Role checkbox picker owns input while open.
                "↑↓ role · space toggle · enter ok · esc cancel"
            } else if model_search {
                "↑↓ result · enter pick · tab next · esc cancel"
            } else if on_route {
                "↑↓ provider/move · enter pin + next · esc cancel"
            } else if on_role {
                "enter roles · esc cancel"
            } else {
                "↑↓ field · ←→ provider · enter select · esc cancel"
            }
        } else if st.prov_modal.is_some() {
            "↑↓ field · ←→ move/type · enter select · esc cancel"
        } else if st.picker.is_some() {
            "type path · @rel or /abs · ↑/↓ select · Tab descend · Enter pick · Esc cancel"
        } else if st.list_editing {
            "↑/↓ entry · + add · - remove · Enter edit · Esc done"
        } else if st.editing {
            "type to edit · Enter/Esc done"
        } else if st.is_providers_category() && st.in_detail {
            if st.prov_delete_armed {
                "ctrl+x again to CONFIRM delete · any key cancels"
            } else {
                "↑↓ select · + add · ctrl+x delete · esc back"
            }
        } else if st.is_models_category() && st.in_detail {
            if st.model_delete_armed {
                "ctrl+x again to CONFIRM delete · any key cancels"
            } else {
                "↑↓ select · + add · ctrl+x delete · esc back"
            }
        } else if on_list_field {
            "Enter manage list"
        } else if st.in_detail {
            "↑/↓ field · Enter edit/toggle · ←/→ accent · ← back"
        } else {
            "↑/↓ category · →/Enter fields · Esc save & close"
        };
        let bar_style = Style::default()
            .fg(palette.sel_fg)
            .bg(palette.sel_bg)
            .add_modifier(Modifier::BOLD);
        // Pad the hint with a leading space, then right-pad to the full width so
        // the Paragraph's base style (bar_style) paints the background edge to edge.
        let padded = format!(" {:<width$}", hint, width = footer_rect.width.saturating_sub(1) as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::raw(padded))).style(bar_style),
            footer_rect,
        );
    }

    // --- FS directory picker overlay ---
    // Mirrors the chat `@` palette: a compact bordered list (the contained-box
    // exception to the flat border convention) showing the live query line and
    // the windowed directory matches. Rendered last so it floats over the panes.
    if let Some(picker) = st.picker.as_ref() {
        const MAX_VIS: usize = crate::app::mode::PICKER_MAX;

        // Query line first, then the matches. The selected match is highlighted.
        let mut rows: Vec<Line> = Vec::new();
        rows.push(Line::from(vec![
            Span::styled("@ ", Style::default().fg(palette.accent)),
            Span::styled(picker.query.as_str(), Style::default().fg(palette.fg)),
            Span::styled("█", Style::default().fg(palette.accent)),
        ]));

        if picker.matches.is_empty() {
            rows.push(Line::from(Span::styled(
                "  (no matching directories)",
                Style::default().fg(palette.dim),
            )));
        } else {
            let sel = picker.sel.min(picker.matches.len().saturating_sub(1));
            // Window keeps `sel` visible, anchoring to the bottom while scrolling.
            let start = if sel < MAX_VIS { 0 } else { sel + 1 - MAX_VIS };
            let end = (start + MAX_VIS).min(picker.matches.len());
            for (vi, m) in picker.matches[start..end].iter().enumerate() {
                let i = start + vi;
                if i == sel {
                    let hl = Style::default().fg(palette.sel_fg).bg(palette.sel_bg);
                    rows.push(Line::from(Span::styled(format!(" {m} "), hl)));
                } else {
                    rows.push(Line::from(Span::styled(
                        format!(" {m} "),
                        Style::default().fg(palette.fg),
                    )));
                }
            }
        }

        // Title shows position when more entries exist than fit on screen.
        let title = if picker.matches.len() > MAX_VIS {
            format!(" pick directory {}/{} ", picker.sel + 1, picker.matches.len())
        } else {
            " pick directory ".to_string()
        };

        // Centre a compact box over the body; size to content, clamped.
        let body = outer[1];
        let h = ((rows.len() as u16) + 2).min(body.height.max(3));
        let w = body.width.saturating_sub(4).max(10);
        let x = body.x + (body.width.saturating_sub(w)) / 2;
        let y = body.y + (body.height.saturating_sub(h)) / 2;
        let popup = Rect { x, y, width: w, height: h };

        let block = Block::bordered()
            .border_style(Style::default().fg(palette.dim))
            .title(Span::styled(title, Style::default().fg(palette.dim)))
            .padding(Padding::horizontal(1));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        frame.render_widget(Paragraph::new(rows), inner);
    }

    // --- Add-provider modal overlay (rendered last, over everything) ---
    if let Some(modal) = st.prov_modal.as_ref() {
        draw_provider_modal(frame, modal, palette, frame.area());
    }

    // --- Add/edit-model modal overlay (rendered last, over everything) ---
    if let Some(modal) = st.model_modal.as_ref() {
        let or = st.mm_provider_is_openrouter();
        draw_model_modal(frame, st, modal, or, models_cache, palette, frame.area());

        // Role checkbox picker overlay: a modal-on-modal, drawn LAST so it floats
        // over the model modal it belongs to.
        if let Some(picker) = modal.role_picker.as_ref() {
            draw_role_picker(frame, picker, palette, frame.area());
        }
    }
}

/// Render the Role checkbox picker overlay (model EDIT modal) as a bordered
/// modal over a dimmed backdrop.
///
/// Mirrors the `/agents` tool picker (`view/agents.rs::draw_tool_picker`) but
/// SIMPLER — the option set is the fixed [`ModelRole::ALL`] (4 entries), so there
/// is no "type to filter" line. Each role is a `[ ] label` / `[x] label` row; the
/// cursor row carries the inverse highlight. A footer line shows the key hints.
///
/// ```text
/// ┌─ roles ─────────────────┐
/// │ [x] main                │
/// │ [ ] awareness           │
/// │ [ ] safeguard           │
/// │ [ ] compactor           │
/// │ space toggle · enter ok…│
/// └─────────────────────────┘
/// ```
fn draw_role_picker(
    frame: &mut Frame,
    picker: &crate::app::mode::settings::RolePickerState,
    palette: &Palette,
    area: Rect,
) {
    use crate::app::mode::settings::ModelRole;

    let n = ModelRole::ALL.len();
    // Content rows: one per role + two hint lines (split for narrow modals). Borders add 2.
    let content_h = n as u16 + 2;
    let total_h = content_h + 2;
    // Width: "[x] awareness" is short; a 28-col inner is comfortable. Clamp to frame.
    let popup_w = 30_u16.min(area.width.saturating_sub(2));
    let w = popup_w;
    let h = total_h.min(area.height.saturating_sub(2)).max(3);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect { x, y, width: w, height: h };

    // Dim everything outside the modal (fg dim + bg reset — same as the other
    // settings modals, so a stacked overlay still recedes the layer beneath it).
    {
        let buf = frame.buffer_mut();
        for cy in area.top()..area.bottom() {
            for cx in area.left()..area.right() {
                if cx >= popup.x && cx < popup.right() && cy >= popup.y && cy < popup.bottom() {
                    continue;
                }
                buf[(cx, cy)].set_fg(palette.dim).set_bg(Color::Reset);
            }
        }
    }

    let modal_block = Block::bordered()
        .border_style(Style::default().fg(palette.accent))
        .title(Span::styled(" roles ", Style::default().fg(palette.accent)));
    let inner = modal_block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(modal_block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let body_w = inner.width as usize;
    let cursor = picker.cursor.min(n.saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();
    for (i, role) in ModelRole::ALL.iter().enumerate() {
        let checked = picker.checked.get(i).copied().unwrap_or(false);
        let mark = if checked { "[x]" } else { "[ ]" };
        if i == cursor {
            // Cursor row: full-width inverse highlight.
            let text = format!("{} {}", mark, role.label());
            lines.push(Line::from(Span::styled(
                format!("{:<width$}", text, width = body_w),
                Style::default().fg(palette.sel_fg).bg(palette.sel_bg),
            )));
        } else {
            // Checkbox accent when checked, dim when not; label follows the box.
            let box_color = if checked { palette.accent } else { palette.dim };
            lines.push(Line::from(vec![
                Span::styled(mark, Style::default().fg(box_color)),
                Span::styled(
                    format!(" {}", role.label()),
                    Style::default().fg(if checked { palette.fg } else { palette.dim }),
                ),
            ]));
        }
    }

    // Footer hint: two lines so narrow modals don't truncate.
    lines.push(Line::from(Span::styled(
        "space toggle",
        Style::default().fg(palette.dim),
    )));
    lines.push(Line::from(Span::styled(
        "enter ok \u{b7} esc cancel",
        Style::default().fg(palette.dim),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the API Providers interactive screen inside `area`.
///
/// Shows a borderless table (header + one row per provider) and a `[+ add]`
/// button below it. The selected real row is inverse-highlighted; the selected
/// add-button row is also inverse-highlighted. Armed-for-delete rows are
/// prefixed with "DEL? " to signal the pending confirm.
fn draw_providers(
    frame: &mut Frame,
    st: &SettingsState,
    palette: &Palette,
    area: Rect,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Column widths: Name (14), Endpoint (flexible), Type (11), Key (8).
    let col_name_w = 14u16;
    let col_type_w = 11u16;
    let col_key_w  = 8u16;
    let col_ep_w   = area.width.saturating_sub(col_name_w + col_type_w + col_key_w + 3);

    // Header row.
    let header = Row::new(vec![
        Cell::from(Span::styled("Name",     Style::default().fg(palette.dim))),
        Cell::from(Span::styled("Endpoint", Style::default().fg(palette.dim))),
        Cell::from(Span::styled("Type",     Style::default().fg(palette.dim))),
        Cell::from(Span::styled("Key",      Style::default().fg(palette.dim))),
    ]);

    // Data rows.
    let rows: Vec<Row> = st.providers.iter().enumerate().map(|(i, p)| {
        let selected = st.in_detail && i == st.prov_sel && !st.prov_on_add_button();
        let armed    = selected && st.prov_delete_armed;

        let name_str = if armed {
            format!("DEL? {}", if p.name.is_empty() { "\u{2014}" } else { &p.name })
        } else if p.name.is_empty() {
            "\u{2014}".to_string()
        } else {
            p.name.clone()
        };
        let name_str = truncate(&name_str, col_name_w as usize);
        let ep_str   = truncate(&p.endpoint, col_ep_w as usize);
        let type_str = p.api_type.short_label().to_string();
        let key_str  = if p.api_key.is_empty() { "\u{2014}".to_string() } else { "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}".to_string() };

        let row_style = if selected {
            Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
        } else {
            Style::default().fg(palette.fg)
        };

        Row::new(vec![
            Cell::from(name_str),
            Cell::from(ep_str),
            Cell::from(type_str),
            Cell::from(key_str),
        ]).style(row_style)
    }).collect();

    let widths = [
        Constraint::Length(col_name_w),
        Constraint::Min(col_ep_w.max(10)),
        Constraint::Length(col_type_w),
        Constraint::Length(col_key_w),
    ];

    // Height for the table: header (1) + rows; leave 1 row for the add button.
    let table_h = area.height.saturating_sub(1).max(1);
    let table_area = Rect { x: area.x, y: area.y, width: area.width, height: table_h };
    let btn_area   = Rect { x: area.x, y: area.y + table_h, width: area.width, height: 1 };

    let table = Table::new(rows, widths)
        .header(header);
    frame.render_widget(table, table_area);

    // Add-button row.
    let on_btn = st.in_detail && st.prov_on_add_button();
    let btn_style = if on_btn {
        Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
    } else {
        Style::default().fg(palette.accent)
    };
    frame.render_widget(
        Paragraph::new(Span::styled("[ + add provider ]", btn_style)),
        btn_area,
    );
}

/// Render the add-provider modal overlay with a dimmed backdrop.
///
/// Mirrors the `draw_tool_picker` approach from `view/agents.rs`:
/// walk `frame.buffer_mut()` to dim every cell outside the modal rect,
/// then `Clear` + `Block::bordered()` + inner content.
fn draw_provider_modal(
    frame: &mut Frame,
    modal: &ProviderModal,
    palette: &Palette,
    area: Rect,
) {
    // Modal dimensions: ~50 wide, 9 tall (header + 4 rows + blank + save + 2 borders).
    const MODAL_W: u16 = 52;
    const MODAL_H: u16 = 9;
    let w = MODAL_W.min(area.width.saturating_sub(2));
    let h = MODAL_H.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect { x, y, width: w, height: h };

    // Dim everything outside the modal.
    {
        let buf = frame.buffer_mut();
        for cy in area.top()..area.bottom() {
            for cx in area.left()..area.right() {
                if cx >= popup.x && cx < popup.right() && cy >= popup.y && cy < popup.bottom() {
                    continue;
                }
                buf[(cx, cy)].set_fg(palette.dim).set_bg(Color::Reset);
            }
        }
    }

    // Modal box.
    let modal_block = Block::bordered()
        .border_style(Style::default().fg(palette.accent))
        .title(Span::styled(" Add API provider ", Style::default().fg(palette.accent)));
    let inner = modal_block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(modal_block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let label_w = 10usize;
    let val_w   = (inner.width as usize).saturating_sub(label_w + 1).max(4);
    let mut lines: Vec<Line> = Vec::new();

    // Row 0: Name
    {
        let active = modal.field == 0;
        let lc = if active { palette.accent } else { palette.dim };
        let label = Span::styled(format!("{:<width$}", "Name", width = label_w), Style::default().fg(lc));
        let mut val = truncate(&modal.name, val_w.saturating_sub(1));
        if active { val.push('\u{2588}'); }
        let vc = if active { palette.fg } else { palette.dim };
        lines.push(Line::from(vec![label, Span::styled(val, Style::default().fg(vc))]));
    }

    // Row 1: Endpoint
    {
        let active = modal.field == 1;
        let lc = if active { palette.accent } else { palette.dim };
        let label = Span::styled(format!("{:<width$}", "Endpoint", width = label_w), Style::default().fg(lc));
        let mut val = truncate(&modal.endpoint, val_w.saturating_sub(1));
        if active { val.push('\u{2588}'); }
        let vc = if active { palette.fg } else { palette.dim };
        lines.push(Line::from(vec![label, Span::styled(val, Style::default().fg(vc))]));
    }

    // Row 2: API key
    {
        let active = modal.field == 2;
        let lc = if active { palette.accent } else { palette.dim };
        let label = Span::styled(format!("{:<width$}", "API key", width = label_w), Style::default().fg(lc));
        let mut val = truncate(&modal.api_key, val_w.saturating_sub(1));
        if active { val.push('\u{2588}'); }
        let vc = if active { palette.fg } else { palette.dim };
        lines.push(Line::from(vec![label, Span::styled(val, Style::default().fg(vc))]));
    }

    // Blank line.
    lines.push(Line::from(""));

    // Button row: `[ Save ]   [ Cancel ]` centered together.
    // Only the chip text carries the highlight bg; padding uses DEFAULT style so
    // the bg does not bleed across the full modal width.
    let save_text   = "[ Save ]";
    let cancel_text = "[ Cancel ]";
    let gap         = "   ";
    let group_len   = save_text.len() + gap.len() + cancel_text.len();
    let inner_w     = inner.width as usize;
    let pad_left    = inner_w.saturating_sub(group_len) / 2;
    let pad_right   = inner_w.saturating_sub(group_len).saturating_sub(pad_left);
    let save_style = if modal.field == 3 {
        Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
    } else {
        Style::default().fg(palette.accent)
    };
    let cancel_style = if modal.field == 4 {
        Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
    } else {
        Style::default().fg(palette.accent)
    };
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(pad_left)),
        Span::styled(save_text, save_style),
        Span::raw(gap),
        Span::styled(cancel_text, cancel_style),
        Span::raw(" ".repeat(pad_right)),
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the Models Select interactive screen inside `area`.
///
/// Mirrors [`draw_providers`]: a borderless table (header + one row per model)
/// and a `[+ add model]` button below it. The selected real row is inverse-
/// highlighted; an armed-for-delete row is prefixed with "DEL? ".
///
/// Columns: Name (12), Role (11), Model (flexible), Provider (12).
fn draw_models(
    frame: &mut Frame,
    st: &SettingsState,
    palette: &Palette,
    area: Rect,
) {
    use crate::app::mode::settings::ModelRole;

    if area.height == 0 || area.width == 0 {
        return;
    }

    // Column widths: Name (12), Role (11), Model (flexible), Provider (12).
    let col_name_w  = 12u16;
    let col_role_w  = 11u16;
    let col_prov_w  = 12u16;
    let col_model_w = area.width.saturating_sub(col_name_w + col_role_w + col_prov_w + 3);

    // Header row.
    let header = Row::new(vec![
        Cell::from(Span::styled("Name",     Style::default().fg(palette.dim))),
        Cell::from(Span::styled("Role",     Style::default().fg(palette.dim))),
        Cell::from(Span::styled("Model",    Style::default().fg(palette.dim))),
        Cell::from(Span::styled("Provider", Style::default().fg(palette.dim))),
    ]);

    // Data rows.
    let rows: Vec<Row> = st.models.iter().enumerate().map(|(i, m)| {
        let selected = st.in_detail && i == st.model_sel && !st.model_on_add_button();
        let armed    = selected && st.model_delete_armed;

        let name_str = if armed {
            format!("DEL? {}", if m.name.is_empty() { "\u{2014}" } else { &m.name })
        } else if m.name.is_empty() {
            "\u{2014}".to_string()
        } else {
            m.name.clone()
        };
        let name_str  = truncate(&name_str, col_name_w as usize);
        // A model may hold several roles → comma-join their labels (truncated to
        // the column width); an em-dash when it holds none.
        let role_str  = if m.roles.is_empty() {
            "\u{2014}".to_string()
        } else {
            m.roles
                .iter()
                .map(|r: &ModelRole| r.label())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let role_str  = truncate(&role_str, col_role_w as usize);
        let model_str = if m.model_id.is_empty() {
            "\u{2014}".to_string()
        } else {
            truncate(&m.model_id, col_model_w as usize)
        };
        let prov_str = st
            .providers
            .get(m.provider_idx)
            .map(|p| p.name.as_str())
            .filter(|n| !n.is_empty())
            .unwrap_or("\u{2014}");
        let prov_str = truncate(prov_str, col_prov_w as usize);

        let row_style = if selected {
            Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
        } else {
            Style::default().fg(palette.fg)
        };

        Row::new(vec![
            Cell::from(name_str),
            Cell::from(role_str),
            Cell::from(model_str),
            Cell::from(prov_str),
        ]).style(row_style)
    }).collect();

    let widths = [
        Constraint::Length(col_name_w),
        Constraint::Length(col_role_w),
        Constraint::Min(col_model_w.max(10)),
        Constraint::Length(col_prov_w),
    ];

    // Height for the table: header (1) + rows; leave 1 row for the add button.
    let table_h = area.height.saturating_sub(1).max(1);
    let table_area = Rect { x: area.x, y: area.y, width: area.width, height: table_h };
    let btn_area   = Rect { x: area.x, y: area.y + table_h, width: area.width, height: 1 };

    let table = Table::new(rows, widths).header(header);
    frame.render_widget(table, table_area);

    // Add-button row.
    let on_btn = st.in_detail && st.model_on_add_button();
    let btn_style = if on_btn {
        Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
    } else {
        Style::default().fg(palette.accent)
    };
    frame.render_widget(
        Paragraph::new(Span::styled("[ + add model ]", btn_style)),
        btn_area,
    );
}

/// Render the add/edit-model modal overlay with a dimmed backdrop.
///
/// Mirrors [`draw_provider_modal`] (backdrop dim + `Clear` + bordered accent
/// box), but is taller because the Model field hosts a live omnisearch results
/// list when the chosen provider is OpenRouter. `or` is
/// `st.mm_provider_is_openrouter()` (passed in so the borrow isn't recomputed),
/// and `cache` is the model catalogue used to render the results list.
#[allow(clippy::too_many_arguments)]
fn draw_model_modal(
    frame: &mut Frame,
    st: &SettingsState,
    modal: &ModelModal,
    or: bool,
    cache: &[ModelInfo],
    palette: &Palette,
    area: Rect,
) {
    use crate::app::mode::settings::{ModelField, ModelRole};

    // Taller than the provider modal: it hosts a results list.
    // OpenRouter mode adds a separate readout row + search field + rule (2 extra).
    // Edit mode gets one further extra row for the Role field; OpenRouter +
    // model-selected adds a Route label row above the (now selectable) options
    // list. Sized for the taller layout so the 8-row options list never clips.
    //
    // When the search query is EMPTY and a model is selected, the options list
    // renders instead of the results dropdown (they're mutually exclusive). Sized
    // for the taller of the two: a Route label + options header + up to 8 rows + a
    // "+N more" line, which is why this base is generous enough that 8 rows never
    // clip even with the extra Route label row.
    const MODAL_W: u16 = 60;
    const MODAL_H_BASE: u16 = 23;
    let modal_h = if modal.is_edit() { MODAL_H_BASE + 1 } else { MODAL_H_BASE };
    let w = MODAL_W.min(area.width.saturating_sub(2));
    let h = modal_h.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect { x, y, width: w, height: h };

    // Dim everything outside the modal (preserve the bg-reset fix).
    {
        let buf = frame.buffer_mut();
        for cy in area.top()..area.bottom() {
            for cx in area.left()..area.right() {
                if cx >= popup.x && cx < popup.right() && cy >= popup.y && cy < popup.bottom() {
                    continue;
                }
                buf[(cx, cy)].set_fg(palette.dim).set_bg(Color::Reset);
            }
        }
    }

    let title = if modal.editing_idx.is_some() {
        " Edit model "
    } else {
        " Add model "
    };
    let modal_block = Block::bordered()
        .border_style(Style::default().fg(palette.accent))
        .title(Span::styled(title, Style::default().fg(palette.accent)));
    let inner = modal_block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(modal_block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let label_w = 10usize;
    let val_w   = (inner.width as usize).saturating_sub(label_w + 1).max(4);
    let mut lines: Vec<Line> = Vec::new();

    // Resolve the focused field through the computed field list (no hardcoded
    // indices): `focused(f)` is true when the modal's `field` cursor points at
    // `f` in the current layout. Name/Provider/Model are always the first three.
    let fields = st.model_modal_fields();
    let focused = |f: ModelField| fields.get(modal.field).copied() == Some(f);

    // Row: Name.
    {
        let active = focused(ModelField::Name);
        let lc = if active { palette.accent } else { palette.dim };
        let label = Span::styled(format!("{:<width$}", "Name", width = label_w), Style::default().fg(lc));
        let mut val = truncate(&modal.name, val_w.saturating_sub(1));
        if active { val.push('\u{2588}'); }
        let vc = if active { palette.fg } else { palette.dim };
        lines.push(Line::from(vec![label, Span::styled(val, Style::default().fg(vc))]));
    }

    // Row: Provider toggle.
    {
        let active = focused(ModelField::Provider);
        let lc = if active { palette.accent } else { palette.dim };
        let label = Span::styled(format!("{:<width$}", "Provider", width = label_w), Style::default().fg(lc));
        let prov_name = st
            .providers
            .get(modal.provider_idx)
            .map(|p| if p.name.is_empty() { "\u{2014}" } else { p.name.as_str() });
        let toggle_text = match prov_name {
            Some(n) => format!("\u{2039} {} \u{203a}", n),
            None    => "\u{2039} (no providers) \u{203a}".to_string(),
        };
        let tc = if active { palette.accent } else { palette.dim };
        lines.push(Line::from(vec![label, Span::styled(toggle_text, Style::default().fg(tc))]));
    }

    // Row(s): Model.
    // OpenRouter layout:
    //   1. Read-only selected model readout  (label "Model" + model_id / dim placeholder, NO cursor)
    //   2. Search input line                 (indented to value column; query text + cursor when focused)
    //   3. Gray ─ bottom rule                (dim, spans value column width)
    //   4. Results dropdown                  (when query is non-empty)
    //   5. Route label + selectable options  (when query is empty + a model is selected)
    // Non-OpenRouter layout:
    //   1. Plain editable model id           (label "Model" + model_id text + cursor when focused)
    //   2. Gray ─ bottom rule
    {
        let active = focused(ModelField::Model);
        let lc = if active { palette.accent } else { palette.dim };
        let label = Span::styled(format!("{:<width$}", "Model", width = label_w), Style::default().fg(lc));

        if or {
            // --- 1. Selected model readout (read-only, no cursor ever) ---
            if modal.model_id.is_empty() {
                lines.push(Line::from(vec![
                    label,
                    Span::styled("(none selected)", Style::default().fg(palette.dim)),
                ]));
            } else {
                let readout = truncate(&modal.model_id, val_w);
                lines.push(Line::from(vec![
                    label,
                    Span::styled(readout, Style::default().fg(palette.fg)),
                ]));
            }

            // --- 2. Search input line (indented to value column) ---
            {
                let indent = Span::raw(" ".repeat(label_w));
                let search_text = if modal.query.is_empty() {
                    let mut ph = "type to search models\u{2026}".to_string();
                    if active { ph.push('\u{2588}'); }
                    Span::styled(ph, Style::default().fg(palette.dim))
                } else {
                    let mut q = truncate(&modal.query, val_w.saturating_sub(1));
                    if active { q.push('\u{2588}'); }
                    Span::styled(q, Style::default().fg(palette.fg))
                };
                lines.push(Line::from(vec![indent, search_text]));
            }

            // --- 3. Gray bottom rule beneath the search field ---
            {
                let rule_w = val_w.max(1);
                let rule_str = "\u{2500}".repeat(rule_w); // ─ repeated
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(label_w)),
                    Span::styled(rule_str, Style::default().fg(palette.dim)),
                ]));
            }

            // --- 4. Results dropdown (only when query is non-empty) ---
            if !modal.query.is_empty() {
                const MAX_VIS: usize = 8;
                let results = filter_models(cache, &modal.query);
                if results.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  (no matching models)",
                        Style::default().fg(palette.dim),
                    )));
                } else {
                    let sel = modal.result_sel.min(results.len().saturating_sub(1));
                    let start = if sel < MAX_VIS { 0 } else { sel + 1 - MAX_VIS };
                    let end = (start + MAX_VIS).min(results.len());
                    let row_w = inner.width as usize;
                    for (vi, &mi) in results[start..end].iter().enumerate() {
                        let i = start + vi;
                        let info = &cache[mi];
                        let id = info.id.clone();
                        let name = info.name.as_deref().unwrap_or("");
                        if i == sel {
                            let text = if name.is_empty() {
                                format!(" {id} ")
                            } else {
                                format!(" {id}  {name} ")
                            };
                            let text = truncate(&text, row_w);
                            lines.push(Line::from(Span::styled(
                                text,
                                Style::default().fg(palette.sel_fg).bg(palette.sel_bg),
                            )));
                        } else {
                            let id_disp = truncate(&id, row_w.saturating_sub(2));
                            let mut spans = vec![
                                Span::raw(" "),
                                Span::styled(id_disp, Style::default().fg(palette.fg)),
                            ];
                            if !name.is_empty() {
                                let used = 1 + id.chars().count();
                                let rem = row_w.saturating_sub(used + 2);
                                if rem > 1 {
                                    let n = truncate(name, rem);
                                    spans.push(Span::raw("  "));
                                    spans.push(Span::styled(n, Style::default().fg(palette.dim)));
                                }
                            }
                            lines.push(Line::from(spans));
                        }
                    }
                }
            }

            // --- 5. Route field: label row + selectable options list (shown when
            //        the search query is empty so it never stacks under the
            //        results dropdown — the two are mutually exclusive). The
            //        Route field only exists once a model is selected; until then
            //        the same area shows loading / hint states. ---
            if modal.query.is_empty() {
                let row_w = inner.width as usize;
                let route_active = focused(ModelField::Route);

                if !modal.model_id.is_empty() {
                    // Route label row: shows the committed choice (Auto / pinned
                    // provider name). Accent when the Route field is focused.
                    let lc = if route_active { palette.accent } else { palette.dim };
                    let rl = Span::styled(
                        format!("{:<width$}", "Route", width = label_w),
                        Style::default().fg(lc),
                    );
                    let choice = match modal.route.as_deref() {
                        Some(name) if !name.is_empty() => name.to_string(),
                        _ => "Auto (OpenRouter routes)".to_string(),
                    };
                    let vc = if route_active { palette.fg } else { palette.dim };
                    lines.push(Line::from(vec![
                        rl,
                        Span::styled(truncate(&choice, val_w), Style::default().fg(vc)),
                    ]));
                }

                if modal.endpoints_loading {
                    lines.push(Line::from(Span::styled(
                        "loading routes\u{2026}",
                        Style::default().fg(palette.dim),
                    )));
                } else if let Some(eps) = modal.endpoints.as_ref() {
                    if eps.is_empty() {
                        lines.push(Line::from(Span::styled(
                            "no routes for this model",
                            Style::default().fg(palette.dim),
                        )));
                    } else {
                        // The Route option list: row 0 = Auto, rows 1..=N = each
                        // endpoint. `option_count` and the option `sel`/`pinned`
                        // indices line up with the input-layer route handling.
                        let option_count = 1 + eps.len();
                        let sel = modal.route_sel.min(option_count - 1);
                        // Which option is the committed route? Auto (0) when
                        // `route` is None, else the endpoint whose name matches.
                        let pinned: usize = match modal.route.as_deref() {
                            None => 0,
                            Some(name) => eps
                                .iter()
                                .position(|ep| {
                                    ep.provider_name
                                        .as_deref()
                                        .filter(|n| !n.is_empty())
                                        .or(ep.name.as_deref().filter(|n| !n.is_empty()))
                                        == Some(name)
                                })
                                .map(|i| i + 1)
                                .unwrap_or(0),
                        };

                        // Render Auto + up to 8 endpoint rows, windowed to keep
                        // `sel` visible while the Route field is focused.
                        const MAX_EP: usize = 8;
                        // Build the full option label list first (index 0 = Auto).
                        let mut opt_labels: Vec<String> = Vec::with_capacity(option_count);
                        opt_labels.push("Auto (OpenRouter routes)".to_string());
                        for ep in eps.iter() {
                            let name = ep
                                .provider_name
                                .as_deref()
                                .filter(|n| !n.is_empty())
                                .or(ep.name.as_deref().filter(|n| !n.is_empty()))
                                .unwrap_or("\u{2014}");
                            let (prompt, completion) = ep
                                .pricing
                                .as_ref()
                                .map(|p| (p.prompt.as_ref(), p.completion.as_ref()))
                                .unwrap_or((None, None));
                            let price = format!(
                                "{}/{}",
                                price_per_million(prompt),
                                price_per_million(completion),
                            );
                            let uptime = ep
                                .uptime_last_30m
                                .map(|v| format!("{v:.0}%"))
                                .unwrap_or_default();
                            // name left-padded to ~14, then price, then uptime.
                            opt_labels.push(format!("{name:<14} {price}  {uptime}"));
                        }

                        // Window of MAX_EP+1 rows (Auto always counts as a row).
                        const VIS: usize = MAX_EP + 1;
                        let start = if !route_active || sel < VIS {
                            0
                        } else {
                            sel + 1 - VIS
                        };
                        let end = (start + VIS).min(option_count);
                        for (i, label) in opt_labels
                            .iter()
                            .enumerate()
                            .take(end)
                            .skip(start)
                        {
                            // Persistent marker on the committed route regardless
                            // of focus, so the pin is always visible.
                            let marker = if i == pinned { "\u{2023} " } else { "  " };
                            let text = truncate(
                                &format!("{marker}{label}"),
                                row_w,
                            );
                            let style = if route_active && i == sel {
                                // Focused highlight on the cursor row.
                                Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
                            } else if i == pinned {
                                // Committed route stands out in accent even when
                                // focus is elsewhere.
                                Style::default().fg(palette.accent)
                            } else {
                                Style::default().fg(palette.fg)
                            };
                            lines.push(Line::from(Span::styled(text, style)));
                        }
                        if end < option_count {
                            lines.push(Line::from(Span::styled(
                                format!("+{} more", option_count - end),
                                Style::default().fg(palette.dim),
                            )));
                        }
                    }
                } else if !modal.model_id.is_empty() {
                    // Model set but endpoints not loaded yet (e.g. a fetch failed
                    // to even start): a neutral hint rather than a blank gap.
                    lines.push(Line::from(Span::styled(
                        "loading routes\u{2026}",
                        Style::default().fg(palette.dim),
                    )));
                } else {
                    // No model selected yet.
                    lines.push(Line::from(Span::styled(
                        "pick a model to see providers",
                        Style::default().fg(palette.dim),
                    )));
                }
            }
        } else {
            // Non-OpenRouter: plain editable model id with a bottom rule.
            let mut val = truncate(&modal.model_id, val_w.saturating_sub(1));
            if active { val.push('\u{2588}'); }
            let vc = if active { palette.fg } else { palette.dim };
            lines.push(Line::from(vec![label, Span::styled(val, Style::default().fg(vc))]));

            // Gray bottom rule (consistent input affordance).
            let rule_w = val_w.max(1);
            let rule_str = "\u{2500}".repeat(rule_w);
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(label_w)),
                Span::styled(rule_str, Style::default().fg(palette.dim)),
            ]));
        }
    }

    // Row: Role readout (edit mode only). A single labelled summary line — the
    // comma-joined assigned role labels, or "none". Enter on this field opens the
    // Role checkbox picker overlay (the actual multi-select UI). Accent label +
    // fg value when the Role field is focused; dim otherwise.
    if modal.is_edit() {
        let active = focused(ModelField::Role);
        let lc     = if active { palette.accent } else { palette.dim };
        let label  = Span::styled(
            format!("{:<width$}", "Role", width = label_w),
            Style::default().fg(lc),
        );
        let value = if modal.roles.is_empty() {
            "none".to_string()
        } else {
            modal
                .roles
                .iter()
                .map(|r: &ModelRole| r.label())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let vc = if active { palette.fg } else { palette.dim };
        lines.push(Line::from(vec![
            label,
            Span::styled(truncate(&value, val_w), Style::default().fg(vc)),
        ]));
    }

    // Blank line before the buttons.
    lines.push(Line::from(""));

    // Button row: `[ Save ]  [ Save session ]  [ Cancel ]` centered together.
    // Only the chip text carries the highlight bg; inter-chip spacing uses plain
    // style so the background does not bleed across the modal width.
    let save_text    = "[ Save ]";
    let session_text = "[ Save session ]";
    let cancel_text  = "[ Cancel ]";
    let gap          = "  ";
    let group_len    = save_text.len() + gap.len() + session_text.len() + gap.len() + cancel_text.len();
    let inner_w      = inner.width as usize;
    let pad_left     = inner_w.saturating_sub(group_len) / 2;
    let pad_right    = inner_w.saturating_sub(group_len).saturating_sub(pad_left);
    let save_style = if focused(ModelField::Save) {
        Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
    } else {
        Style::default().fg(palette.accent)
    };
    let session_style = if focused(ModelField::SaveSession) {
        Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
    } else {
        Style::default().fg(palette.accent)
    };
    let cancel_style = if focused(ModelField::Cancel) {
        Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
    } else {
        Style::default().fg(palette.accent)
    };
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(pad_left)),
        Span::styled(save_text, save_style),
        Span::raw(gap),
        Span::styled(session_text, session_style),
        Span::raw(gap),
        Span::styled(cancel_text, cancel_style),
        Span::raw(" ".repeat(pad_right)),
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
}
