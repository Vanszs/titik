use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use crate::app::mode::KeyInputForm;

pub fn draw(frame: &mut Frame, form: &KeyInputForm) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(3), // api key
            Constraint::Length(3), // model
            Constraint::Min(0),    // spacer
            Constraint::Length(1), // footer
        ])
        .split(frame.area());

    let title = Paragraph::new("Enter OpenRouter credentials (saved to this session only)")
        .style(Style::default().fg(Color::White));
    frame.render_widget(title, chunks[0]);

    let key_active = form.field == 0;
    let model_active = form.field == 1;

    let key_border = if key_active {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let model_border = if model_active {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let masked: String = "*".repeat(form.api_key.chars().count());
    let key_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(key_border))
        .title("API key");
    let key_field = Paragraph::new(masked).block(key_block);
    frame.render_widget(key_field, chunks[1]);

    let model_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(model_border))
        .title("Model");
    let model_field = Paragraph::new(form.model.as_str()).block(model_block);
    frame.render_widget(model_field, chunks[2]);

    let footer = Paragraph::new("Tab/↑↓ switch · Enter next/save · Esc cancel · Ctrl+C quit")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[4]);
}
