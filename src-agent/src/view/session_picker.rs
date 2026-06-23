//! View – `--resume` session picker (SessionPicker mode).
//!
//! Displays the list of saved sessions filtered in real time as the user
//! types.  Layout (top to bottom):
//!
//! 1. Bordered search box — title ` search `, cursor appended.
//! 2. Bordered session list — title ` sessions (N) `, columns aligned.
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
    widgets::{Block, Padding, Paragraph},
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

/// Truncate `s` to at most `max` Unicode scalar values, appending `…` if cut.
fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        // Reserve one char for the ellipsis.
        let mut out: String = chars[..max.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }
}

/// Render the session picker for `picker` using the given colour `palette`.
pub fn draw(frame: &mut Frame, picker: &PickerState, palette: &Palette) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // search box (border + 1 line + border)
            Constraint::Min(1),    // session list box
            Constraint::Length(1), // keybinding hint line
        ])
        .split(frame.area());

    // --- Search box ---
    // Bordered block titled " search ", dim border + title, accent cursor.
    let search_block = Block::bordered()
        .title(Span::styled(" search ", Style::default().fg(palette.dim)))
        .border_style(Style::default().fg(palette.dim))
        .padding(Padding::horizontal(1));

    let search_inner = search_block.inner(chunks[0]);

    let search_line = Line::from(vec![
        Span::styled(picker.query.as_str(), Style::default().fg(palette.fg)),
        Span::styled("█", Style::default().fg(palette.accent)),
    ]);
    frame.render_widget(search_block, chunks[0]);
    frame.render_widget(Paragraph::new(search_line), search_inner);

    // --- Session list box ---
    // Bordered block titled " sessions (N) ", dim border + title.
    let count = picker.filtered_idx.len();
    let list_title = format!(" sessions ({count}) ");
    let list_block = Block::bordered()
        .title(Span::styled(list_title, Style::default().fg(palette.dim)))
        .border_style(Style::default().fg(palette.dim))
        .padding(Padding::horizontal(1));

    let list_inner = list_block.inner(chunks[1]);

    // Build one styled Line per filtered entry with aligned columns.
    //
    // Each row: `{name:<name_w$}  {count:>3} msgs   {age:>8}`
    //
    // `name_w` is derived from the inner width so the right columns always
    // land at the same horizontal offset regardless of name length.
    let inner_w = list_inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    for (i, &j) in picker.filtered_idx.iter().enumerate() {
        let meta = &picker.all[j];
        let right = format!(
            "{:>3} msgs   {:>8}",
            meta.message_count,
            fmt_modified(meta.modified)
        );
        // Width available for the name: total inner width minus right column
        // minus two separator spaces, clamped to at least 4 chars.
        let name_w = inner_w
            .saturating_sub(right.chars().count() + 2)
            .max(4);
        let name = truncate(&meta.name, name_w);
        let row = format!("{name:<name_w$}  {right}");

        let style = if i == picker.selected {
            Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
        } else {
            Style::default().fg(palette.fg)
        };
        lines.push(Line::styled(row, style));
    }

    // Scroll so the selected row stays visible within the box inner height.
    let list_height = list_inner.height as usize;
    let scroll = picker
        .selected
        .saturating_sub(list_height.saturating_sub(1)) as u16;

    frame.render_widget(list_block, chunks[1]);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), list_inner);

    // --- Keybinding hint ---
    let instructions =
        Paragraph::new("↑↓ select · type to filter · Enter open · Esc/Ctrl+C quit")
            .style(Style::default().fg(palette.dim));
    frame.render_widget(instructions, chunks[2]);
}
