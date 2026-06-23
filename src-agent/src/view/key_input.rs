//! View – credentials form (KeyInput mode).
//!
//! Shown when the app lacks a usable OpenRouter API key: either on first run
//! or when the user explicitly reconfigures credentials.  The form contains
//! three fields — API key (masked), Model, and Provider — navigated with Tab / ↑↓.
//!
//! Layout is fully flat (no borders); the active field label is rendered in
//! `palette.accent` and its value in `palette.fg`. Inactive labels / values
//! use `palette.dim` / `palette.fg` respectively.
//!
//! This screen is purely presentational; field editing and field-switching
//! logic live in [`app::mode::KeyInputForm`], and the submit / cancel actions
//! are returned by [`controller::input::handle_key_input`].

use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use crate::app::mode::KeyInputForm;
use crate::view::theme::Palette;

/// Render the credentials form for `form` using the given colour `palette`.
///
/// Active field label is in `palette.accent`; inactive labels in `palette.dim`.
/// The API key value is always masked with `*` characters. Provider is shown in
/// plain text and is optional.
pub fn draw(frame: &mut Frame, form: &KeyInputForm, palette: &Palette) {
    // Layout: title | api_key label+value | model label+value | provider label+value
    //         | spacer | footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // api key label
            Constraint::Length(1), // api key value
            Constraint::Length(1), // model label
            Constraint::Length(1), // model value
            Constraint::Length(1), // provider label
            Constraint::Length(1), // provider value
            Constraint::Min(0),    // flexible spacer
            Constraint::Length(1), // footer / key hints
        ])
        .split(frame.area());

    // Title
    let title = Paragraph::new("Enter OpenRouter credentials (saved to this session only)")
        .style(Style::default().fg(palette.fg));
    frame.render_widget(title, chunks[0]);

    // Focus state: field 0 = api_key, field 1 = model, field 2 = provider.
    let key_active = form.field == 0;
    let model_active = form.field == 1;
    let provider_active = form.field == 2;

    // Always show the API key as asterisks — never reveal it in plain text.
    let masked: String = "*".repeat(form.api_key.chars().count());

    // API key
    let key_label_color = if key_active { palette.accent } else { palette.dim };
    let key_value_color = if key_active { palette.fg } else { palette.dim };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("API key", Style::default().fg(key_label_color)),
        ])),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(masked, Style::default().fg(key_value_color)),
        ])),
        chunks[2],
    );

    // Model
    let model_label_color = if model_active { palette.accent } else { palette.dim };
    let model_value_color = if model_active { palette.fg } else { palette.dim };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Model", Style::default().fg(model_label_color)),
        ])),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(form.model.as_str(), Style::default().fg(model_value_color)),
        ])),
        chunks[4],
    );

    // Provider (optional)
    let provider_label_color = if provider_active { palette.accent } else { palette.dim };
    let provider_value_color = if provider_active { palette.fg } else { palette.dim };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Provider (OpenRouter routing, optional — e.g. anthropic)",
                Style::default().fg(provider_label_color),
            ),
        ])),
        chunks[5],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(form.provider.as_str(), Style::default().fg(provider_value_color)),
        ])),
        chunks[6],
    );

    // Footer lives in chunk[8] (chunk[7] is the flexible spacer).
    let footer = Paragraph::new("Tab/↑↓ switch · Enter next/save · Esc cancel · Ctrl+C quit")
        .style(Style::default().fg(palette.dim));
    frame.render_widget(footer, chunks[8]);
}
