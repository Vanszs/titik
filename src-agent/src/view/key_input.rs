//! View – first-run setup WIZARD (KeyInput mode).
//!
//! A themed, 2-step, provider-agnostic setup screen shown when the app has no
//! usable provider connection (first run, or an explicit reconfigure). It mirrors
//! the loading splash aesthetic (`view::loading`): borderless, palette-themed,
//! centered, no emoji — braille / `·` / box-drawing glyphs only.
//!
//! Steps:
//! - **0 — connection:** `Endpoint` (any OpenAI-compatible base URL) + `API key`.
//! - **1 — model:** `Model` id (PLAIN TEXT this pass; OpenRouter omnisearch is a
//!   later pass, gated on [`KeyInputForm::is_openrouter`]).
//!
//! Purely presentational: field editing / step transitions live in
//! [`app::mode::KeyInputForm`]; the finish / cancel actions are returned by
//! [`controller::input::handle_key_input`].

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::mode::KeyInputForm;
use crate::view::theme::Palette;

/// Width (chars) of the right-aligned label column, so values line up in a
/// single column across both steps. Two trailing spaces separate label / value.
const LABEL_W: usize = 9;
/// Width (chars) of the gray underline rule drawn beneath the active input.
const RULE_W: usize = 40;
/// Cursor block glyph appended to the active field's value.
const CURSOR: char = '\u{2588}'; // █

/// One label + value line for a field, right-aligned label column + value.
///
/// The active field's label is `palette.accent` and its value `palette.fg` with
/// a trailing cursor block; inactive labels/values are `palette.dim`.
fn field_line<'a>(label: &'a str, value: &str, active: bool, palette: &Palette) -> Line<'a> {
    let (label_color, value_color) = if active {
        (palette.accent, palette.fg)
    } else {
        (palette.dim, palette.dim)
    };
    let mut shown = value.to_string();
    if active {
        shown.push(CURSOR);
    }
    Line::from(vec![
        Span::styled(
            format!("{label:>LABEL_W$}  "),
            Style::default().fg(label_color),
        ),
        Span::styled(shown, Style::default().fg(value_color)),
    ])
}

/// The gray underline rule placed beneath the active input field (affordance
/// borrowed from the model modal's search field). Offset by the label column so
/// it sits under the value.
fn rule_line(palette: &Palette) -> Line<'static> {
    Line::from(vec![
        Span::raw(format!("{:width$}", "", width = LABEL_W + 2)),
        Span::styled("\u{2500}".repeat(RULE_W), Style::default().fg(palette.dim)),
    ])
}

/// A dim, label-column-offset hint line shown beneath a field (e.g. an example
/// model id). Aligns under the value column like [`rule_line`].
fn hint_line(text: &str, palette: &Palette) -> Line<'static> {
    Line::from(vec![
        Span::raw(format!("{:width$}", "", width = LABEL_W + 2)),
        Span::styled(text.to_string(), Style::default().fg(palette.dim)),
    ])
}

/// Render the setup wizard for `form` using the given colour `palette`.
pub fn draw(frame: &mut Frame, form: &KeyInputForm, palette: &Palette) {
    let area = frame.area();

    // Vertical layout mirrors the loading splash: a top spacer drops the title
    // into the upper third, the wizard body sits below it, and a dim footer pins
    // to the bottom. Fixed-height rows are separated by flexible spacers so the
    // whole thing reads as a centered splash.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(22), // top spacer → title in the upper third
            Constraint::Length(1),      // title
            Constraint::Length(1),      // gap
            Constraint::Length(1),      // NOTE subtitle
            Constraint::Length(1),      // gap
            Constraint::Length(1),      // step indicator
            Constraint::Length(1),      // gap
            Constraint::Min(5),         // wizard body (fields + rules + hints)
            Constraint::Length(1),      // footer
            Constraint::Length(1),      // bottom margin
        ])
        .split(area);

    // --- Title: "simple-coder" in accent, centered ---
    let title = Paragraph::new(Line::from(Span::styled(
        "simple-coder",
        Style::default().fg(palette.accent),
    )))
    .alignment(Alignment::Center);
    frame.render_widget(title, chunks[1]);

    // --- NOTE subtitle: clearly visible, dim, centered ---
    let subtitle = Paragraph::new(Line::from(Span::styled(
        "first-time setup · change anything later in /settings",
        Style::default().fg(palette.dim),
    )))
    .alignment(Alignment::Center);
    frame.render_widget(subtitle, chunks[3]);

    // --- Step indicator: "step N / 2 · <name>" — current step number in accent ---
    let (step_num, step_name) = if form.step == 0 {
        ("1", "connection")
    } else {
        ("2", "model")
    };
    let step_line = Paragraph::new(Line::from(vec![
        Span::styled("step ", Style::default().fg(palette.dim)),
        Span::styled(step_num, Style::default().fg(palette.accent)),
        Span::styled(" / 2 · ", Style::default().fg(palette.dim)),
        Span::styled(step_name, Style::default().fg(palette.dim)),
    ]))
    .alignment(Alignment::Center);
    frame.render_widget(step_line, chunks[5]);

    // --- Wizard body: the active step's fields, each with the active one ruled ---
    let mut body: Vec<Line> = Vec::new();
    if form.step == 0 {
        let endpoint_active = form.field == 0;
        let key_active = form.field == 1;

        body.push(field_line("Endpoint", &form.endpoint, endpoint_active, palette));
        if endpoint_active {
            body.push(rule_line(palette));
        }
        body.push(Line::default()); // spacer between the two fields

        body.push(field_line("API key", &form.api_key, key_active, palette));
        if key_active {
            body.push(rule_line(palette));
        }
    } else {
        // Model step: a single plain-text field with a gray rule + example hint.
        body.push(field_line("Model", &form.model, true, palette));
        body.push(rule_line(palette));
        body.push(hint_line("e.g. openai/gpt-4o-mini", palette));
    }
    frame.render_widget(
        Paragraph::new(body).alignment(Alignment::Center),
        chunks[7],
    );

    // --- Footer: context-sensitive key hints, dim, centered ---
    let footer_text = if form.step == 0 {
        "tab/\u{2191}\u{2193} switch · enter next · esc cancel · ctrl+c quit"
    } else {
        "enter finish · esc back · ctrl+c quit"
    };
    let footer = Paragraph::new(Line::from(Span::styled(
        footer_text,
        Style::default().fg(palette.dim),
    )))
    .alignment(Alignment::Center);
    frame.render_widget(footer, chunks[8]);
}
