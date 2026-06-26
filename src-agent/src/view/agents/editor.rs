//! Full-screen prompt editor and edit/create/delete detail rows.

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Position},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::app::mode::editor::TextEditorState;
use crate::app::mode::{AgentEditField, AgentSubMode, AgentsState};
use crate::model::app_config::AppConfig;
use crate::model::settings::Settings;
use crate::view::theme::Palette;

use super::{model_display, truncate};

/// Render the FULL-SCREEN nano-style prompt editor over the whole frame.
///
/// Layout (minimalist, matching the app's header convention):
///
/// ```text
///  edit prompt
/// ─────────────────────────────────────────  ← dim BOTTOM rule
///   You are a focused subagent…             ← body (clipped, h-scrolled)
///   …                                        ← real terminal cursor sits here
///
///  ↑↓←→ move · Enter newline · Esc save & back ← dim footer
/// ```
///
/// ## Wrapping vs cursor placement
/// We DELIBERATELY do not soft-wrap. Each logical line is HARD-CLIPPED to the
/// inner width, and the body scrolls BOTH ways — vertically by whole lines and
/// horizontally by chars — so the cursor cell is always on screen. This keeps the
/// real terminal cursor's `(x, y)` EXACT (column maps 1:1 to a screen cell), which
/// soft-wrapping would make fragile. Correctness over fanciness, per the spec.
///
/// The stored `scroll` is treated as a seed: the effective vertical scroll is
/// recomputed every frame from the cursor and body height, so the view stays
/// correct without mutating state (the renderer only borrows `ed`).
pub(super) fn draw_prompt_editor(frame: &mut Frame, ed: &TextEditorState, palette: &Palette) {
    let area = frame.area();

    // Header (title + BOTTOM rule) | body | footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // title line + BOTTOM border
            Constraint::Min(0),    // editable body
            Constraint::Length(1), // footer hint
        ])
        .split(area);

    // --- Header: "edit prompt" (dim) with a BOTTOM rule. ---
    let header_block = Block::new()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim));
    let header_inner = header_block.inner(outer[0]);
    frame.render_widget(header_block, outer[0]);
    frame.render_widget(
        Paragraph::new(Span::styled("edit prompt", Style::default().fg(palette.dim))),
        header_inner.inner(Margin { horizontal: 2, vertical: 0 }),
    );

    // --- Footer hint (dim). ---
    frame.render_widget(
        Paragraph::new(Span::styled(
            "\u{2191}\u{2193}\u{2190}\u{2192} move \u{b7} Enter newline \u{b7} Esc save & back",
            Style::default().fg(palette.dim),
        )),
        outer[2].inner(Margin { horizontal: 2, vertical: 0 }),
    );

    // --- Body: horizontal margin so text isn't glued to the edge (chat-style). ---
    let body = outer[1].inner(Margin { horizontal: 2, vertical: 0 });
    if body.width == 0 || body.height == 0 {
        return;
    }
    let inner_w = body.width as usize;
    let body_h = body.height as usize;

    // Vertical scroll: keep the cursor row inside [scroll, scroll + body_h).
    // Seed from the stored scroll, then clamp it around the cursor.
    let mut v_scroll = ed.scroll.min(ed.lines.len().saturating_sub(1));
    if ed.row < v_scroll {
        v_scroll = ed.row;
    } else if ed.row >= v_scroll + body_h {
        v_scroll = ed.row + 1 - body_h;
    }

    // Horizontal scroll: keep the cursor column inside [h_scroll, h_scroll+inner_w).
    // Recomputed from 0 each frame (no stored horizontal offset) → always exact.
    let h_scroll = if ed.col >= inner_w {
        ed.col + 1 - inner_w
    } else {
        0
    };

    // Render the visible window: each logical line hard-clipped to the h-window.
    let mut lines: Vec<Line> = Vec::with_capacity(body_h);
    for li in v_scroll..(v_scroll + body_h).min(ed.lines.len()) {
        let chars: Vec<char> = ed.lines[li].chars().collect();
        let slice: String = if h_scroll < chars.len() {
            chars[h_scroll..(h_scroll + inner_w).min(chars.len())]
                .iter()
                .collect()
        } else {
            String::new()
        };
        lines.push(Line::from(Span::styled(slice, Style::default().fg(palette.fg))));
    }
    frame.render_widget(Paragraph::new(lines), body);

    // --- Real terminal cursor at the exact mapped cell. ---
    // x = body.x + (col - h_scroll); y = body.y + (row - v_scroll). Both offsets
    // are non-negative by construction (the scroll math above guarantees the
    // cursor is inside the window). Clamp to the body rect for safety.
    let cursor_x = body.x + (ed.col.saturating_sub(h_scroll)) as u16;
    let cursor_y = body.y + (ed.row.saturating_sub(v_scroll)) as u16;
    frame.set_cursor_position(Position {
        x: cursor_x.min(body.right().saturating_sub(1)),
        y: cursor_y.min(body.bottom().saturating_sub(1)),
    });
}

/// Detail rows for Edit / Create: one labelled draft field per row.
pub(super) fn editor_lines<'a>(
    st: &'a AgentsState,
    config: &AppConfig,
    settings: Option<&Settings>,
    palette: &Palette,
    width: usize,
) -> Vec<Line<'a>> {
    let value_w = width.saturating_sub(16).max(4);
    let mut lines = Vec::new();

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

        if f == AgentEditField::Model {
            // Model is a SELECTION over the registered models, not free text.
            // Enter opens the picker; the row shows the current choice resolved to
            // `name @ provider` (or a dim "(inherit main)").
            let (text, chosen) =
                model_display(config, settings, &st.draft_model_uuid, &st.draft_model_legacy);
            let color = if chosen { palette.fg } else { palette.dim };
            let mut row = vec![
                marker,
                label,
                Span::styled(truncate(&text, value_w), Style::default().fg(color)),
            ];
            if selected {
                row.push(Span::styled("  enter pick", Style::default().fg(palette.dim)));
            }
            lines.push(Line::from(row));
            continue;
        }

        // Single-line text fields.
        let raw = st.draft(f);
        let (shown, color) = if raw.is_empty() && !editing_here {
            let ph = match f {
                AgentEditField::Name => "(required)",
                AgentEditField::Description => "(required)",
                AgentEditField::Tools => "(read-only default)",
                AgentEditField::Model | AgentEditField::Body => "",
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
pub(super) fn delete_lines<'a>(st: &'a AgentsState, palette: &Palette) -> Vec<Line<'a>> {
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
