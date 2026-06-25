//! View – first-run setup WIZARD (KeyInput mode).
//!
//! A themed, 2-step, provider-agnostic setup screen shown when the app has no
//! usable provider connection (first run, or an explicit reconfigure).
//!
//! Steps:
//! - **0 — connection:** `Endpoint` (any OpenAI-compatible base URL) + `API key`.
//! - **1 — model:** `Model` id (PLAIN TEXT this pass; OpenRouter omnisearch is a
//!   later pass, gated on [`KeyInputForm::is_openrouter`]).
//!
//! Layout: a single left-aligned block of BLOCK_W columns, horizontally centred
//! in the frame and placed in the upper portion (≈25 % down). Every element
//! within the block shares the same left edge, so nothing is independently
//! centred. Label column is LABEL_W cols, right-aligned; a 2-space gap separates
//! it from the value column. An underline rule (`─`) sits directly below every
//! field (accent when active, dim when inactive) as the input affordance.
//!
//! Purely presentational: field editing / step transitions live in
//! [`app::mode::KeyInputForm`]; the finish / cancel actions are returned by
//! [`controller::input::handle_key_input`].

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::mode::KeyInputForm;
use crate::view::theme::Palette;

/// Total width (chars) of the content block. Clamped to the available area.
const BLOCK_W: u16 = 58;
/// Width (chars) of the right-aligned label column. Two trailing spaces separate
/// label from value.
const LABEL_W: usize = 9;
/// Gap between label column and value column (spaces).
const GAP: usize = 2;
/// Cursor block glyph appended to the active field's value.
const CURSOR: char = '\u{2588}'; // █

// ── helpers ──────────────────────────────────────────────────────────────────

/// Compute the left-edge x coordinate that centres `block_w` inside `frame_w`.
fn block_x(frame_w: u16, block_w: u16) -> u16 {
    frame_w.saturating_sub(block_w) / 2
}

/// Build a label+value line for a form field.
///
/// The label is right-aligned in LABEL_W cols; a GAP-space separator is
/// appended; then the value text (with an optional trailing cursor block when
/// `active`). Label colour: `palette.accent` when active, else `palette.dim`.
/// Value colour: `palette.fg` when active, else `palette.dim`.
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
            format!("{label:>LABEL_W$}{:GAP$}", ""),
            Style::default().fg(label_color),
        ),
        Span::styled(shown, Style::default().fg(value_color)),
    ])
}

/// Build the underline rule that sits directly below a field row.
///
/// The rule spans the value column only (offset by LABEL_W + GAP spaces).
/// `rule_w` is the available width for the rule (value column width).
/// Colour: `palette.accent` when the field is active, else `palette.dim`.
fn rule_line(rule_w: usize, active: bool, palette: &Palette) -> Line<'static> {
    let color = if active { palette.accent } else { palette.dim };
    Line::from(vec![
        Span::raw(format!("{:width$}", "", width = LABEL_W + GAP)),
        Span::styled(
            "\u{2500}".repeat(rule_w.max(1)),
            Style::default().fg(color),
        ),
    ])
}

/// Build a dim hint line indented to the value column (e.g. an example model id).
fn hint_line(text: &'static str, palette: &Palette) -> Line<'static> {
    Line::from(vec![
        Span::raw(format!("{:width$}", "", width = LABEL_W + GAP)),
        Span::styled(text, Style::default().fg(palette.dim)),
    ])
}

// ── main draw ────────────────────────────────────────────────────────────────

/// Render the setup wizard for `form` using the given colour `palette`.
pub fn draw(frame: &mut Frame, form: &KeyInputForm, palette: &Palette) {
    let area = frame.area();

    // ── Vertical layout ──────────────────────────────────────────────────────
    // A top spacer pushes the block to ≈25 % down; the block body is a Min
    // region so it expands to hold all rows; a dim footer is pinned to the
    // bottom. An explicit bottom margin prevents the footer from touching the
    // terminal edge.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(25), // top spacer → block starts at ~25 %
            Constraint::Min(1),         // block body (title + subtitle + step + fields)
            Constraint::Length(1),      // footer key hints
            Constraint::Length(1),      // bottom margin
        ])
        .split(area);

    // ── Block geometry ───────────────────────────────────────────────────────
    // Clamp block width to the available frame width; compute the left edge so
    // the block is horizontally centred. Every widget in the block is rendered
    // into a 1-row-tall Rect at (block_x, row_y, block_w, 1).
    let block_w = BLOCK_W.min(area.width);
    let bx = block_x(area.width, block_w);

    // The block body region starts at chunks[1].y. We render rows top-down,
    // incrementing `row` for each line.
    let body_y = chunks[1].y;
    let body_h = chunks[1].height;

    // ── Helper: render one line into the block at row offset `row` ───────────
    // Returns immediately when the row would be out of the body region.
    let mut row: u16 = 0;
    let render_line = |frame: &mut Frame, line: Line, r: u16| {
        if r >= body_h {
            return;
        }
        let rect = Rect {
            x: bx,
            y: body_y + r,
            width: block_w,
            height: 1,
        };
        frame.render_widget(Paragraph::new(line), rect);
    };

    // Row 0: "simple-coder" in accent, left-aligned at the block's left edge.
    render_line(
        frame,
        Line::from(Span::styled("simple-coder", Style::default().fg(palette.accent))),
        row,
    );
    row += 1;

    // Row 1: dim subtitle.
    render_line(
        frame,
        Line::from(Span::styled(
            "first-time setup \u{00b7} change anything later in /settings",
            Style::default().fg(palette.dim),
        )),
        row,
    );
    row += 1;

    // Row 2: blank.
    row += 1;

    // Row 3: "step N / 2 · <name>" — step number in accent, rest dim.
    let (step_num, step_name) = if form.step == 0 {
        ("1", "connection")
    } else {
        ("2", "model")
    };
    render_line(
        frame,
        Line::from(vec![
            Span::styled("step ", Style::default().fg(palette.dim)),
            Span::styled(step_num, Style::default().fg(palette.accent)),
            Span::styled(
                format!(" / 2 \u{00b7} {step_name}"),
                Style::default().fg(palette.dim),
            ),
        ]),
        row,
    );
    row += 1;

    // Row 4: blank.
    row += 1;

    // ── Form fields ──────────────────────────────────────────────────────────
    // The value column starts at offset LABEL_W + GAP from the block's left
    // edge; the rule spans from there to the block's right edge.
    let value_col_start = LABEL_W + GAP;
    let rule_w = (block_w as usize).saturating_sub(value_col_start);

    if form.step == 0 {
        // Step 0: Endpoint + API key fields.
        let endpoint_active = form.field == 0;
        let key_active = form.field == 1;

        // Endpoint field row.
        render_line(frame, field_line("Endpoint", &form.endpoint, endpoint_active, palette), row);
        row += 1;
        // Endpoint underline rule.
        render_line(frame, rule_line(rule_w, endpoint_active, palette), row);
        row += 1;

        // Blank spacer between fields.
        row += 1;

        // API key field row.
        render_line(frame, field_line("API key", &form.api_key, key_active, palette), row);
        row += 1;
        // API key underline rule.
        render_line(frame, rule_line(rule_w, key_active, palette), row);
        // row not needed after last field
    } else {
        // Step 1: single Model field.
        // Model field row (always the only/active field on this step).
        render_line(frame, field_line("Model", &form.model, true, palette), row);
        row += 1;
        // Model underline rule (always active accent on this step).
        render_line(frame, rule_line(rule_w, true, palette), row);
        row += 1;
        // Example hint indented to the value column.
        render_line(frame, hint_line("e.g. openai/gpt-4o-mini", palette), row);
    }

    // ── Footer ───────────────────────────────────────────────────────────────
    // Pinned to the bottom of the frame, dim, centred.
    let footer_text = if form.step == 0 {
        "tab/\u{2191}\u{2193} switch \u{00b7} enter next \u{00b7} esc cancel \u{00b7} ctrl+c quit"
    } else {
        "enter finish \u{00b7} esc back \u{00b7} ctrl+c quit"
    };
    let footer = Paragraph::new(Line::from(Span::styled(
        footer_text,
        Style::default().fg(palette.dim),
    )));
    // Centre the footer text inside chunks[2].
    let footer_w = footer_text.chars().count() as u16;
    let footer_x = chunks[2]
        .x
        .saturating_add(chunks[2].width.saturating_sub(footer_w) / 2);
    let footer_rect = Rect {
        x: footer_x,
        y: chunks[2].y,
        width: footer_w.min(chunks[2].width),
        height: 1,
    };
    frame.render_widget(footer, footer_rect);
}
