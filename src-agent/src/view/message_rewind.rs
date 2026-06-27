//! View – message-rewind picker (MessageRewind mode).
//!
//! Displays the conversation's prior user messages, NEWEST-FIRST, so the user
//! can pick one to rewind to and edit. Layout (top to bottom):
//!
//! 1. Top+bottom rule title bar — ` edit a previous message ` on the TOP rule.
//! 2. Flat (borderless) message list — one truncated preview line per entry.
//!    The selected row is highlighted with `palette.sel_fg` on `palette.sel_bg`.
//!    The list scrolls to keep the selection visible.
//! 3. One-line keybinding hint.
//!
//! Selection state lives in [`app::mode::RewindState`]. Keystroke handling lives
//! in [`controller::input::handle_rewind`].

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use crate::app::mode::RewindState;
use crate::view::theme::Palette;

/// Collapse a message to a single line and truncate it to at most `max` Unicode
/// scalar values, appending `…` if cut. Newlines/tabs become spaces so a
/// multi-line message stays on one row.
fn preview(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\t' || c == '\r' { ' ' } else { c })
        .collect();
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= max {
        flat
    } else if max == 0 {
        String::new()
    } else {
        // Reserve one char for the ellipsis.
        let mut out: String = chars[..max.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }
}

/// Render the message-rewind picker for `rw` using the given colour `palette`.
pub fn draw(frame: &mut Frame, rw: &RewindState, palette: &Palette) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // title: top+bottom rules
            Constraint::Min(1),    // flat message list
            Constraint::Length(1), // keybinding hint line
        ])
        .split(frame.area());

    // --- Title bar ---
    // Top+bottom rules only — title sits on the TOP rule, dim style.
    let title_block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim))
        .title(Span::styled(
            " edit a previous message ",
            Style::default().fg(palette.dim),
        ))
        .padding(Padding::horizontal(1));

    let title_inner = title_block.inner(chunks[0]);

    let note = Line::from(Span::styled(
        "pick the message to rewind to — the top entry is the last message",
        Style::default().fg(palette.dim),
    ));
    frame.render_widget(title_block, chunks[0]);
    frame.render_widget(Paragraph::new(note), title_inner);

    // --- Message list (flat, no borders) ---
    // Render rows directly into the inset area (1 char horizontal margin).
    let inner = chunks[1].inner(Margin { horizontal: 1, vertical: 0 });
    let inner_w = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();
    for (i, entry) in rw.entries.iter().enumerate() {
        // Whole inner width is available for the preview (minus a 1-char gutter).
        let text = preview(&entry.content, inner_w.saturating_sub(1).max(1));
        let style = if i == rw.selected {
            Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
        } else {
            Style::default().fg(palette.fg)
        };
        lines.push(Line::styled(text, style));
    }

    // Scroll so the selected row stays visible within the inner height.
    let list_height = inner.height as usize;
    let scroll = rw.selected.saturating_sub(list_height.saturating_sub(1)) as u16;

    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);

    // --- Keybinding hint ---
    let hint = "↑↓ select · Enter edit · Esc cancel";
    let instructions = Paragraph::new(hint).style(Style::default().fg(palette.dim));
    frame.render_widget(instructions, chunks[2]);
}
