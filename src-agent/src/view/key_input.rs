//! View – credentials form (KeyInput mode).
//!
//! Shown when the app lacks a usable OpenRouter API key: either on first run
//! or when the user explicitly reconfigures credentials.  The form contains
//! two fields — API key (masked) and Model — navigated with Tab / ↑↓.
//!
//! This screen is purely presentational; field editing and field-switching
//! logic live in [`app::mode::KeyInputForm`], and the submit / cancel actions
//! are returned by [`controller::input::handle_key_input`].

use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use crate::app::mode::KeyInputForm;

/// Render the credentials form for `form`.
///
/// The active field is highlighted with a yellow border; the inactive field
/// uses dark gray.  The API key value is always masked with `*` characters
/// regardless of which field is focused.  The provider field is optional and
/// shown in plain text.
pub fn draw(frame: &mut Frame, form: &KeyInputForm) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(3), // api key field (bordered)
            Constraint::Length(3), // model field   (bordered)
            Constraint::Length(3), // provider field (bordered)
            Constraint::Min(0),    // flexible spacer pushes footer to bottom
            Constraint::Length(1), // footer / key hints
        ])
        .split(frame.area());

    let title = Paragraph::new("Enter OpenRouter credentials (saved to this session only)")
        .style(Style::default().fg(Color::White));
    frame.render_widget(title, chunks[0]);

    // Derive focus state: field 0 = api_key, field 1 = model, field 2 = provider.
    let key_active = form.field == 0;
    let model_active = form.field == 1;
    let provider_active = form.field == 2;

    // Active field gets a yellow border; inactive field gets dark gray.
    let key_border = if key_active { Color::Yellow } else { Color::DarkGray };
    let model_border = if model_active { Color::Yellow } else { Color::DarkGray };
    let provider_border = if provider_active { Color::Yellow } else { Color::DarkGray };

    // Always show the API key as asterisks — never reveal it in plain text.
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

    let provider_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(provider_border))
        .title("Provider (OpenRouter routing, optional — e.g. anthropic)");
    let provider_field = Paragraph::new(form.provider.as_str()).block(provider_block);
    frame.render_widget(provider_field, chunks[3]);

    // Footer lives in chunk[5] (chunk[4] is the flexible spacer).
    let footer = Paragraph::new("Tab/↑↓ switch · Enter next/save · Esc cancel · Ctrl+C quit")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[5]);
}
