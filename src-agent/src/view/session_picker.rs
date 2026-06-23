use std::time::SystemTime;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use crate::app::mode::PickerState;

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
        Err(_) => "?".to_string(),
    }
}

pub fn draw(frame: &mut Frame, picker: &PickerState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // search
            Constraint::Min(1),    // list
            Constraint::Length(1), // instructions
        ])
        .split(frame.area());

    // Search bar.
    let search_block = Block::default().borders(Borders::ALL).title("search");
    let search = Paragraph::new(picker.query.as_str()).block(search_block);
    frame.render_widget(search, chunks[0]);

    // List.
    let mut lines: Vec<Line> = Vec::new();
    for (i, &j) in picker.filtered_idx.iter().enumerate() {
        let meta = &picker.all[j];
        let text = format!(
            "{}  ({} msgs)  {}",
            meta.name,
            meta.message_count,
            fmt_modified(meta.modified)
        );
        let style = if i == picker.selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        lines.push(Line::styled(text, style));
    }

    let list_height = (chunks[1].height as usize).saturating_sub(2); // top + bottom border
    let scroll = picker
        .selected
        .saturating_sub(list_height.saturating_sub(1)) as u16;

    let list_block = Block::default().borders(Borders::ALL).title("sessions");
    let list = Paragraph::new(lines).block(list_block).scroll((scroll, 0));
    frame.render_widget(list, chunks[1]);

    // Instructions.
    let instructions = Paragraph::new("↑↓ select · type to filter · Enter open · Esc/Ctrl+C quit")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(instructions, chunks[2]);
}
