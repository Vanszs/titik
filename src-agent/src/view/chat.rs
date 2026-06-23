//! Chat screen renderer: the read-only view of [`AppStateRest`].
//!
//! Last stage of the keystroke -> Action -> state -> render flow. Pure
//! function of state: it borrows the session transcript, the live streaming
//! buffer, the input, and the status line into three stacked panes (messages /
//! input / status). It never mutates state and never allocates the transcript —
//! every span borrows from state so a redraw stays cheap at 60fps+.

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

/// Render the chat screen from `rest`. Borrows throughout — no per-frame
/// clones of the transcript or streaming buffer.
pub fn draw(frame: &mut Frame, rest: &AppStateRest) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // Title bits from the active session settings. Borrowed, not cloned.
    let (name, model): (&str, &str) = match rest.session.as_ref() {
        Some(s) => (s.name.as_str(), s.settings.model.as_str()),
        None => (APP_TITLE, DEFAULT_MODEL),
    };

    // Build transcript lines, skipping the System message. Spans borrow message
    // content directly, so no per-frame allocation of the transcript.
    let cap = rest
        .session
        .as_ref()
        .map_or(0, |s| s.conversation.messages().len())
        + 1; // +1 for the live streaming line
    let mut lines: Vec<Line> = Vec::with_capacity(cap);
    if let Some(session) = rest.session.as_ref() {
        for msg in session.conversation.messages() {
            let (prefix_text, color) = match msg.role {
                Role::System => continue,
                Role::User => ("[you] ", Color::Cyan),
                Role::Assistant => ("[ai] ", Color::Green),
            };
            let prefix = Span::styled(prefix_text, Style::default().fg(color));
            let content = Span::raw(msg.content.as_str());
            lines.push(Line::from(vec![prefix, content]));
        }
    }

    // Live streaming buffer, if any non-empty.
    if let Some(buf) = rest.streaming.as_ref() {
        if !buf.is_empty() {
            let prefix = Span::styled("[ai] ", Style::default().fg(Color::Green));
            let content = Span::raw(buf.as_str());
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
