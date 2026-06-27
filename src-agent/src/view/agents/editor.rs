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

/// Soft word-wrap one logical line (`chars`) into visual segments of at most
/// `wrap_w` cells, returning each segment as a `(start, end)` CHAR-index range
/// into `chars`.
///
/// Greedy: a segment grows until the next char would overflow `wrap_w`, then it
/// breaks at the LAST space that still fits — that space is consumed (it ends a
/// line and is not re-rendered at the start of the next). A single word longer
/// than `wrap_w` (no usable space) HARD-breaks exactly at `wrap_w`. An empty
/// line yields one empty segment `(0, 0)` so it still occupies a visual row.
///
/// `wrap_w` must be `>= 1` (the caller clamps it); every segment width
/// (`end - start`) is then `<= wrap_w`. Works purely on char indices, so it
/// never splits a multi-byte codepoint.
fn wrap_segments(chars: &[char], wrap_w: usize) -> Vec<(usize, usize)> {
    let n = chars.len();
    if n == 0 {
        return vec![(0, 0)];
    }
    let mut segs = Vec::new();
    let mut start = 0;
    while start < n {
        if n - start <= wrap_w {
            // The remainder fits on one line.
            segs.push((start, n));
            break;
        }
        // `limit` is the first char index that does NOT fit on this line; since
        // `n - start > wrap_w` here, `limit < n`, so `chars[limit]` is in range.
        // Scan downward for the rightmost space in `start+1 ..= limit` to break
        // on cleanly (that space ends the line and is consumed on the next row).
        let limit = start + wrap_w;
        let mut brk = None;
        let mut j = limit;
        while j > start {
            if chars[j] == ' ' {
                brk = Some(j);
                break;
            }
            j -= 1;
        }
        match brk {
            Some(j) => {
                segs.push((start, j));
                start = j + 1; // consume the breaking space
            }
            None => {
                // No usable space in the window: hard-break at `wrap_w`.
                segs.push((start, limit));
                start = limit;
            }
        }
    }
    segs
}

/// Render the FULL-SCREEN nano-style text editor over the whole frame.
///
/// `title` is the active field's label (e.g. `"prompt"`, `"description"`,
/// `"conditions"`) shown dim in the header — the same editor serves all three
/// full-size-editable fields. `clear_confirm` arms the Ctrl+X "clear the whole
/// field?" prompt in the footer.
///
/// Layout (minimalist, matching the app's header convention):
///
/// ```text
///  edit prompt
/// ─────────────────────────────────────────  ← dim BOTTOM rule
///   1 You are a focused subagent that wraps…   ← number gutter, then wrapped body
///     onto the next visual row.                ← continuation row: blank gutter
///   2                                          ← empty logical line
///
///  ↑↓←→ move · Enter newline · Ctrl+X clear · Esc save & back ← dim footer
/// ```
///
/// ## Wrapping, gutter, and the cursor
/// Each logical line is SOFT WORD-WRAPPED (see [`wrap_segments`]) to the body
/// width minus a left line-number gutter. The gutter shows the 1-based logical
/// line number (right-aligned, dim) on the FIRST visual row of each line and a
/// blank of equal width on every continuation row, so wrapped text stays
/// aligned. Vertical scroll is by VISUAL rows (not logical lines): the stored
/// `scroll` is a seed, re-clamped each frame around the cursor's visual row so
/// the view stays correct without mutating state (the renderer only borrows
/// `ed`). The real terminal cursor is mapped to its exact wrapped cell by
/// wrapping the cursor's own line with the SAME algorithm and locating `col`
/// within its segment — so the mapping is exact even with multi-byte text.
pub(super) fn draw_field_editor(
    frame: &mut Frame,
    ed: &TextEditorState,
    title: &str,
    clear_confirm: bool,
    palette: &Palette,
) {
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
        Paragraph::new(Span::styled(
            format!("edit {title}"),
            Style::default().fg(palette.dim),
        )),
        header_inner.inner(Margin { horizontal: 2, vertical: 0 }),
    );

    // --- Footer hint (dim), or the Ctrl+X clear-confirm prompt (accent). ---
    let footer_line = if clear_confirm {
        Line::from(Span::styled(
            "clear entire field? y = yes, any key = cancel",
            Style::default().fg(palette.accent),
        ))
    } else {
        Line::from(Span::styled(
            "\u{2191}\u{2193}\u{2190}\u{2192} move \u{b7} Enter newline \u{b7} Ctrl+X clear \u{b7} Esc save & back",
            Style::default().fg(palette.dim),
        ))
    };
    frame.render_widget(
        Paragraph::new(footer_line),
        outer[2].inner(Margin { horizontal: 2, vertical: 0 }),
    );

    // --- Body: horizontal margin so text isn't glued to the edge (chat-style). ---
    let body = outer[1].inner(Margin { horizontal: 2, vertical: 0 });
    if body.width == 0 || body.height == 0 {
        return;
    }
    let body_inner_w = body.width as usize;
    let body_h = body.height as usize;

    // --- Gutter geometry. ---
    // Width = max(3, digits in line count) for the number, plus a 1-col
    // separator. `wrap_w` is whatever body width is left; clamp to >= 1 so a
    // narrow terminal can't produce a zero-width wrap (it just overflows and the
    // Paragraph clips — no panic).
    let digits = ed.lines.len().to_string().len();
    let num_w = digits.max(3);
    let gutter_w = num_w + 1; // number columns + a single separator column
    let wrap_w = body_inner_w.saturating_sub(gutter_w).max(1);

    // Pre-wrap every logical line ONCE: reused for the cursor's visual row, the
    // total visual-row count, and the render below (so the wrap is computed with
    // a single algorithm everywhere → cursor mapping stays exact).
    let line_chars: Vec<Vec<char>> = ed.lines.iter().map(|l| l.chars().collect()).collect();
    let per_line: Vec<Vec<(usize, usize)>> = line_chars
        .iter()
        .map(|chars| wrap_segments(chars, wrap_w))
        .collect();

    // Cursor → absolute VISUAL row + the column offset within its segment.
    // The visual row is (all segments of the lines above `row`) + the index of
    // the segment that holds `col` within `row`. `col`'s segment is the LAST one
    // whose start is <= col; the x offset is `col - seg.start` (a trailing space
    // that was consumed by the wrap lands at the end of its segment, which is a
    // sensible cursor cell).
    let rows_above: usize = per_line[..ed.row].iter().map(|s| s.len()).sum();
    let cur_segs = &per_line[ed.row];
    let mut seg_idx = 0;
    for (i, &(s, _e)) in cur_segs.iter().enumerate() {
        if s <= ed.col {
            seg_idx = i;
        } else {
            break;
        }
    }
    let cursor_vrow = rows_above + seg_idx;
    let cursor_x_off = ed.col - cur_segs[seg_idx].0;

    let total_vrows: usize = per_line.iter().map(|s| s.len()).sum();

    // Vertical scroll in VISUAL-row space: seed from the stored scroll, then
    // clamp it so the cursor's visual row sits inside [v_scroll, v_scroll+body_h).
    let mut v_scroll = ed.scroll.min(total_vrows.saturating_sub(1));
    if cursor_vrow < v_scroll {
        v_scroll = cursor_vrow;
    } else if cursor_vrow >= v_scroll + body_h {
        v_scroll = cursor_vrow + 1 - body_h;
    }

    // Render the visible window of VISUAL rows. Walk every logical line, emit its
    // segments as rows, and keep only those in [v_scroll, v_scroll+body_h). The
    // gutter shows the line number on a line's FIRST visual row, blank after.
    let mut out_lines: Vec<Line> = Vec::with_capacity(body_h);
    let mut vrow = 0usize;
    'outer: for (li, segs) in per_line.iter().enumerate() {
        for (si, &(s, e)) in segs.iter().enumerate() {
            if vrow >= v_scroll + body_h {
                break 'outer;
            }
            if vrow >= v_scroll {
                // Gutter cell: right-aligned number on the first row, else blanks.
                let gutter = if si == 0 {
                    format!("{:>width$} ", li + 1, width = num_w)
                } else {
                    " ".repeat(gutter_w)
                };
                let text: String = line_chars[li][s..e].iter().collect();
                out_lines.push(Line::from(vec![
                    Span::styled(gutter, Style::default().fg(palette.dim)),
                    Span::styled(text, Style::default().fg(palette.fg)),
                ]));
            }
            vrow += 1;
        }
    }
    frame.render_widget(Paragraph::new(out_lines), body);

    // --- Real terminal cursor at the exact mapped cell. ---
    // x = body.x + gutter_w + (col offset within its wrapped segment);
    // y = body.y + (cursor visual row - v_scroll). Both offsets are non-negative
    // by construction. Clamp to the body rect so a narrow terminal never panics.
    let cursor_x = body.x + (gutter_w + cursor_x_off) as u16;
    let cursor_y = body.y + (cursor_vrow.saturating_sub(v_scroll)) as u16;
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

        if f == AgentEditField::Description || f == AgentEditField::Conditions {
            // Full-size editable: Enter opens the nano editor (never inline-edited),
            // so the row is a single-line PREVIEW of the draft's first line — no
            // block cursor — with an "enter edit fullsize" hint when selected.
            let raw = st.draft(f);
            let first = raw.lines().next().unwrap_or("");
            let (shown, color) = if first.is_empty() {
                let ph = if f == AgentEditField::Description {
                    "(required)"
                } else {
                    "(optional — when to delegate)"
                };
                (ph.to_string(), palette.dim)
            } else {
                (truncate(first, value_w), palette.fg)
            };
            let mut row = vec![marker, label, Span::styled(shown, Style::default().fg(color))];
            if selected {
                row.push(Span::styled(
                    "  enter edit fullsize",
                    Style::default().fg(palette.dim),
                ));
            }
            lines.push(Line::from(row));
            continue;
        }

        // Single-line text fields.
        let raw = st.draft(f);
        let (shown, color) = if raw.is_empty() && !editing_here {
            let ph = match f {
                AgentEditField::Name => "(required)",
                AgentEditField::Tools => "(read-only default)",
                AgentEditField::Description
                | AgentEditField::Conditions
                | AgentEditField::Model
                | AgentEditField::Body => "",
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
