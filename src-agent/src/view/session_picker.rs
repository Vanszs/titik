//! View – `--resume` session picker (SessionPicker mode).
//!
//! Displays the list of saved sessions filtered in real time as the user
//! types.  Layout (top to bottom):
//!
//! 1. Search label + query line — flat, no border.
//! 2. Session list — each row: name, message count, last-modified age.
//!    The selected row is highlighted with `palette.sel_fg` on `palette.sel_bg`.
//!    The list scrolls to keep the selection visible.
//! 3. One-line keybinding hint.
//!
//! Filtering and selection state live in [`app::mode::PickerState`].
//! Keystroke handling lives in [`controller::input::handle_picker`].

use std::time::SystemTime;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use crate::app::mode::PickerState;
use crate::view::theme::Palette;

/// Format a `SystemTime` as a human-readable relative age string.
///
/// Returns strings like `"5s ago"`, `"3m ago"`, `"2h ago"`, `"4d ago"`.
/// Falls back to `"?"` if the system clock is behind `modified` (unlikely
/// but possible with clock skew or file-system metadata from the future).
fn fmt_modified(modified: SystemTime) -> String {
    match SystemTime::now().duration_since(modified) {
        Ok(dur) => {
            let secs = dur.as_secs();
            if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86400)
            }
        }
        // SystemTime::duration_since returns Err when `modified` > `now`.
        Err(_) => "?".to_string(),
    }
}

/// Render the session picker for `picker` using the given colour `palette`.
pub fn draw(frame: &mut Frame, picker: &PickerState, palette: &Palette) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // search label
            Constraint::Length(1), // search query value
            Constraint::Min(1),    // session list
            Constraint::Length(1), // keybinding hint line
        ])
        .split(frame.area());

    // --- Search bar ---
    // Label in accent, value below in fg.
    let search_label = Paragraph::new(
        Line::from(vec![Span::styled("search", Style::default().fg(palette.accent))]),
    );
    frame.render_widget(search_label, chunks[0]);

    let search_value = Paragraph::new(
        Line::from(vec![Span::styled(
            picker.query.as_str(),
            Style::default().fg(palette.fg),
        )]),
    );
    frame.render_widget(search_value, chunks[1]);

    // --- Session list ---
    // Build one styled Line per filtered entry.  `i` is the position in the
    // filtered list; `j` is the index into `picker.all` (the unfiltered vec).
    let mut lines: Vec<Line> = Vec::new();
    for (i, &j) in picker.filtered_idx.iter().enumerate() {
        let meta = &picker.all[j];
        let text = format!(
            "{}  ({} msgs)  {}",
            meta.name,
            meta.message_count,
            fmt_modified(meta.modified)
        );
        // Highlight the selected row with palette selection colours.
        let style = if i == picker.selected {
            Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
        } else {
            Style::default().fg(palette.fg)
        };
        lines.push(Line::styled(text, style));
    }

    // Scroll calculation: keep the selected row on-screen.
    //
    // `list_height` is the number of visible text rows in the flat list area.
    // (No borders to subtract from since the list is borderless.)
    //
    // `scroll` is the first row index that should be visible.  We want the
    // selected row to stay within [scroll, scroll + list_height - 1], so:
    //   scroll = max(0, selected - (list_height - 1))
    // which is exactly `selected.saturating_sub(list_height - 1)`.
    let list_height = chunks[2].height as usize;
    let scroll = picker
        .selected
        .saturating_sub(list_height.saturating_sub(1)) as u16;

    let list = Paragraph::new(lines).scroll((scroll, 0));
    frame.render_widget(list, chunks[2]);

    // --- Keybinding hint ---
    let instructions =
        Paragraph::new("↑↓ select · type to filter · Enter open · Esc/Ctrl+C quit")
            .style(Style::default().fg(palette.dim));
    frame.render_widget(instructions, chunks[3]);
}
