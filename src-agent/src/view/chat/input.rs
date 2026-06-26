//! Input box rendering: multiline editor with caret, compact animation, and
//! the session-name tab on the top border.

use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
    Frame,
};
use crate::app::state::AppStateRest;
use crate::view::theme::Palette;
use super::helpers::render_compact_anim;

/// Compute how many inner rows the input box needs.
///
/// Used by the caller to reserve the correct height in the layout split before
/// any widgets are rendered.
pub(super) fn input_row_count(rest: &AppStateRest, frame_width: u16) -> usize {
    // Inner content width = frame width minus 2 borders and 4 cols of horizontal
    // padding (2 left + 2 right). Logical lines split on '\n'; the first is
    // visually prefixed by the prompt "[$] " (4 cols), continuations by 4 spaces
    // so they hang under the prompt. Each prefixed line wraps to inner_w.
    let inner_w = (frame_width.saturating_sub(2 + 4) as usize).max(1);
    let mut input_rows = 0usize;
    for line in rest.input.split('\n') {
        // 4 cols for the prompt on the first line, 4 for the hanging indent on
        // continuations — both happen to be the same width here.
        let prefixed = line.chars().count() + 4;
        input_rows += 1usize.max(prefixed.div_ceil(inner_w));
    }
    // While compacting, the input box shows the animation instead of the editor;
    // reserve 2 inner rows (spinner line + progress bar) regardless of input text.
    if rest.compact_anim_start.is_some() {
        2
    } else {
        input_rows.clamp(1, 8)
    }
}

/// Render the input block (borders + either the compact animation or the
/// multiline editor) into `chunk`.
pub(super) fn render_input(frame: &mut Frame, chunk: Rect, rest: &AppStateRest, palette: &Palette) {
    // --- Input box / compaction animation --- dim top + bottom borders. The top
    // border carries the session name as a right-aligned tab "┤ {name} ├" styled
    // in palette.accent so it reads as a tab label without boxing the whole widget.
    // While a `/compact` is in flight we replace the input contents with an animated
    // indicator (spinner + elapsed + indeterminate sweep) so the wait is legible;
    // otherwise the normal `[$] {input}` editor is drawn.
    let session_tab: Option<Span<'static>> = rest.session.as_ref().map(|s| {
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
/// block caret is painted AT `rest.cursor` (a char index into the whole input,
/// counting the '\n's), so mid-text edits show the caret in place rather than
/// always at the end.
fn render_editor(frame: &mut Frame, area: Rect, rest: &AppStateRest, palette: &Palette) {
    // Map the flat char-index caret to (logical line, column): walk the lines
    // accumulating their char counts plus 1 per consumed '\n'. The caret sits
    // on the line where `consumed <= cursor <= consumed + line_chars` (the
    // upper bound is the line's end, just before its '\n').
    let mut input_lines: Vec<Line> = Vec::new();
    let cursor = rest.cursor;
    let mut consumed = 0usize; // chars before the current logical line
    let logicals: Vec<&str> = rest.input.split('\n').collect();
    let last_idx = logicals.len().saturating_sub(1);
    for (i, logical) in logicals.iter().enumerate() {
        let line_chars = logical.chars().count();
        // The caret falls on this line when its flat index lands within the
        // line's char span. Use `<=` on the end so an end-of-line caret shows
        // here; for non-final lines the '\n' position belongs to the NEXT line
        // (handled by the `< end` guard) so it isn't drawn twice.
        let on_this_line = if i == last_idx {
            cursor >= consumed && cursor <= consumed + line_chars
        } else {
            cursor >= consumed && cursor < consumed + line_chars + 1
        };
        // Prompt prefix: accent "[$] " on the first line, 4-col hang otherwise.
        let prefix: Span = if i == 0 {
            Span::styled("[$] ", Style::default().fg(palette.accent))
        } else {
            Span::raw("    ")
        };
        let mut spans: Vec<Span> = vec![prefix];
        if on_this_line {
            let col = (cursor - consumed).min(line_chars);
            let before: String = logical.chars().take(col).collect();
            let at: String = logical.chars().nth(col).map(String::from).unwrap_or_default();
            let after: String = logical.chars().skip(col + 1).collect();
            if !before.is_empty() {
                spans.push(Span::raw(before));
            }
            // The caret cell: reverse-video over the char under it, or a solid
            // block when the caret is at end-of-line (no char to invert).
            if at.is_empty() {
                spans.push(Span::styled("█", Style::default().fg(palette.accent)));
            } else {
                spans.push(Span::styled(
                    at,
                    Style::default().add_modifier(Modifier::REVERSED),
                ));
            }
            if !after.is_empty() {
                spans.push(Span::raw(after));
            }
        } else {
            spans.push(Span::raw(*logical));
        }
        input_lines.push(Line::from(spans));
        // Advance past this line's chars plus the '\n' that split consumed.
        consumed += line_chars + 1;
    }
    frame.render_widget(
        Paragraph::new(input_lines).wrap(Wrap { trim: false }),
        area,
    );
}
