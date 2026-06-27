//! View -- `/usage` cost and token usage dashboard (Usage mode).
//!
//! A full-screen, read-only page with four panels (top to bottom):
//!
//! 1. **Current session** -- live counters from `AppStateRest`: tokens_in,
//!    tokens_cached, tokens_out, cost.  No DB query needed; data is in memory.
//! 2. **Yearly heatmap** -- GitHub-style cost-per-day grid: 7 rows x ~53 cols.
//! 3. **Top models** -- top models by total spend from the global ledger.
//! 4. **Weekly breakdown** -- cost per week for the last 12 weeks with a
//!    block-char bar scaled to the max week cost.
//!
//! Border convention (matches project rules):
//! - Page header: `Borders::BOTTOM` only (single horizontal rule).
//! - Section headers: plain dim bold line, no borders.
//! - No full boxes.
//!
//! Heatmap colour scheme (fixed RGB, theme-independent so it reads on both
//! dark and light terminals):
//!
//! | bucket | condition          | RGB              |
//! |--------|--------------------|------------------|
//! | 0      | zero / no data     | (40, 40, 40)     |
//! | 1      | > 0                | (0, 68, 27)      |
//! | 2      | > p33              | (0, 109, 44)     |
//! | 3      | > p66              | (38, 166, 65)    |
//! | 4      | > p90              | (57, 211, 83)    |

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};

use crate::app::state::AppStateRest;
use crate::model::usage::{daily_costs, top_models, weekly_costs, DailyCost};
use crate::view::theme::Palette;

// ── Heatmap constants ────────────────────────────────────────────────────────

// Fixed green-ramp (matches GitHub contribution graph spirit).
// Level 0 = empty/background, levels 1-4 = ascending intensity.
const HEAT_EMPTY: Color = Color::Rgb(40, 40, 40);
const HEAT_1: Color = Color::Rgb(0, 68, 27);
const HEAT_2: Color = Color::Rgb(0, 109, 44);
const HEAT_3: Color = Color::Rgb(38, 166, 65);
const HEAT_4: Color = Color::Rgb(57, 211, 83);

/// Single block character used for each heatmap cell.
const CELL: &str = "\u{2588}"; // U+2588 FULL BLOCK

// ── Weekly bar constants ─────────────────────────────────────────────────────

// Block chars for the inline bar (8 levels: empty through full block).
const BAR_CHARS: [char; 9] = [' ', '\u{258F}', '\u{258E}', '\u{258D}', '\u{258C}', '\u{258B}', '\u{258A}', '\u{2589}', '\u{2588}'];

/// Max bar width in chars for each weekly row.
const BAR_MAX_WIDTH: usize = 20;

// ── Entry point ──────────────────────────────────────────────────────────────

/// Render the `/usage` dashboard using live counters from `rest` and the
/// given colour `palette`.
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

    // -- Header ---------------------------------------------------------------
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

    // -- Body -----------------------------------------------------------------
    let body = outer[1].inner(Margin { horizontal: 1, vertical: 0 });

    let lines = build_body(rest, palette);
    frame.render_widget(Paragraph::new(lines), body);

    // -- Footer hint ----------------------------------------------------------
    let hint = outer[2].inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Esc close",
            Style::default().fg(palette.dim),
        ))),
        hint,
    );
}

// ── Body builder ────────────────────────────────────────────────────────────

/// Build all body lines for the dashboard.
fn build_body(rest: &AppStateRest, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // ---- Section: current session ------------------------------------------
    lines.push(section_header("current session", palette));
    lines.push(Line::default());

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
    lines.push(Line::default());
    lines.push(Line::default());

    // ---- Section: yearly heatmap -------------------------------------------
    lines.push(section_header("yearly", palette));
    lines.push(Line::default());

    // Query last 371 days (53 full weeks + 2 days buffer to always have 53 cols).
    let daily = daily_costs(371);
    for heatmap_line in build_heatmap(&daily) {
        lines.push(heatmap_line);
    }
    lines.push(Line::default());
    lines.push(Line::default());

    // ---- Section: top models -----------------------------------------------
    lines.push(section_header("top models", palette));
    lines.push(Line::default());

    let models = top_models(8);
    if models.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no data yet",
            Style::default().fg(palette.dim),
        )));
    } else {
        for m in &models {
            let id = if m.model_id.is_empty() { "unknown".to_owned() } else { m.model_id.clone() };
            lines.push(Line::from(vec![
                Span::raw("  "),
                value_span(&id, palette),
                Span::styled(
                    format!("  {}  {} calls", format_cost(m.total_cost), m.call_count),
                    Style::default().fg(palette.dim),
                ),
            ]));
        }
    }
    lines.push(Line::default());
    lines.push(Line::default());

    // ---- Section: weekly breakdown -----------------------------------------
    lines.push(section_header("weekly", palette));
    lines.push(Line::default());

    let weeks = weekly_costs(12);
    if weeks.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no data yet",
            Style::default().fg(palette.dim),
        )));
    } else {
        let max_cost = weeks.iter().map(|w| w.cost).fold(0.0_f64, f64::max);
        for w in &weeks {
            lines.push(build_weekly_row(w.week_epoch, w.cost, max_cost, palette));
        }
    }

    lines
}

// ── Heatmap builder ─────────────────────────────────────────────────────────

/// Build the 7-row x 53-col GitHub-style heatmap lines from daily cost data.
///
/// Layout:
/// ```text
///     Mon  ██░░██...
///     Wed  ░░██░░...
///     Fri  ██░░██...
///          less ░▒▓█ more
/// ```
///
/// - Each cell = one `CELL` char coloured by cost intensity bucket.
/// - Gutter = 4-char weekday label (Mon/Wed/Fri on rows 1/3/5; blank others).
/// - Total width = 4 (gutter) + 53 (cells) = 57 chars -- fits normal terminal.
fn build_heatmap(daily: &[DailyCost]) -> Vec<Line<'static>> {
    // Build a day_epoch -> cost lookup.
    let cost_map: HashMap<i64, f64> = daily
        .iter()
        .map(|d| (d.day_epoch, d.cost))
        .collect();

    // "Today" in day-epoch units.
    let today_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let today_epoch = today_secs - today_secs % 86400;

    // Weekday of today (0=Sun, 1=Mon ... 6=Sat, matching epoch arithmetic).
    // Unix epoch (Jan 1 1970) was a Thursday = day 4.
    let today_day_of_week = ((today_epoch / 86400 + 4) % 7) as usize; // 0=Sun

    // We want the grid to end on "today" with today in the correct weekday row.
    // Columns go left=oldest, right=newest; rows are weekdays Sun(0)..Sat(6).
    // The rightmost column ends on today; the column starts on (today - today_dow * 86400).
    // We use 53 columns = 53 weeks.
    const COLS: usize = 53;
    const ROWS: usize = 7; // Sun=0, Mon=1, Tue=2, Wed=3, Thu=4, Fri=5, Sat=6

    // day_epoch for the cell at (row, col): the topmost-leftmost cell is
    // (today - (today_dow + ROWS*(COLS-1)) * 86400 + row*86400).
    // Simpler: compute "start_of_grid" = day_epoch of row=0, col=0.
    let grid_start = today_epoch - (today_day_of_week as i64 + (ROWS * (COLS - 1)) as i64) * 86400;

    // Collect all cost values to determine intensity thresholds.
    let mut nonzero: Vec<f64> = cost_map.values().copied().filter(|&v| v > 0.0).collect();
    nonzero.sort_by(|a: &f64, b: &f64| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let p33 = percentile(&nonzero, 33);
    let p66 = percentile(&nonzero, 66);
    let p90 = percentile(&nonzero, 90);

    // Row labels: show Mon/Wed/Fri on their respective rows (Mon=row 1, etc.)
    // Rows: 0=Sun 1=Mon 2=Tue 3=Wed 4=Thu 5=Fri 6=Sat
    let row_labels = ["   ", "Mon", "   ", "Wed", "   ", "Fri", "   "];

    let mut result: Vec<Line<'static>> = Vec::with_capacity(ROWS + 2);

    for (row, &label) in row_labels.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(COLS + 2);

        // Weekday gutter (4 chars: label + space).
        spans.push(Span::styled(
            format!("{label} "),
            Style::default().fg(Color::Rgb(100, 100, 100)),
        ));

        for col in 0..COLS {
            let day = grid_start + (col as i64 * ROWS as i64 + row as i64) * 86400;
            // Only colour cells on or before today.
            let cost = if day <= today_epoch {
                cost_map.get(&day).copied().unwrap_or(0.0)
            } else {
                // Future cell: render as empty
                -1.0
            };

            let color = if cost < 0.0 {
                // Future / out-of-range: same as empty but slightly dimmer to
                // distinguish from "zero cost today"; use same empty color.
                HEAT_EMPTY
            } else if cost == 0.0 {
                HEAT_EMPTY
            } else if cost <= p33 {
                HEAT_1
            } else if cost <= p66 {
                HEAT_2
            } else if cost <= p90 {
                HEAT_3
            } else {
                HEAT_4
            };

            spans.push(Span::styled(CELL, Style::default().fg(color)));
        }

        result.push(Line::from(spans));
    }

    // Legend row: "less [0][1][2][3][4] more"
    let legend = Line::from(vec![
        Span::styled("     less ", Style::default().fg(Color::Rgb(100, 100, 100))),
        Span::styled(CELL, Style::default().fg(HEAT_EMPTY)),
        Span::styled(CELL, Style::default().fg(HEAT_1)),
        Span::styled(CELL, Style::default().fg(HEAT_2)),
        Span::styled(CELL, Style::default().fg(HEAT_3)),
        Span::styled(CELL, Style::default().fg(HEAT_4)),
        Span::styled(" more", Style::default().fg(Color::Rgb(100, 100, 100))),
    ]);
    result.push(Line::default());
    result.push(legend);

    result
}

/// Return the value at the given percentile (0..=100) of a sorted slice.
/// Returns 0.0 for empty slices.
fn percentile(sorted: &[f64], pct: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) * pct) / 100;
    sorted[idx]
}

// ── Weekly row builder ───────────────────────────────────────────────────────

/// Build one weekly breakdown row:
/// `  YYYY-MM-DD  ████████████         $N.NNNN`
///
/// The bar is scaled linearly to `BAR_MAX_WIDTH` chars using 1/8-block
/// precision (9 block chars from space to full-block).
fn build_weekly_row(
    week_epoch: i64,
    cost: f64,
    max_cost: f64,
    palette: &Palette,
) -> Line<'static> {
    // Format the week start date as YYYY-MM-DD from unix seconds.
    let date_label = epoch_to_date(week_epoch);

    // Build the block bar.
    let bar = if max_cost <= 0.0 {
        " ".repeat(BAR_MAX_WIDTH)
    } else {
        // Scale: total units = BAR_MAX_WIDTH * 8 (sub-block precision).
        let units = ((cost / max_cost) * (BAR_MAX_WIDTH * 8) as f64).round() as usize;
        let full_blocks = units / 8;
        let remainder = units % 8;
        let mut s = String::with_capacity(BAR_MAX_WIDTH);
        for _ in 0..full_blocks {
            s.push(BAR_CHARS[8]); // full block
        }
        if full_blocks < BAR_MAX_WIDTH {
            if remainder > 0 {
                s.push(BAR_CHARS[remainder]);
                // Pad remainder of bar with spaces.
                let pad = BAR_MAX_WIDTH - full_blocks - 1;
                for _ in 0..pad {
                    s.push(' ');
                }
            } else {
                let pad = BAR_MAX_WIDTH - full_blocks;
                for _ in 0..pad {
                    s.push(' ');
                }
            }
        }
        s
    };

    let cost_label = format_cost(cost);

    Line::from(vec![
        Span::raw("  "),
        Span::styled(date_label, Style::default().fg(palette.dim)),
        Span::raw("  "),
        Span::styled(bar, Style::default().fg(palette.accent)),
        Span::raw("  "),
        Span::styled(cost_label, Style::default().fg(palette.dim)),
    ])
}

/// Convert a unix-seconds epoch to a `YYYY-MM-DD` string (UTC, manual arithmetic).
///
/// Uses the proleptic Gregorian algorithm so there is no chrono dependency.
fn epoch_to_date(ts: i64) -> String {
    // Days since unix epoch.
    let days = ts / 86400;
    // Shift to the civil epoch used by the algorithm (March 1, year 0).
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // year of era [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // month prime [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // day [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // month [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Dim bold section header (no border, top-down convention).
fn section_header(title: &'static str, palette: &Palette) -> Line<'static> {
    Line::from(Span::styled(
        title,
        Style::default()
            .fg(palette.dim)
            .add_modifier(Modifier::BOLD),
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
