//! View – `--resume` session picker (SessionPicker mode).
//!
//! Displays the list of saved sessions filtered in real time as the user
//! types.  Layout (top to bottom):
//!
//! 1. Search bar — shows the live query string.
//! 2. Session list — each row: name, message count, last-modified age.
//!    The selected row is highlighted cyan.  The list scrolls to keep the
//!    selection visible.
//! 3. One-line keybinding hint.
//!
//! Filtering and selection state live in [`app::mode::PickerState`].
//! Keystroke handling lives in [`controller::input::handle_picker`].

use std::time::SystemTime;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use crate::app::mode::PickerState;

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

/// Render the session picker for `picker`.
pub fn draw(frame: &mut Frame, picker: &PickerState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // search bar (bordered → 1 text row + 2 border)
            Constraint::Min(1),    // session list (variable height)
            Constraint::Length(1), // keybinding hint line
        ])
        .split(frame.area());

    // --- Search bar ---
    let search_block = Block::default().borders(Borders::ALL).title("search");
    let search = Paragraph::new(picker.query.as_str()).block(search_block);
    frame.render_widget(search, chunks[0]);

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
        // Highlight the currently selected row with inverted cyan colours.
        let style = if i == picker.selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        lines.push(Line::styled(text, style));
    }

    // Scroll calculation: keep the selected row on-screen.
    //
    // `list_height` is the number of visible text rows inside the bordered
    // widget (total height minus 2 for the top and bottom border lines).
    // `saturating_sub(2)` prevents underflow when the terminal is tiny.
    //
    // `scroll` is the first row index that should be visible.  We want the
    // selected row to stay within [scroll, scroll + list_height - 1], so:
    //   scroll = max(0, selected - (list_height - 1))
    // which is exactly `selected.saturating_sub(list_height - 1)`.
    let list_height = (chunks[1].height as usize).saturating_sub(2); // subtract top + bottom border
    let scroll = picker
        .selected
        .saturating_sub(list_height.saturating_sub(1)) as u16;

    let list_block = Block::default().borders(Borders::ALL).title("sessions");
    let list = Paragraph::new(lines).block(list_block).scroll((scroll, 0));
    frame.render_widget(list, chunks[1]);

    // --- Keybinding hint ---
    let instructions = Paragraph::new("↑↓ select · type to filter · Enter open · Esc/Ctrl+C quit")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(instructions, chunks[2]);
}
