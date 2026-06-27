//! View – `/usage` cost and token usage dashboard (Usage mode).
//!
//! A full-screen, read-only page with four panels (top to bottom):
//!
//! 1. **Current session** — live counters from `AppStateRest`: tokens_in,
//!    tokens_cached, tokens_out, cost.  No DB query needed; data is in memory.
//! 2. **Yearly heatmap** — placeholder header; filled in Stage 2.
//! 3. **Top models** — placeholder header; filled in Stage 2.
//! 4. **Weekly breakdown** — placeholder header; filled in Stage 3.
//!
//! Border convention (matches project rules):
//! - Page header: `Borders::BOTTOM` only (single horizontal rule).
//! - Section headers: plain dim line, no borders.
//! - No full boxes.
//!
//! Layout:
//! ```text
//!  usage
//! ─────────────────────────────────────────────────────────
//!  current session
//!  tokens in    12 345   cached  1 200   out  4 567   $0.0123
//!
//!  yearly  (coming soon)
//!
//!  top models  (coming soon)
//!
//!  weekly  (coming soon)
//!
//!  Esc close
//! ```

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};

use crate::app::state::AppStateRest;
use crate::view::theme::Palette;

/// Render the `/usage` dashboard using live counters from `rest` and the
/// given colour `palette`.
///
/// Stage 2 will add `daily_costs`, `top_models`, and `weekly` query results
/// as extra parameters; for now only the in-memory counters are shown.
pub fn draw(frame: &mut Frame, rest: &AppStateRest, palette: &Palette) {
    let area = frame.area();

    // Outer zones: header | body | footer hint.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // "usage" title + BOTTOM border
            Constraint::Min(0),    // scrollable body
            Constraint::Length(1), // key hint
        ])
        .split(area);

    // ── Header ──────────────────────────────────────────────────────────────
    let header_block = Block::new()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim))
        .padding(Padding::horizontal(1));
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "usage",
            Style::default().fg(palette.dim),
        )))
        .block(header_block),
        outer[0],
    );

    // ── Body ────────────────────────────────────────────────────────────────
    let body = outer[1].inner(Margin { horizontal: 1, vertical: 0 });

    let lines = build_body(rest, palette);
    frame.render_widget(Paragraph::new(lines), body);

    // ── Footer hint ─────────────────────────────────────────────────────────
    let hint = outer[2].inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Esc close",
            Style::default().fg(palette.dim),
        ))),
        hint,
    );
}

/// Build the body lines for the dashboard.
///
/// Extracted so Stage 2 can extend this by appending DB-sourced sections
/// without touching the outer `draw` function signature.
fn build_body(rest: &AppStateRest, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // ── Section: current session ─────────────────────────────────────────
    lines.push(section_header("current session", palette));
    lines.push(Line::default()); // blank spacer

    // Row: "tokens in  NNN  cached  NNN  out  NNN  $N.NNNN"
    let cost_str = format_cost(rest.cost);
    let row = Line::from(vec![
        label_span("tokens in"),
        Span::raw("  "),
        value_span(&fmt_tokens(rest.tokens_in), palette),
        Span::raw("   "),
        label_span("cached"),
        Span::raw("  "),
        value_span(&fmt_tokens(rest.tokens_cached), palette),
        Span::raw("   "),
        label_span("out"),
        Span::raw("  "),
        value_span(&fmt_tokens(rest.tokens_out), palette),
        Span::raw("   "),
        value_span(&cost_str, palette),
    ]);
    lines.push(row);

    // Blank line between sections.
    lines.push(Line::default());
    lines.push(Line::default());

    // ── Section: yearly heatmap (Stage 2 placeholder) ────────────────────
    lines.push(section_header("yearly", palette));
    lines.push(Line::default());
    lines.push(placeholder_line("heatmap — available after Stage 2", palette));
    lines.push(Line::default());
    lines.push(Line::default());

    // ── Section: top models (Stage 2 placeholder) ─────────────────────────
    lines.push(section_header("top models", palette));
    lines.push(Line::default());
    lines.push(placeholder_line("model rankings — available after Stage 2", palette));
    lines.push(Line::default());
    lines.push(Line::default());

    // ── Section: weekly breakdown (Stage 3 placeholder) ───────────────────
    lines.push(section_header("weekly", palette));
    lines.push(Line::default());
    lines.push(placeholder_line("weekly totals — available after Stage 3", palette));

    lines
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Render a dim bold section header label (no border, top-down convention).
fn section_header(title: &'static str, palette: &Palette) -> Line<'static> {
    Line::from(Span::styled(
        title,
        Style::default()
            .fg(palette.dim)
            .add_modifier(Modifier::BOLD),
    ))
}

/// A dim italic placeholder note for not-yet-implemented sections.
fn placeholder_line(text: &'static str, palette: &Palette) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(palette.dim)
            .add_modifier(Modifier::ITALIC),
    ))
}

/// A dim label span (field name).
fn label_span(text: &'static str) -> Span<'static> {
    Span::raw(text)
}

/// An accented value span (the numeric data).
fn value_span(text: &str, palette: &Palette) -> Span<'static> {
    Span::styled(
        text.to_owned(),
        Style::default().fg(palette.accent),
    )
}

/// Format a token count with thousands separators (space-separated groups).
fn fmt_tokens(n: u64) -> String {
    // Simple space-grouped formatting without external crates.
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let offset = bytes.len() % 3;
    for (i, &b) in bytes.iter().enumerate() {
        if i != 0 && i % 3 == offset {
            out.push(' ');
        }
        out.push(b as char);
    }
    out
}

/// Format a cost value as `$N.NNNN` (four decimal places).
fn format_cost(cost: f64) -> String {
    format!("${cost:.4}")
}
