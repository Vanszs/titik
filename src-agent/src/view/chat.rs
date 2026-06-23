use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use crate::app::state::AppStateRest;
use crate::config::{APP_TITLE, DEFAULT_MODEL};
use crate::dto::chat::Role;

pub fn draw(frame: &mut Frame, rest: &AppStateRest) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // Title bits from the active session settings.
    let (name, model) = match rest.session.as_ref() {
        Some(s) => (s.name.clone(), s.settings.model.clone()),
        None => (APP_TITLE.to_string(), DEFAULT_MODEL.to_string()),
    };

    // Build transcript lines, skipping the System message.
    let mut lines: Vec<Line> = Vec::new();
    if let Some(session) = rest.session.as_ref() {
        for msg in session.conversation.messages() {
            let (prefix_text, color) = match msg.role {
                Role::System => continue,
                Role::User => ("[you] ", Color::Cyan),
                Role::Assistant => ("[ai] ", Color::Green),
            };
            let prefix = Span::styled(prefix_text, Style::default().fg(color));
            let content = Span::raw(msg.content.clone());
            lines.push(Line::from(vec![prefix, content]));
        }
    }

    // Live streaming buffer, if any non-empty.
    if let Some(buf) = rest.streaming.as_ref() {
        if !buf.is_empty() {
            let prefix = Span::styled("[ai] ", Style::default().fg(Color::Green));
            let content = Span::raw(buf.clone());
            lines.push(Line::from(vec![prefix, content]));
        }
    }

    let messages_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("chat — {name} [{model}]"));

    let messages = Paragraph::new(lines)
        .block(messages_block)
        .wrap(Wrap { trim: false })
        .scroll((rest.scroll, 0));
    frame.render_widget(messages, chunks[0]);

    // Input box.
    let input_block = Block::default()
        .borders(Borders::ALL)
        .title("message (Enter=send, /help, Ctrl+R=resend, Esc=interrupt/quit)");
    let input = Paragraph::new(rest.input.as_str()).block(input_block);
    frame.render_widget(input, chunks[1]);

    // Status bar.
    let status = Paragraph::new(rest.status.as_str()).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(status, chunks[2]);
}
