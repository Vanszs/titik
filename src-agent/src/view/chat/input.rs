//! Input box rendering: multiline editor with caret, compact animation, and
//! the session-name tab on the top border.

use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use crate::app::state::AppStateRest;
use crate::view::theme::Palette;
use super::helpers::render_compact_anim;

/// Prefix width in columns: "[$] " on line 1, "    " on continuations.
const PREFIX_W: usize = 4;

/// Compute how many inner rows the input box needs.
///
/// Used by the caller to reserve the correct height in the layout split before
/// any widgets are rendered. Capped at 50% of the terminal height so the input
/// can never eat the transcript.
///
/// The input `Block` uses `Borders::TOP | Borders::BOTTOM` only (no side borders)
/// plus `Padding::horizontal(2)`, so inner width = frame_width - 4 (2+2 padding).
/// This matches `render_editor`'s `area.width` from `input_block.inner(chunk)`.
pub(super) fn input_row_count(rest: &AppStateRest, frame_width: u16, frame_height: u16) -> usize {
    let inner_w = (frame_width.saturating_sub(4) as usize).max(1);
    let mut input_rows = 0usize;
    let cursor = rest.cursor;
    let logicals: Vec<&str> = rest.input.split('\n').collect();
    let last_idx = logicals.len().saturating_sub(1);
    let mut char_start = 0usize;
    for (i, logical) in logicals.iter().enumerate() {
        let line_chars = logical.chars().count();
        let on_this_line = if i == last_idx {
            cursor >= char_start && cursor <= char_start + line_chars
        } else {
            cursor >= char_start && cursor < char_start + line_chars + 1
        };
        let total_cols = PREFIX_W + line_chars + if on_this_line { 1 } else { 0 };
        input_rows += 1usize.max(total_cols.div_ceil(inner_w));
        char_start += line_chars + 1;
    }
    if rest.compact_anim_start.is_some() {
        return 2;
    }
    let max_inner = ((frame_height / 2).saturating_sub(2) as usize).max(1);
    input_rows.clamp(1, max_inner)
}

/// Render the input block (borders + either the compact animation or the
/// multiline editor) into `chunk`.
pub(super) fn render_input(frame: &mut Frame, chunk: Rect, rest: &AppStateRest, palette: &Palette) {
    let session_tab: Option<Span<'static>> = rest.fg().session.as_ref().map(|s| {
        Span::styled(
            format!(" {} ", s.name.clone()),
            Style::default().fg(palette.accent),
        )
    });
    let input_block = if let Some(tab) = session_tab {
        Block::new()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(Style::default().fg(palette.dim))
            .title(tab)
            .title_alignment(Alignment::Right)
            .padding(Padding::horizontal(2))
    } else {
        Block::new()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(Style::default().fg(palette.dim))
            .padding(Padding::horizontal(2))
    };
    let input_inner = input_block.inner(chunk);
    frame.render_widget(input_block, chunk);
    if let Some(start) = rest.compact_anim_start {
        render_compact_anim(frame, input_inner, start, palette);
    } else {
        render_editor(frame, input_inner, rest, palette);
    }
}

/// Render the multiline editor (the normal `[$] {input}` state) into `area`.
///
/// Logical lines split on '\n'; the first carries the accent prompt, every
/// continuation a 4-col indent so it hangs under the prompt. A non-blinking
/// block caret is painted AT `rest.cursor`.
///
/// Each logical line is pre-expanded into individual visual rows of exactly
/// `inner_w` columns. This means `Paragraph` never needs to wrap — we pass
/// one `Line` per visual row, so `scroll((row, 0))` maps 1:1 to screen rows
/// and caret tracking is exact.
fn render_editor(frame: &mut Frame, area: Rect, rest: &AppStateRest, palette: &Palette) {
    let inner_w = (area.width as usize).max(1);
    let cursor = rest.cursor;
    let logicals: Vec<&str> = rest.input.split('\n').collect();
    let last_idx = logicals.len().saturating_sub(1);

    // Total wrapped visual lines (uncapped) and caret's wrapped row.
    let mut total_vis: usize = 0;
    let mut caret_vis: usize = 0;
    let mut caret_found = false;

    let mut visual_lines: Vec<Line> = Vec::new();

    // Running char offset accumulator: avoids O(L^2) re-walks from line 0.
    let mut char_start = 0usize;

    for (i, logical) in logicals.iter().enumerate() {
        let line_chars = logical.chars().count();
        let on_this_line = if i == last_idx {
            cursor >= char_start && cursor <= char_start + line_chars
        } else {
            cursor >= char_start && cursor < char_start + line_chars + 1
        };

        let chars: Vec<char> = logical.chars().collect();
        let total_cols = PREFIX_W + line_chars + if on_this_line { 1 } else { 0 };
        let rows = 1usize.max(total_cols.div_ceil(inner_w));

        // Caret position within this logical line (column, prefix included).
        let caret_col_in_line = if on_this_line {
            PREFIX_W + (cursor - char_start).min(line_chars)
        } else {
            usize::MAX
        };

        for row in 0..rows {
            let row_col_start = row * inner_w;       // first column of this visual row
            let row_col_end = ((row + 1) * inner_w).min(total_cols);

            // Does the caret land on this visual row?
            let caret_here = caret_col_in_line >= row_col_start && caret_col_in_line < row_col_end;

            let mut spans: Vec<Span> = Vec::new();

            // Prefix chars that overlap this visual row's column range.
            if row_col_start < PREFIX_W {
                let pfx_start = row_col_start;
                let pfx_end = PREFIX_W.min(row_col_end);
                let pfx = if i == 0 { "[$] " } else { "    " };
                let slice = &pfx[pfx_start..pfx_end];
                if !slice.is_empty() {
                    if i == 0 {
                        spans.push(Span::styled(slice.to_string(), Style::default().fg(palette.accent)));
                    } else {
                        spans.push(Span::raw(slice.to_string()));
                    }
                }
            }

            // Content char index range for this visual row.
            let cont_start = row_col_start.saturating_sub(PREFIX_W);
            let cont_end = row_col_end.saturating_sub(PREFIX_W).min(line_chars);

            if caret_here {
                let caret_cont = caret_col_in_line.saturating_sub(PREFIX_W);
                // before caret
                if cont_start < caret_cont.min(cont_end) {
                    let s: String = chars[cont_start..caret_cont.min(cont_end)].iter().collect();
                    if !s.is_empty() { spans.push(Span::raw(s)); }
                }
                // caret char or block
                if caret_cont < chars.len() {
                    spans.push(Span::styled(
                        chars[caret_cont].to_string(),
                        Style::default().add_modifier(Modifier::REVERSED),
                    ));
                    // after caret
                    let after_start = caret_cont + 1;
                    if after_start < cont_end.min(chars.len()) {
                        let s: String = chars[after_start..cont_end.min(chars.len())].iter().collect();
                        if !s.is_empty() { spans.push(Span::raw(s)); }
                    }
                } else {
                    // cursor at end of content — block █
                    spans.push(Span::styled("\u{2588}", Style::default().fg(palette.accent)));
                }
                caret_vis = total_vis;
                caret_found = true;
            } else if cont_start < cont_end.min(chars.len()) {
                let s: String = chars[cont_start..cont_end.min(chars.len())].iter().collect();
                if !s.is_empty() { spans.push(Span::raw(s)); }
            }

            visual_lines.push(Line::from(spans));
            total_vis += 1;
        }

        char_start += line_chars + 1; // +1 for the '\n'
    }

    if !caret_found {
        caret_vis = total_vis.saturating_sub(1);
    }

    // Scroll-to-caret: keep caret on the last visible row (bottom-anchored).
    // No Wrap needed — each Line is already exactly one visual row.
    let viewport_h = area.height as usize;
    let scroll = if total_vis > viewport_h {
        let desired = caret_vis.saturating_sub(viewport_h.saturating_sub(1));
        let max_scroll = total_vis.saturating_sub(viewport_h);
        (desired.min(max_scroll)) as u16
    } else {
        0
    };

    frame.render_widget(
        Paragraph::new(visual_lines).scroll((scroll, 0)),
        area,
    );
}
