//! View – in-app agents management dashboard (Agents mode).
//!
//! Two-pane layout, mirroring `/settings`: a narrow sidebar LISTs every agent
//! (with a source tag); the detail pane on the right shows the selected agent
//! read-only (Browse), an editable field form (Edit/Create), or a confirm
//! prompt (DeleteConfirm). A context-sensitive footer shows key hints.
//!
//! Border convention (strict, matches project rules + `/settings`):
//! - Header: `Borders::BOTTOM` only.
//! - List/detail divider: `Borders::RIGHT` on the list pane.
//! - Footer: plain dim line (no full box anywhere).
//!
//! ```text
//!  agents
//! ─────────────────────────────────────────────────────────
//! │ explore  built-in │  name         my-agent
//! │ general  built-in │  description  Does the thing
//! │ my-agent session  │  model        (inherit)
//!                     │  prompt       You are a focused subagent…
//!
//!  ↑/↓ pick · →/Enter edit · n new · d delete · Esc close
//! ```
//!
//! All draft mutation lives in [`crate::app::mode::AgentsState`]; key handling
//! lives in [`crate::controller::input::handle_agents`].

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::app::mode::agents::{source_label, ProviderPickerState, ToolPickerState};
use crate::app::mode::{filter_models, AgentEditField, AgentSubMode, AgentsState};
use crate::dto::openrouter::ModelInfo;
use crate::model::app_config::AppConfig;
use crate::view::theme::Palette;

/// List (sidebar) column width in terminal columns (includes the RIGHT border).
const SIDEBAR_W: u16 = 26;

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

/// Render the agents dashboard for `st` using the given colour `palette`.
///
/// `config` supplies the API provider catalogue (to resolve a `provider_uuid` to
/// its display name), `settings` the active session's settings (to resolve the
/// "inherit" omnisearch endpoint — the session Main route), `models_cache` the
/// on-demand model catalogue, and `cache_endpoint` the endpoint it was fetched for
/// (so the Model omnisearch can tell "this is my endpoint's catalogue" from "still
/// fetching"). All are threaded down to the detail/editor rows.
///
/// All colours flow through `palette` — no hardcoded `Color::` values.
pub fn draw(
    frame: &mut Frame,
    st: &AgentsState,
    config: &AppConfig,
    settings: Option<&crate::model::settings::Settings>,
    models_cache: &[ModelInfo],
    cache_endpoint: Option<&str>,
    palette: &Palette,
) {
    // Outer vertical zones: header | body | footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header text + BOTTOM border
            Constraint::Min(0),    // list + detail
            Constraint::Length(1), // footer key hints
        ])
        .split(frame.area());

    // --- Header ---
    let header_block = Block::new()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim));
    let header_inner = header_block.inner(outer[0]);
    frame.render_widget(header_block, outer[0]);
    frame.render_widget(
        Paragraph::new(Span::styled("agents", Style::default().fg(palette.dim))),
        header_inner.inner(Margin { horizontal: 2, vertical: 0 }),
    );

    // --- Body: list sidebar + detail pane ---
    let body_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(SIDEBAR_W), // list with RIGHT border as divider
            Constraint::Min(0),            // detail pane
        ])
        .split(outer[1]);

    draw_list(frame, st, palette, body_cols[0]);
    draw_detail(frame, st, config, settings, models_cache, cache_endpoint, palette, body_cols[1]);

    // --- Footer ---
    // Full-width inverse status bar: background fills the entire footer line
    // edge to edge; text is left-padded by 1 space so it doesn't touch the edge.
    let footer_rect = outer[2];
    if footer_rect.width > 0 {
        let hint = footer_hint(st, config, settings);
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

    // --- Tool picker overlay (rendered on top of everything else) ---
    if let Some(picker) = &st.tool_picker {
        draw_tool_picker(frame, picker, palette, frame.area());
    }

    // --- Provider picker overlay (rendered last; only one modal open at a time) ---
    if let Some(picker) = &st.provider_picker {
        draw_provider_picker(frame, picker, palette, frame.area());
    }
}

/// Resolve a `provider_uuid` to a human-readable display name for the editor /
/// browse rows: `None` → "inherit session"; a known uuid → its provider name
/// (falling back to the endpoint, or the raw uuid if the connection is gone);
/// an unknown uuid → a short "(unknown provider)" note so a dangling reference
/// is visible rather than silently blank.
fn provider_display_name(config: &AppConfig, provider_uuid: &Option<String>) -> String {
    match provider_uuid {
        None => "inherit session".to_string(),
        Some(uuid) => match config.providers.iter().find(|p| &p.uuid == uuid) {
            Some(p) if !p.name.trim().is_empty() => p.name.clone(),
            Some(p) if !p.endpoint.trim().is_empty() => p.endpoint.clone(),
            Some(_) => uuid.clone(),
            None => "(unknown provider)".to_string(),
        },
    }
}

/// Compute a centered overlay `Rect` with the given width and height,
/// clamped to the available area.
fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect { x, y, width: w, height: h }
}

/// Render the tool multi-select picker overlay as a proper bordered modal.
///
/// Visual structure (Borders::ALL box, backdrop dimmed):
///
/// ```text
/// ┌─ tools (N selected) ────────────┐
/// │ type to filter                  │
/// │ [x] read                        │
/// │ [ ] grep                        │
/// │ …                               │
/// │ space toggle · enter ok · esc   │
/// └─────────────────────────────────┘
/// ```
fn draw_tool_picker(
    frame: &mut Frame,
    picker: &ToolPickerState,
    palette: &Palette,
    area: Rect,
) {
    let filtered = picker.filtered_indices();
    // Content rows: filter line (1) + options (min 1, max 10) + hint (2 lines, split for narrow modals).
    let opt_rows = filtered.len().clamp(1, 10) as u16;
    let content_h = 1 + opt_rows + 2; // filter + options + hint (2 lines)
    // Total height includes top and bottom borders.
    let total_h = content_h + 2;
    // Width: content is "[x] toolname" with 1-space left pad + padding.
    // 36 inner chars + 2 borders = 38 total, clamped to frame.
    let popup_w = 38_u16.min(area.width.saturating_sub(2));
    let popup = centered_rect(area, popup_w, total_h);

    // --- Dim the backdrop (everything outside the modal rect). ---
    // We mutate the frame buffer directly: for each cell not inside the modal,
    // set its foreground to palette.dim so the background recedes.
    {
        let buf = frame.buffer_mut();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                // Skip cells that are inside (or on the border of) the modal.
                if x >= popup.x && x < popup.right() && y >= popup.y && y < popup.bottom() {
                    continue;
                }
                buf[(x, y)].set_fg(palette.dim);
            }
        }
    }

    // --- Modal box: Clear → Block (Borders::ALL) → inner content. ---
    let n_checked = picker.checked.iter().filter(|&&c| c).count();
    let title = if n_checked > 0 {
        format!(" tools ({n_checked} selected) ")
    } else {
        " tools ".to_string()
    };
    let modal_block = Block::bordered()
        .border_style(Style::default().fg(palette.dim))
        .title(Span::styled(title, Style::default().fg(palette.accent)));
    let inner = modal_block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(modal_block, popup);

    // Bail out if the inner area is too small to render content.
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let body_w = inner.width;
    let body_x = inner.x;

    // Filter line (top row of inner area).
    let filter_text = if picker.filter.is_empty() {
        format!("{:<width$}", "type to filter", width = body_w as usize)
    } else {
        let shown = format!("{}█", picker.filter);
        format!("{:<width$}", shown, width = body_w as usize)
    };
    let filter_color = if picker.filter.is_empty() { palette.dim } else { palette.fg };
    frame.render_widget(
        Paragraph::new(Span::styled(filter_text, Style::default().fg(filter_color))),
        Rect { x: body_x, y: inner.y, width: body_w, height: 1 },
    );

    // Option rows.
    let cursor = picker.cursor.min(filtered.len().saturating_sub(1));
    let opt_area_y = inner.y + 1;
    // Scroll so the cursor row is always visible.
    let scroll = cursor.saturating_sub((opt_rows as usize).saturating_sub(1));

    let mut lines: Vec<Line> = Vec::new();
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no matches)",
            Style::default().fg(palette.dim),
        )));
    } else {
        for (fi, &oi) in filtered.iter().enumerate() {
            let mark = if picker.checked[oi] { "[x]" } else { "[ ]" };
            let label = format!("{} {}", mark, picker.options[oi]);
            if fi == cursor {
                lines.push(Line::from(Span::styled(
                    format!("{:<width$}", label, width = body_w as usize),
                    Style::default().fg(palette.sel_fg).bg(palette.sel_bg),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    label,
                    Style::default().fg(palette.accent),
                )));
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines).scroll((scroll as u16, 0)),
        Rect { x: body_x, y: opt_area_y, width: body_w, height: opt_rows },
    );

    // Hint lines (last two rows of inner area): split for narrow modals.
    let hint_y = opt_area_y + opt_rows;
    frame.render_widget(
        Paragraph::new(Span::styled("space toggle", Style::default().fg(palette.dim))),
        Rect { x: body_x, y: hint_y, width: body_w, height: 1 },
    );
    frame.render_widget(
        Paragraph::new(Span::styled("enter ok \u{b7} esc cancel", Style::default().fg(palette.dim))),
        Rect { x: body_x, y: hint_y + 1, width: body_w, height: 1 },
    );
}

/// Render the single-select API-provider picker overlay as a bordered modal.
///
/// Mirrors [`draw_tool_picker`] (dimmed backdrop + `Clear` + accent bordered
/// box + footer hint) but it is a PICK-ONE list, so there are no checkboxes and
/// no filter line: each option is a plain row, the cursor row carries the
/// inverse highlight, and a `›` accent marker flags the cursor for clarity.
///
/// ```text
/// ┌─ api provider ──────────────────┐
/// │ › inherit session               │
/// │   OpenRouter                    │
/// │   Local llama                   │
/// │ ↑↓ select · enter ok · esc …    │
/// └─────────────────────────────────┘
/// ```
fn draw_provider_picker(
    frame: &mut Frame,
    picker: &ProviderPickerState,
    palette: &Palette,
    area: Rect,
) {
    // Content rows: options (min 1, max 10) + two hint lines (split for narrow modals). Borders add 2.
    let opt_rows = picker.options.len().clamp(1, 10) as u16;
    let content_h = opt_rows + 2;
    let total_h = content_h + 2;
    let popup_w = 40_u16.min(area.width.saturating_sub(2));
    let popup = centered_rect(area, popup_w, total_h);

    // Dim the backdrop (fg dim + bg reset, like the settings modals so a stacked
    // layer still recedes).
    {
        let buf = frame.buffer_mut();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if x >= popup.x && x < popup.right() && y >= popup.y && y < popup.bottom() {
                    continue;
                }
                buf[(x, y)].set_fg(palette.dim).set_bg(Color::Reset);
            }
        }
    }

    let modal_block = Block::bordered()
        .border_style(Style::default().fg(palette.accent))
        .title(Span::styled(" api provider ", Style::default().fg(palette.accent)));
    let inner = modal_block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(modal_block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let body_w = inner.width;
    let body_x = inner.x;
    let cursor = picker.cursor.min(picker.options.len().saturating_sub(1));
    // Scroll so the cursor row stays visible.
    let scroll = cursor.saturating_sub((opt_rows as usize).saturating_sub(1));

    let lines: Vec<Line> = picker
        .options
        .iter()
        .enumerate()
        .map(|(i, (_, name))| {
            let label = format!("{} {}", if i == cursor { "›" } else { " " }, name);
            if i == cursor {
                Line::from(Span::styled(
                    format!("{:<width$}", label, width = body_w as usize),
                    Style::default().fg(palette.sel_fg).bg(palette.sel_bg),
                ))
            } else {
                Line::from(Span::styled(label, Style::default().fg(palette.fg)))
            }
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).scroll((scroll as u16, 0)),
        Rect { x: body_x, y: inner.y, width: body_w, height: opt_rows },
    );

    // Hint lines (last two inner rows): split for narrow modals.
    let hint_y = inner.y + opt_rows;
    frame.render_widget(
        Paragraph::new(Span::styled(
            "\u{2191}\u{2193} select",
            Style::default().fg(palette.dim),
        )),
        Rect { x: body_x, y: hint_y, width: body_w, height: 1 },
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            "enter ok \u{b7} esc cancel",
            Style::default().fg(palette.dim),
        )),
        Rect { x: body_x, y: hint_y + 1, width: body_w, height: 1 },
    );
}

/// Render the LIST pane: one row per agent (`name` + source tag), RIGHT border.
fn draw_list(
    frame: &mut Frame,
    st: &AgentsState,
    palette: &Palette,
    area: ratatui::layout::Rect,
) {
    let block = Block::new()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(palette.dim));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content = inner.inner(Margin { horizontal: 1, vertical: 1 });
    // Focus lives in the LIST only while Browsing and not in the detail pane.
    let list_focused = st.mode == AgentSubMode::Browse && !st.in_detail;

    let lines: Vec<Line> = if st.agents.is_empty() {
        vec![Line::from(Span::styled(
            "(no agents)",
            Style::default().fg(palette.dim),
        ))]
    } else {
        let name_w = (content.width as usize).saturating_sub(12).max(4);
        st.agents
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let selected = i == st.list_sel;
                let (marker, color) = if selected {
                    let c = if list_focused { palette.accent } else { palette.dim };
                    ("› ", c)
                } else {
                    ("  ", palette.dim)
                };
                let name = truncate(&a.name, name_w);
                Line::from(vec![
                    Span::styled(marker, Style::default().fg(color)),
                    Span::styled(format!("{name:<width$}", width = name_w), Style::default().fg(color)),
                    Span::styled(" ", Style::default()),
                    Span::styled(source_label(a.source), Style::default().fg(palette.dim)),
                ])
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), content);
}

/// Render the DETAIL pane based on the active sub-mode.
#[allow(clippy::too_many_arguments)]
fn draw_detail(
    frame: &mut Frame,
    st: &AgentsState,
    config: &AppConfig,
    settings: Option<&crate::model::settings::Settings>,
    models_cache: &[ModelInfo],
    cache_endpoint: Option<&str>,
    palette: &Palette,
    area: ratatui::layout::Rect,
) {
    let inner = area.inner(Margin { horizontal: 2, vertical: 1 });
    let lines = match st.mode {
        AgentSubMode::Browse => browse_lines(st, config, palette, inner.width as usize),
        AgentSubMode::Edit | AgentSubMode::Create => editor_lines(
            st,
            config,
            settings,
            models_cache,
            cache_endpoint,
            palette,
            inner.width as usize,
        ),
        AgentSubMode::DeleteConfirm => delete_lines(st, palette),
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Detail rows for Browse: the selected agent's metadata + a body preview.
fn browse_lines<'a>(
    st: &'a AgentsState,
    config: &AppConfig,
    palette: &Palette,
    width: usize,
) -> Vec<Line<'a>> {
    let Some(a) = st.current_agent() else {
        return vec![Line::from(Span::styled(
            "no agent selected",
            Style::default().fg(palette.dim),
        ))];
    };
    let value_w = width.saturating_sub(14).max(4);
    let mut lines = Vec::new();

    let row = |label: &str, value: String, color: Color| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{label:<14}"), Style::default().fg(palette.dim)),
            Span::styled(value, Style::default().fg(color)),
        ])
    };

    lines.push(row("name", a.name.clone(), palette.accent));
    lines.push(row("source", source_label(a.source).to_string(), palette.fg));
    lines.push(row(
        "description",
        truncate(&a.description, value_w),
        palette.fg,
    ));
    lines.push(row(
        "model",
        match &a.model {
            Some(m) => truncate(m, value_w),
            None => "(inherit)".to_string(),
        },
        if a.model.is_some() { palette.fg } else { palette.dim },
    ));
    // Provider is the chosen API provider connection (resolved to its name);
    // None = inherit the session provider, shown dim.
    lines.push(row(
        "provider",
        truncate(&provider_display_name(config, &a.provider_uuid), value_w),
        if a.provider_uuid.is_some() { palette.fg } else { palette.dim },
    ));
    let tools = if a.tools.is_empty() {
        "(read-only default)".to_string()
    } else {
        truncate(&a.tools.join(", "), value_w)
    };
    lines.push(row(
        "tools",
        tools,
        if a.tools.is_empty() { palette.dim } else { palette.fg },
    ));

    // Body preview: a label row, then the first few prompt lines, dim.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "prompt",
        Style::default().fg(palette.dim),
    )));
    let preview_w = width.saturating_sub(2).max(4);
    for raw in a.prompt.lines().take(8) {
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(raw, preview_w)),
            Style::default().fg(palette.fg),
        )));
    }
    lines
}

/// Detail rows for Edit / Create: one labelled draft field per row.
#[allow(clippy::too_many_arguments)]
fn editor_lines<'a>(
    st: &'a AgentsState,
    config: &AppConfig,
    settings: Option<&crate::model::settings::Settings>,
    models_cache: &[ModelInfo],
    cache_endpoint: Option<&str>,
    palette: &Palette,
    width: usize,
) -> Vec<Line<'a>> {
    let value_w = width.saturating_sub(16).max(4);
    let mut lines = Vec::new();
    // The endpoint+key the Model omnisearch fetches against (chosen provider, or
    // inherited session Main); `None` → the Model field is a plain text box.
    let omni_conn = settings.and_then(|st_settings| st.model_omnisearch_conn(config, st_settings));
    // Whether the Model field is a live omnisearch (drives both the Model row
    // rendering and whether Enter opens a search vs. plain edit).
    let model_omni = omni_conn.is_some();
    // Does the cache hold THIS endpoint's catalogue? (still fetching otherwise)
    let cache_matches = omni_conn
        .as_ref()
        .map(|(ep, _)| cache_endpoint == Some(ep.as_str()))
        .unwrap_or(false);

    // Create shows the chosen scope on its own (non-editing) top row.
    if st.mode == AgentSubMode::Create {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(format!("{:<14}", "scope"), Style::default().fg(palette.dim)),
            Span::styled(st.create_scope.label(), Style::default().fg(palette.accent)),
            Span::styled("  (←/→ toggle)", Style::default().fg(palette.dim)),
        ]));
    }

    for &f in st.fields() {
        let selected = f == st.field;
        let editing_here = st.editing && selected;
        let marker = Span::styled(
            if selected { "› " } else { "  " },
            Style::default().fg(palette.accent),
        );
        let label_color = if selected { palette.accent } else { palette.dim };
        let label = Span::styled(
            format!("{:<14}", f.label()),
            Style::default().fg(label_color),
        );

        if f == AgentEditField::Body {
            // Body label row, then the multiline draft beneath it. The active
            // line carries a block cursor while editing this field.
            let mut header = vec![marker, label];
            if editing_here {
                header.push(Span::styled("(editing)", Style::default().fg(palette.dim)));
            }
            lines.push(Line::from(header));
            let body_w = width.saturating_sub(2).max(4);
            let body = &st.draft_body;
            let body_lines: Vec<&str> = if body.is_empty() {
                vec![""]
            } else {
                body.lines().collect()
            };
            let last = body_lines.len().saturating_sub(1);
            for (i, bl) in body_lines.iter().enumerate() {
                let mut text = truncate(bl, body_w);
                if editing_here && i == last {
                    text.push('█');
                }
                lines.push(Line::from(Span::styled(
                    format!("  {text}"),
                    Style::default().fg(palette.fg),
                )));
            }
            continue;
        }

        if f == AgentEditField::Provider {
            // Provider is a SELECTION (resolved name), not free text. Enter opens
            // the picker; the row just shows the current choice. None → dim
            // "inherit session"; a chosen provider → its name in fg.
            let chosen = st.draft_provider_uuid.is_some();
            let name = provider_display_name(config, &st.draft_provider_uuid);
            let color = if chosen { palette.fg } else { palette.dim };
            let mut row = vec![marker, label, Span::styled(truncate(&name, value_w), Style::default().fg(color))];
            if selected {
                row.push(Span::styled("  enter pick", Style::default().fg(palette.dim)));
            }
            lines.push(Line::from(row));
            continue;
        }

        if f == AgentEditField::Model && model_omni {
            // Live catalogue omnisearch (any provider with an endpoint). While
            // searching this field, the row hosts a live query box and a results
            // dropdown beneath it; otherwise it shows the committed model id (or an
            // "(inherit)" placeholder).
            if editing_here {
                let mut q = truncate(&st.model_query, value_w.saturating_sub(1));
                q.push('█');
                let qcolor = if st.model_query.is_empty() { palette.dim } else { palette.fg };
                lines.push(Line::from(vec![
                    marker,
                    label,
                    Span::styled(q, Style::default().fg(qcolor)),
                ]));

                // Results dropdown: up to 8 catalogue hits, the selected row
                // inverse-highlighted. The catalogue is fetched on demand for the
                // resolved endpoint: until the cache holds it (`cache_matches`),
                // show `searching models…`; an empty-query box shows the search
                // hint; a fetched-empty / no-match set shows `no models — type an id`
                // (the raw-query fallback still lets Enter commit).
                const MAX_VIS: usize = 8;
                let results = if cache_matches {
                    filter_models(models_cache, &st.model_query)
                } else {
                    Vec::new()
                };
                if !cache_matches {
                    lines.push(Line::from(Span::styled(
                        "  searching models…",
                        Style::default().fg(palette.dim),
                    )));
                } else if st.model_query.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  type to search models…",
                        Style::default().fg(palette.dim),
                    )));
                } else if results.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  no models — type an id",
                        Style::default().fg(palette.dim),
                    )));
                } else {
                    let sel = st.model_result_sel.min(results.len() - 1);
                    let start = if sel < MAX_VIS { 0 } else { sel + 1 - MAX_VIS };
                    let end = (start + MAX_VIS).min(results.len());
                    let row_w = width.saturating_sub(2).max(4);
                    for (vi, &mi) in results[start..end].iter().enumerate() {
                        let i = start + vi;
                        let id = models_cache[mi].id.clone();
                        if i == sel {
                            let text = truncate(&format!(" {id} "), row_w);
                            lines.push(Line::from(Span::styled(
                                text,
                                Style::default().fg(palette.sel_fg).bg(palette.sel_bg),
                            )));
                        } else {
                            lines.push(Line::from(Span::styled(
                                format!("  {}", truncate(&id, row_w.saturating_sub(2))),
                                Style::default().fg(palette.fg),
                            )));
                        }
                    }
                }
            } else {
                // Not searching: show the committed model id (or inherit hint).
                let (shown, color) = if st.draft_model.is_empty() {
                    ("(inherit)".to_string(), palette.dim)
                } else {
                    (truncate(&st.draft_model, value_w), palette.fg)
                };
                let mut row = vec![marker, label, Span::styled(shown, Style::default().fg(color))];
                if selected {
                    row.push(Span::styled("  enter search", Style::default().fg(palette.dim)));
                }
                lines.push(Line::from(row));
            }
            continue;
        }

        // Single-line text fields.
        let raw = st.draft(f);
        let (shown, color) = if raw.is_empty() && !editing_here {
            let ph = match f {
                AgentEditField::Name => "(required)",
                AgentEditField::Description => "(required)",
                AgentEditField::Model => "(inherit)",
                AgentEditField::Provider => "(default)",
                AgentEditField::Tools => "(read-only default)",
                AgentEditField::Body => "",
            };
            (ph.to_string(), palette.dim)
        } else {
            let trunc_w = if editing_here { value_w.saturating_sub(1) } else { value_w };
            let mut s = truncate(raw, trunc_w);
            if editing_here {
                s.push('█');
            }
            (s, palette.fg)
        };
        lines.push(Line::from(vec![
            marker,
            label,
            Span::styled(shown, Style::default().fg(color)),
        ]));
    }
    lines
}

/// Detail rows for DeleteConfirm: a one-line `y`/`n` prompt.
fn delete_lines<'a>(st: &'a AgentsState, palette: &Palette) -> Vec<Line<'a>> {
    let name = st
        .current_agent()
        .map(|a| a.name.as_str())
        .unwrap_or("?");
    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("delete ", Style::default().fg(palette.fg)),
            Span::styled(format!("'{name}'"), Style::default().fg(palette.accent)),
            Span::styled("?", Style::default().fg(palette.fg)),
        ]),
        Line::from(Span::styled(
            "this removes the file from disk",
            Style::default().fg(palette.dim),
        )),
    ]
}

/// Context-sensitive footer hint for the active sub-mode.
///
/// `config`/`settings` let the Edit/Create hints reflect the provider picker, the
/// Provider field (pick), and the Model field's omnisearch — all of which depend on
/// the chosen provider (or the inherited session Main endpoint).
fn footer_hint(
    st: &AgentsState,
    config: &AppConfig,
    settings: Option<&crate::model::settings::Settings>,
) -> &'static str {
    // Provider picker owns input while open (deepest modal).
    if st.provider_picker.is_some() {
        return "↑↓ select · enter ok · esc cancel";
    }

    let model_field = st.field == AgentEditField::Model;
    let model_or = model_field
        && settings
            .map(|s| st.model_field_omnisearchable(config, s))
            .unwrap_or(false);

    match st.mode {
        AgentSubMode::DeleteConfirm => "y delete · n/Esc cancel",
        AgentSubMode::Create | AgentSubMode::Edit => {
            if st.editing {
                if model_or {
                    "type to search · ↑↓ pick · enter ok · esc cancel"
                } else {
                    "type to edit · Ctrl+J newline (prompt) · Enter/Esc done"
                }
            } else if st.field == AgentEditField::Provider {
                "enter pick provider · ↑/↓ field · esc cancel"
            } else if model_or {
                "enter search model · ↑/↓ field · esc cancel"
            } else if st.mode == AgentSubMode::Create {
                // Keep the scope-toggle affordance for Create's field nav.
                "↑/↓ field · ←/→ scope · Enter edit · s create · Esc cancel"
            } else {
                "↑/↓ field · Enter edit · s save · Esc cancel"
            }
        }
        AgentSubMode::Browse => "↑/↓ pick · →/Enter edit · n new · d delete · Esc close",
    }
}
