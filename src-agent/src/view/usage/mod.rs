//! View — `/usage` cost and token usage dashboard.
//!
//! Two views toggled with Tab:
//! - **View A (Global)**: KPI list, range-adaptive heatmap, top-models table,
//!   per-model token bars, role split.
//! - **View B (Session)**: models-used table, hourly heatmap, session KPI totals.
//!
//! All DB queries are non-fatal (return empty/zero on missing ledger).
//!
//! # Aesthetic
//! Colors follow the user's chosen `Palette` (accent / fg / dim / sel_bg / sel_fg).
//! Per koma's house style — NO full bordered boxes. Each section is an uppercase
//! LABEL followed by a single thin horizontal rule (`Borders::BOTTOM`-equivalent),
//! then its content packed to its own height. Lots of data, almost no box-drawing
//! except the section rules and the bars / heatmap cells.
//! - Background: black (terminal default).
//! - Section labels / active-tab text / bar chars: `palette.accent`.
//! - Numeric values / model ids: `palette.fg`.
//! - Sub-labels / separators / axis labels / inactive tabs: `palette.dim`.
//! - Section rule / header bottom border: `palette.dim`.
//! - Active range tab: `palette.sel_bg` bg + `palette.sel_fg` fg (bold).
//! - Heatmap ramp (cheap->expensive): grey -> green -> yellow-green -> amber -> red.
//!
//! # Layout (View A)
//! ```text
//! koma / usage  [tab: global]  1:today 2:week 3:year  [m: cost]
//!
//! KPI ──────────────────────────────────────────────────────────────────
//! total      $0.0234
//! in         1.2M
//! cached     0
//! out        340.0k
//! calls      42
//! avg/call   $0.0006
//!
//! HEATMAP (HOURLY) ─────────────────  TOP MODELS ─────────────────────────
//! ███▇▅▃ … hourly cells                model      cost   tokens calls  %
//!                                      gpt-…    $0.012    1.2M    20  51
//!
//! ROLE SPLIT ────────────────────────────────────────────────────────────
//! main $0.018  ████████          60%  30c
//! sub  $0.012  █████             40%  12c
//!
//! [Tab] view  [1-4] range  [m] metric  [Esc] exit
//! ```

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::app::mode::{UsageMetric, UsageNavState, UsageRange, UsageView};
use crate::app::state::AppStateRest;
use crate::model::usage::{BucketSize, SpendBucket, UsageData};
use crate::view::theme::Palette;

/// Collect the `/usage` dashboard's ledger projection for the current `nav` + the
/// foreground session in `rest`.
///
/// This is the single mapping from the UI nav state (view / range) to the
/// UI-agnostic [`UsageData::collect`] primitives: it resolves the range's start
/// epoch + the heatmap's bucket granularity and the foreground session's uuid, then
/// runs the (non-fatal) ledger queries. Called by the LOCAL TUI's renderer every
/// frame (live data) AND by the daemon when it builds the snapshot the thin client
/// renders from — so both sides compute the dashboard identically from one place.
pub fn collect_usage_data(nav: &UsageNavState, rest: &AppStateRest) -> UsageData {
    let session_view = nav.view == UsageView::Session;
    let since = nav.range.since_secs();
    // Heatmap bucket granularity per range (matches the per-range chart builders):
    // hourly (24) for Today, daily for Week (7) and Year (≈year of days).
    let (heat_bucket, heat_n) = match nav.range {
        UsageRange::Today => (BucketSize::Hour, 24),
        UsageRange::Week => (BucketSize::Day, 7),
        UsageRange::Year => (BucketSize::Day, 371),
    };
    let uuid = rest
        .fg()
        .session
        .as_ref()
        .map(|s| s.id.clone())
        .unwrap_or_default();
    UsageData::collect(session_view, since, heat_bucket, heat_n, &uuid)
}

// ── Heatmap ramp (cheap -> expensive) ────────────────────────────────────────

const HEAT_EMPTY: Color = Color::Rgb(35, 35, 35);   // no data
const HEAT_1: Color = Color::Rgb(0, 120, 60);       // green  (cheap)
const HEAT_2: Color = Color::Rgb(100, 160, 50);     // yellow-green
const HEAT_3: Color = Color::Rgb(200, 140, 0);      // amber
const HEAT_4: Color = Color::Rgb(220, 50, 50);      // red   (expensive)

/// Full-block cell character used in every heatmap.
const CELL: &str = "\u{2588}";

/// Single horizontal-rule character for section underlines.
const RULE: &str = "\u{2500}";

// ── Bar / sparkline character sets ───────────────────────────────────────────

/// 8-level block chars: index 0 = space (empty), index 8 = full block.
const BAR_CHARS: [char; 9] = [
    ' ',
    '\u{258F}', '\u{258E}', '\u{258D}', '\u{258C}',
    '\u{258B}', '\u{258A}', '\u{2589}', '\u{2588}',
];

/// Max bar width for per-model token bars (chars).
const BAR_MAX_WIDTH: usize = 20;

/// Column gap between the two side-by-side sections in the middle row.
const COL_GAP: u16 = 2;

// ── KPI label column width (for vertical alignment) ──────────────────────────

/// Fixed width of the label column in the vertical KPI list.
const KPI_LABEL_W: usize = 10;

// ── Entry point ──────────────────────────────────────────────────────────────

/// Render the `/usage` dashboard every frame while `Mode::Usage` is active.
///
/// `data` is the pre-fetched ledger projection for THIS frame (see
/// [`UsageData`]): the renderer reads it instead of querying the sqlite ledger, so
/// the same draw path serves both the local TUI (which collects `data` live each
/// frame) and the daemon's thin client (which receives `data` in the snapshot and
/// has no DB of its own). The active-view counters come from `rest`'s live
/// foreground session, which `data.session_calls` complements.
pub fn draw(
    frame: &mut Frame,
    rest: &AppStateRest,
    nav: &UsageNavState,
    data: &UsageData,
    palette: &Palette,
) {
    let area = frame.area();

    // Minimum-size guard — nothing below panics on a very small terminal.
    if area.width < 20 || area.height < 6 {
        frame.render_widget(
            Paragraph::new(Span::styled("terminal too small", Style::default().fg(palette.accent))),
            area,
        );
        return;
    }

    // Three vertical zones: header | body | footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // nav line + BOTTOM rule
            Constraint::Min(0),    // sections
            Constraint::Length(1), // hotkey legend
        ])
        .split(area);

    draw_header(frame, nav, palette, outer[0]);
    draw_footer(frame, palette, outer[2]);

    match nav.view {
        UsageView::Global  => draw_global(frame, nav, data, palette, outer[1]),
        UsageView::Session => draw_session(frame, rest, nav, data, palette, outer[1]),
    }
}

// ── Nav header ─────────────────────────────────────────────────────────────────

fn draw_header(frame: &mut Frame, nav: &UsageNavState, palette: &Palette, area: Rect) {
    let view_label = match nav.view {
        UsageView::Global  => "global",
        UsageView::Session => "session",
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        "koma / usage  ",
        Style::default().fg(palette.accent).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!("[tab: {view_label}]  "),
        Style::default().fg(palette.dim),
    ));

    if nav.view == UsageView::Global {
        let ranges: &[(UsageRange, &str)] = &[
            (UsageRange::Today, "1:today"),
            (UsageRange::Week,  "2:week"),
            (UsageRange::Year,  "3:year"),
        ];
        for (r, label) in ranges {
            if *r == nav.range {
                spans.push(Span::styled(
                    format!(" {label} "),
                    Style::default()
                        .fg(palette.sel_fg)
                        .bg(palette.sel_bg)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    format!(" {label} "),
                    Style::default().fg(palette.dim),
                ));
            }
            spans.push(Span::raw("  "));
        }
        let metric_label = match nav.metric {
            UsageMetric::Cost   => "[m: cost]",
            UsageMetric::Tokens => "[m: tokens]",
        };
        spans.push(Span::styled(metric_label, Style::default().fg(palette.dim)));
    }

    // House style: a single BOTTOM rule, not a box.
    let header_block = Block::new()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim));
    let inner = header_block.inner(area);
    frame.render_widget(header_block, area);
    let margin = inner.inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(Paragraph::new(Line::from(spans)), margin);
}

// ── Footer ────────────────────────────────────────────────────────────────────

fn draw_footer(frame: &mut Frame, palette: &Palette, area: Rect) {
    let margin = area.inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(
        Paragraph::new(Span::styled(
            "[Tab] view  [1-3] range  [m] metric  [Esc] exit",
            Style::default().fg(palette.dim),
        )),
        margin,
    );
}

// ── Section primitive (label + thin rule, NO box) ──────────────────────────────

/// Draw an accent-colored uppercase section LABEL followed by a single dim
/// rule that fills the rest of the row, then return the inner content rect
/// (everything below the rule).  This is the boxless, top-down house style:
/// a header underline, never a surrounding box.
///
/// Returns a zero-height rect when `area` cannot hold the label row.
fn section(frame: &mut Frame, title: &str, palette: &Palette, area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect { x: area.x, y: area.y, width: area.width, height: 0 };
    }

    let w = area.width as usize;
    // "LABEL ───…" — one space after the label, then the rule fills the rest.
    let label_w = title.chars().count().min(w);
    // Account for label + one trailing space before the rule.
    let rule_len = w.saturating_sub(label_w + 1);
    let rule: String = RULE.repeat(rule_len);

    let line = Line::from(vec![
        Span::styled(
            title.chars().take(label_w).collect::<String>(),
            Style::default().fg(palette.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(rule, Style::default().fg(palette.dim)),
    ]);

    let label_row = Rect { x: area.x, y: area.y, width: area.width, height: 1 };
    frame.render_widget(Paragraph::new(line), label_row);

    // Content sits directly under the rule.
    Rect {
        x: area.x,
        y: area.y.saturating_add(1),
        width: area.width,
        height: area.height.saturating_sub(1),
    }
}

// ── View A: Global ────────────────────────────────────────────────────────────

fn draw_global(frame: &mut Frame, nav: &UsageNavState, data: &UsageData, palette: &Palette, area: Rect) {
    if area.height < 3 {
        return;
    }

    // Pre-fetched ledger projection for this frame — no DB access here (so the thin
    // client renders the SAME dashboard from the snapshot's `data`).
    let totals  = &data.totals;
    let models  = data.top_models.as_slice();
    let rsplit  = &data.role_split;

    // Pre-measure the side-by-side middle row so it sizes to the TALLER of its
    // two contents — never stretched to fill the screen.
    let mid_w        = area.width.saturating_sub(COL_GAP) as usize;
    let left_w       = mid_w * 45 / 100;
    let right_w      = mid_w.saturating_sub(left_w);
    let heatmap_rows = heatmap_content_height(nav);
    // models = 1 header row + 2 lines (row+bar) per model, or 1 "no data" line.
    let model_rows   = if models.is_empty() { 2 } else { 1 + models.len() * 2 };
    let mid_content  = heatmap_rows.max(model_rows).max(1);
    let mid_total    = (mid_content + 1) as u16; // +1 for the section label row

    // Role-split content height: 2 bar rows (main/sub), +1 label row.
    let role_total = 3u16;
    // KPI: 6 metric lines + 1 label row = 7.
    let kpi_total = 7u16;

    // Sections, each sized to its own content, with a single blank line between
    // them. A trailing Min(0) spacer soaks up any remaining height so nothing
    // gets stretched into an empty cavern.
    let blank = Constraint::Length(1);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(kpi_total),
            blank,
            Constraint::Length(mid_total),
            blank,
            Constraint::Length(role_total),
            Constraint::Min(0),
        ])
        .split(area);

    // KPI — vertical list.
    {
        let inner = section(frame, "KPI", palette, rows[0]);
        draw_kpi_strip(frame, totals, palette, inner);
    }

    // Middle: heatmap (left) | top-models (right), sized to the taller content.
    {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(left_w as u16),
                Constraint::Length(COL_GAP),
                Constraint::Min(0),
            ])
            .split(rows[2]);

        let heat_inner = section(frame, &heatmap_title(nav), palette, cols[0]);
        draw_heatmap(frame, nav, &data.heatmap_buckets, heat_inner, palette);

        let models_inner = section(frame, "TOP MODELS", palette, cols[2]);
        draw_models(frame, models, totals, nav, palette, models_inner, right_w);
    }

    // Role split — full-width compact section.
    {
        let inner = section(frame, "ROLE SPLIT", palette, rows[4]);
        draw_role_split(frame, rsplit, palette, inner);
    }
}

// ── KPI strip (vertical) ─────────────────────────────────────────────────────

fn draw_kpi_strip(
    frame: &mut Frame,
    totals: &crate::model::usage::RangeTotals,
    palette: &Palette,
    area: Rect,
) {
    if area.height == 0 || area.width < 10 {
        return;
    }

    let avg = if totals.calls > 0 { totals.cost / totals.calls as f64 } else { 0.0 };

    let metrics: &[(&str, String)] = &[
        ("total",    fmt_cost(totals.cost)),
        ("in",       fmt_tokens_i64(totals.tokens_in)),
        ("cached",   fmt_tokens_i64(totals.tokens_cached)),
        ("out",      fmt_tokens_i64(totals.tokens_out)),
        ("calls",    totals.calls.to_string()),
        ("avg/call", fmt_cost(avg)),
    ];

    let lines: Vec<Line<'static>> = metrics
        .iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(
                    format!("{:<KPI_LABEL_W$}", label),
                    Style::default().fg(palette.dim),
                ),
                Span::styled(value.clone(), Style::default().fg(palette.fg)),
            ])
        })
        .collect();

    let visible: Vec<Line<'static>> = lines.into_iter().take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), area);
}

// ── Heatmap section ────────────────────────────────────────────────────────────

fn heatmap_title(nav: &UsageNavState) -> String {
    let metric_label = match nav.metric {
        UsageMetric::Cost   => "COST",
        UsageMetric::Tokens => "TOKEN USAGE",
    };
    match nav.range {
        UsageRange::Today => format!("{metric_label} (HOURLY)"),
        UsageRange::Week  => format!("{metric_label} (DAILY)"),
        UsageRange::Year  => "HEATMAP (YEARLY)".to_string(),
    }
}

/// Content-row count a heatmap occupies for the active range (excludes the
/// section label row). Drives tight middle-row sizing.
fn heatmap_content_height(nav: &UsageNavState) -> usize {
    match nav.range {
        UsageRange::Today => 25, // 24 hourly rows + legend
        UsageRange::Week  => 8,  // 7 day rows (Mon–Sun) + legend
        UsageRange::Year  => 9,  // 7 day rows + blank + legend
    }
}

fn draw_heatmap(frame: &mut Frame, nav: &UsageNavState, buckets: &[SpendBucket], area: Rect, palette: &Palette) {
    if area.width < 8 || area.height == 0 {
        return;
    }

    let lines = build_heatmap(nav, buckets, area.width as usize, palette);
    let visible: Vec<Line<'static>> = lines.into_iter().take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), area);
}

// ── Top models + per-model token bars section ──────────────────────────────────

fn draw_models(
    frame: &mut Frame,
    models: &[crate::model::usage::ModelCostRange],
    totals: &crate::model::usage::RangeTotals,
    nav: &UsageNavState,
    palette: &Palette,
    area: Rect,
    width_hint: usize,
) {
    let w = (area.width as usize).max(width_hint.min(area.width as usize));
    if w < 20 || area.height == 0 {
        return;
    }

    let total_cost = totals.cost;
    let max_tokens: i64 = models.iter().map(|m| m.tokens_in + m.tokens_out).max().unwrap_or(1).max(1);

    // Fit the model-name column into available width.
    let fixed_cols = 34usize; // cost(9) + tokens(9) + calls(6) + pct(6) + sep spaces
    let col_model = w.saturating_sub(fixed_cols).clamp(8, 24);

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header row.
    lines.push(Line::from(Span::styled(
        format!(
            "{:<col_model$}  {:>9}  {:>9}  {:>6}  {:>5}",
            "model", "cost", "tokens", "calls", "%"
        ),
        Style::default().fg(palette.dim),
    )));

    for m in models {
        let id  = truncate(&m.model_id, col_model);
        let pct = if total_cost > 0.0 { (m.total_cost / total_cost * 100.0).round() as u64 } else { 0 };
        let total_tok = m.tokens_in + m.tokens_out;

        lines.push(Line::from(vec![
            Span::styled(format!("{:<col_model$}", id),             Style::default().fg(palette.fg)),
            Span::styled(format!("  {:>9}", fmt_cost(m.total_cost)),Style::default().fg(palette.fg)),
            Span::styled(format!("  {:>9}", fmt_tokens_i64(total_tok)), Style::default().fg(palette.dim)),
            Span::styled(format!("  {:>6}", m.call_count),          Style::default().fg(palette.dim)),
            Span::styled(format!("  {:>4}%", pct),                  Style::default().fg(palette.dim)),
        ]));

        // Per-model bar below the row, scaled to the metric.
        let bar_w = w.saturating_sub(col_model + 3).min(BAR_MAX_WIDTH);
        let (bar_val, bar_max) = match nav.metric {
            UsageMetric::Tokens => (total_tok, max_tokens),
            UsageMetric::Cost   => {
                let scale = 1_000_000i64;
                ((m.total_cost * scale as f64) as i64, (total_cost * scale as f64).max(1.0) as i64)
            }
        };
        let bar = build_bar(bar_val, bar_max.max(1), bar_w);
        lines.push(Line::from(vec![
            Span::raw(format!("{:<col_model$}  ", "")),
            Span::styled(bar, Style::default().fg(palette.accent)),
        ]));
    }

    if models.is_empty() {
        lines.push(Line::from(Span::styled("no data for range", Style::default().fg(palette.dim))));
    }

    let visible: Vec<Line<'static>> = lines.into_iter().take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), area);
}

// ── Role split section ──────────────────────────────────────────────────────────

fn draw_role_split(frame: &mut Frame, split: &crate::model::usage::RoleSplit, palette: &Palette, area: Rect) {
    if area.height == 0 || area.width < 12 {
        return;
    }

    let total = (split.main_cost + split.sub_cost).max(1e-12);
    let main_pct = (split.main_cost / total * 100.0).round() as u64;
    let sub_pct  = (split.sub_cost  / total * 100.0).round() as u64;

    let bar_w = (area.width as usize).saturating_sub(22).min(BAR_MAX_WIDTH);
    let total_i = (total * 1_000_000.0) as i64;
    let main_bar = build_bar((split.main_cost * 1_000_000.0) as i64, total_i, bar_w);
    let sub_bar  = build_bar((split.sub_cost  * 1_000_000.0) as i64, total_i, bar_w);

    let lines = vec![
        Line::from(vec![
            Span::styled("main ", Style::default().fg(palette.dim)),
            Span::styled(format!("{:>8}  ", fmt_cost(split.main_cost)), Style::default().fg(palette.fg)),
            Span::styled(main_bar, Style::default().fg(HEAT_1)),
            Span::styled(format!("  {:>3}%  {:>3}c", main_pct, split.main_calls), Style::default().fg(palette.dim)),
        ]),
        Line::from(vec![
            Span::styled("sub  ", Style::default().fg(palette.dim)),
            Span::styled(format!("{:>8}  ", fmt_cost(split.sub_cost)), Style::default().fg(palette.fg)),
            Span::styled(sub_bar, Style::default().fg(HEAT_3)),
            Span::styled(format!("  {:>3}%  {:>3}c", sub_pct, split.sub_calls), Style::default().fg(palette.dim)),
        ]),
    ];

    let visible: Vec<Line<'static>> = lines.into_iter().take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), area);
}

// ── View B: Session ───────────────────────────────────────────────────────────

fn draw_session(frame: &mut Frame, rest: &AppStateRest, _nav: &UsageNavState, data: &UsageData, palette: &Palette, area: Rect) {
    if area.height < 3 {
        return;
    }

    // Pre-fetched, session-scoped ledger projection for this frame (no DB access —
    // the thin client renders from the snapshot's `data`).
    let sess_models = data.session_models.as_slice();
    let hourly      = data.session_hourly.as_slice();
    // DB call count only; live `rest` counters take precedence for tokens/cost since
    // they may be ahead of the ledger (last call not yet committed, or session opened
    // without a prior ledger entry).
    let db_calls    = data.session_calls;

    // Pre-measure the side-by-side row so it sizes to the taller content.
    let mid_w     = area.width.saturating_sub(COL_GAP) as usize;
    let left_w    = mid_w * 55 / 100;
    let right_w   = mid_w.saturating_sub(left_w);
    let model_rows = if sess_models.is_empty() { 2 } else { 1 + sess_models.len() };
    let hourly_rows = if hourly.is_empty() { 1 } else { hourly.len().min(24) + 1 }; // bars + legend
    let mid_content = model_rows.max(hourly_rows).max(1);
    let mid_total   = (mid_content + 1) as u16; // +1 label row

    // Session KPI: 5 metric lines + 1 label row = 6.
    let kpi_total = 6u16;

    let blank = Constraint::Length(1);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(kpi_total),
            blank,
            Constraint::Length(mid_total),
            Constraint::Min(0),
        ])
        .split(area);

    {
        let inner = section(frame, "SESSION TOTALS", palette, rows[0]);
        draw_session_kpi(frame, rest, db_calls, palette, inner);
    }

    {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(left_w as u16),
                Constraint::Length(COL_GAP),
                Constraint::Min(0),
            ])
            .split(rows[2]);

        let models_inner = section(frame, "MODELS USED", palette, cols[0]);
        draw_session_models(frame, sess_models, palette, models_inner, left_w);

        let hourly_inner = section(frame, "HOURLY HEATMAP", palette, cols[2]);
        draw_session_hourly(frame, hourly, palette, hourly_inner, right_w);
    }
}

fn draw_session_kpi(
    frame: &mut Frame,
    rest: &AppStateRest,
    db_calls: i64,
    palette: &Palette,
    area: Rect,
) {
    if area.height == 0 || area.width < 10 {
        return;
    }

    // Use the FOREGROUND session's live counters for tokens/cost (ahead of the
    // ledger mid-session, and scoped to the active session); call count from the
    // DB totals (the runtime doesn't track a call counter).
    let fg = rest.fg();
    let metrics: &[(&str, String)] = &[
        ("in",     fmt_tokens_u64(fg.tokens_in)),
        ("cached", fmt_tokens_u64(fg.tokens_cached)),
        ("out",    fmt_tokens_u64(fg.tokens_out)),
        ("cost",   fmt_cost(fg.cost)),
        ("calls",  db_calls.to_string()),
    ];

    let lines: Vec<Line<'static>> = metrics
        .iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(
                    format!("{:<KPI_LABEL_W$}", label),
                    Style::default().fg(palette.dim),
                ),
                Span::styled(value.clone(), Style::default().fg(palette.fg)),
            ])
        })
        .collect();

    let visible: Vec<Line<'static>> = lines.into_iter().take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), area);
}

fn draw_session_models(
    frame: &mut Frame,
    models: &[crate::model::usage::ModelCostRange],
    palette: &Palette,
    area: Rect,
    width_hint: usize,
) {
    let w = (area.width as usize).max(width_hint.min(area.width as usize));
    if area.height == 0 || w < 20 {
        return;
    }

    // Fixed columns: cost(9) + tokens(9) + calls(6) + separators → 30 chars.
    // "usage" bar takes whatever is left after model name and fixed cols.
    let fixed_cols = 30usize;
    let col_model  = w.saturating_sub(fixed_cols).clamp(8, 24);
    let bar_w      = w
        .saturating_sub(col_model + fixed_cols + 2)
        .clamp(0, 12);

    let max_tokens: i64 = models
        .iter()
        .map(|m| m.tokens_in + m.tokens_out)
        .max()
        .unwrap_or(1)
        .max(1);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("{:<col_model$}  {:>9}  {:>9}  {:>6}", "model", "cost", "tokens", "calls"),
        Style::default().fg(palette.dim),
    )));

    for m in models {
        let id        = truncate(&m.model_id, col_model);
        let total_tok = m.tokens_in + m.tokens_out;
        let bar       = build_bar(total_tok, max_tokens, bar_w);
        lines.push(Line::from(vec![
            Span::styled(format!("{:<col_model$}", id),                  Style::default().fg(palette.fg)),
            Span::styled(format!("  {:>9}", fmt_cost(m.total_cost)),     Style::default().fg(palette.fg)),
            Span::styled(format!("  {:>9}", fmt_tokens_i64(total_tok)),  Style::default().fg(palette.dim)),
            Span::styled(format!("  {:>6}", m.call_count),               Style::default().fg(palette.dim)),
            // "usage" bar — token proportion relative to the top model.
            Span::styled(format!("  {bar}"),                             Style::default().fg(palette.accent)),
        ]));
    }

    if models.is_empty() {
        lines.push(Line::from(Span::styled("no usage recorded yet", Style::default().fg(palette.dim))));
    }

    let visible: Vec<Line<'static>> = lines.into_iter().take(area.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), area);
}

fn draw_session_hourly(frame: &mut Frame, hourly: &[SpendBucket], palette: &Palette, area: Rect, width_hint: usize) {
    let w = (area.width as usize).max(width_hint.min(area.width as usize));
    if area.height == 0 || w < 4 {
        return;
    }

    let lines = build_session_hourly_heatmap(hourly, palette, w);
    let h = area.height as usize;
    let skip = lines.len().saturating_sub(h);
    let visible: Vec<Line<'static>> = lines.into_iter().skip(skip).collect();
    frame.render_widget(Paragraph::new(visible), area);
}

/// Build horizontal bar chart for View B (session scope).
///
/// One row per hour (00–23) that has data in the session, each bar extending
/// rightward, colored by the heat ramp.  Same visual style as the global
/// Today heatmap.
fn build_session_hourly_heatmap(hourly: &[SpendBucket], palette: &Palette, max_width: usize) -> Vec<Line<'static>> {
    if hourly.is_empty() {
        return vec![
            Line::from(Span::styled("no data yet", Style::default().fg(palette.dim))),
        ];
    }

    let map: std::collections::HashMap<i64, &SpendBucket> =
        hourly.iter().map(|b| (b.bucket_epoch, b)).collect();

    let nonzero: Vec<f64> = hourly.iter().map(|b| b.cost).filter(|&v| v > 0.0).collect();
    let (p33, p66, p90) = percentile_thresholds(&nonzero);
    let max_val = hourly.iter().map(|b| b.cost).fold(0.0_f64, f64::max);

    // Rebuild range from session data starting at the first hour WITH data,
    // not midnight — so early empty hours are dropped and active bars render.
    let first = hourly.first().map(|b| b.bucket_epoch).unwrap_or(0);
    let last  = hourly.last().map(|b| b.bucket_epoch).unwrap_or(first);
    let n_hours = (((last - first) / 3600) + 1).clamp(1, 24) as usize;

    let label_w = 3usize; // "HH"
    let bar_w = max_width.saturating_sub(label_w).max(1);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(n_hours + 1); // bars + legend

    for i in 0..n_hours {
        let epoch = first + i as i64 * 3600;
        let v = map.get(&epoch).map(|b| b.cost).unwrap_or(0.0);
        let col = heat_color(v, p33, p66, p90, false);
        let fill = if max_val <= 0.0 {
            0usize
        } else {
            ((v / max_val) * bar_w as f64).round() as usize
        };

        let tz = crate::model::usage::local_utc_offset_secs();
        let hour = (((epoch + tz) % 86400) / 3600) as usize;
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(bar_w + 1);

        spans.push(Span::styled(
            format!("{hour:02}"),
            Style::default().fg(palette.dim).bg(HEAT_EMPTY),
        ));

        for j in 0..bar_w {
            if j < fill {
                spans.push(Span::styled(CELL, Style::default().fg(col).bg(HEAT_EMPTY)));
            } else {
                spans.push(Span::styled(CELL, Style::default().fg(HEAT_EMPTY).bg(HEAT_EMPTY)));
            }
        }

        lines.push(Line::from(spans));
    }

    lines.push(heat_legend(palette));
    lines
}

// ── Heatmap builder ──────────────────────────────────────────────────────────

/// Build range-adaptive heatmap lines driven by `nav.range` and `nav.metric`.
///
/// `buckets` is the pre-fetched spend series for the active range (hourly for
/// Today, daily for Week/Year — see [`UsageData::collect`]). Each builder maps the
/// absolute `bucket_epoch`s onto its own time grid, so no DB / `since` is needed.
///
/// | Range | Grid style                   |
/// |-------|------------------------------|
/// | Today | 24 hourly horizontal bars    |
/// | Week  | 7 daily horizontal bars      |
/// | Year  | 7 rows x 53 cols Github grid |
fn build_heatmap(nav: &UsageNavState, buckets: &[SpendBucket], max_width: usize, palette: &Palette) -> Vec<Line<'static>> {
    match nav.range {
        UsageRange::Today => build_hourly_horizontal_chart(buckets, nav.metric, max_width, palette),
        UsageRange::Week  => build_day_horizontal_chart(buckets, nav.metric, max_width, palette),
        UsageRange::Year  => build_heatmap_yearly(buckets, nav.metric, palette),
    }
}

/// Format the per-bar metric value for display at the right edge.
fn bar_metric_label(v: f64, metric: UsageMetric) -> String {
    match metric {
        UsageMetric::Cost   => fmt_cost(v),
        UsageMetric::Tokens => fmt_tokens_u64(v as u64),
    }
}

/// Width reserved for the right-aligned metric label (chars).
const METRIC_LABEL_W: usize = 9;

/// Horizontal bar chart for the Today view: one row per hour (00–23), each bar
/// extending rightward, colored by the heat ramp.  The current hour is highlighted.
/// Cost/token value is right-aligned at the end of each row.
fn build_hourly_horizontal_chart(
    buckets: &[SpendBucket],
    metric: UsageMetric,
    max_width: usize,
    palette: &Palette,
) -> Vec<Line<'static>> {
    // Index the pre-fetched buckets by epoch (cloned — the series is tiny: ≤24/7/371
    // rows — so an owned map keeps the existing `&SpendBucket` value shape downstream).
    let map: HashMap<i64, SpendBucket> = buckets.iter().cloned().map(|b| (b.bucket_epoch, b)).collect();

    let now = now_secs();
    let tz = crate::model::usage::local_utc_offset_secs();
    let local_now = now + tz;
    let today = local_now - local_now % 86400 - tz; // UTC epoch of local midnight
    let current_hour = ((local_now % 86400) / 3600) as usize;
    let epochs: Vec<i64> = (0..24).map(|i| today + i * 3600).collect();

    let values: Vec<f64> = epochs
        .iter()
        .map(|ep| map.get(ep).map(|b| metric_val(b, metric)).unwrap_or(0.0))
        .collect();

    let nonzero: Vec<f64> = values.iter().copied().filter(|&v| v > 0.0).collect();
    let (p33, p66, p90) = percentile_thresholds(&nonzero);
    let max_val = values.iter().cloned().fold(0.0_f64, f64::max);

    // Fixed label width "00" = 2 chars + 1 space.
    let label_w = 3usize;
    // Shrink bar area to make room for right-aligned metric value.
    let bar_w = max_width.saturating_sub(label_w + METRIC_LABEL_W + 1).max(1);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(25); // 24 bars + legend

    for (&v, &h) in values.iter().zip(epochs.iter()) {
        let col = heat_color(v, p33, p66, p90, false);
        let fill = if max_val <= 0.0 {
            0usize
        } else {
            ((v / max_val) * bar_w as f64).round() as usize
        };

        let hour = (((h + tz) % 86400) / 3600) as usize;
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(bar_w + 2);

        // Hour label: current hour gets bold accent, others dim.
        let label_style = if hour == current_hour {
            Style::default().fg(palette.accent).bg(HEAT_EMPTY).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.dim).bg(HEAT_EMPTY)
        };
        spans.push(Span::styled(
            format!("{hour:02}"),
            label_style,
        ));

        // Bar cells: filled portion colored, rest dark empty cells.
        for j in 0..bar_w {
            if j < fill {
                spans.push(Span::styled(CELL, Style::default().fg(col).bg(HEAT_EMPTY)));
            } else {
                spans.push(Span::styled(CELL, Style::default().fg(HEAT_EMPTY).bg(HEAT_EMPTY)));
            }
        }

        // Right-aligned metric value (always 9 chars, padded).
        let val_str = bar_metric_label(v, metric);
        spans.push(Span::styled(
            format!(" {val_str:>width$}", width = METRIC_LABEL_W - 1),
            Style::default().fg(palette.dim).bg(HEAT_EMPTY),
        ));

        lines.push(Line::from(spans));
    }

    lines.push(heat_legend(palette));
    lines
}

/// Horizontal bar chart for the Week view: one row per day (Mon–Sun), each bar
/// extending rightward, colored by the heat ramp.  Today is highlighted.
fn build_day_horizontal_chart(
    buckets: &[SpendBucket],
    metric: UsageMetric,
    max_width: usize,
    palette: &Palette,
) -> Vec<Line<'static>> {
    const DAY_LABELS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let map: HashMap<i64, SpendBucket> = buckets.iter().cloned().map(|b| (b.bucket_epoch, b)).collect();

    let now = now_secs();
    let tz = crate::model::usage::local_utc_offset_secs();
    let local_now = now + tz;
    let snap = local_now - local_now % 86400 - tz; // UTC epoch of local midnight
    let today_dow = (local_now / 86400 + 3) % 7; // Mon=0..Sun=6 in local time
    let monday = snap - today_dow * 86400;
    let epochs: Vec<i64> = (0..7).map(|i| monday + i * 86400).collect();

    let values: Vec<f64> = epochs
        .iter()
        .map(|ep| map.get(ep).map(|b| metric_val(b, metric)).unwrap_or(0.0))
        .collect();

    let nonzero: Vec<f64> = values.iter().copied().filter(|&v| v > 0.0).collect();
    let (p33, p66, p90) = percentile_thresholds(&nonzero);
    let max_val = values.iter().cloned().fold(0.0_f64, f64::max);

    let label_w = 4usize; // "Mon " = 4 chars
    let bar_w = max_width.saturating_sub(label_w + METRIC_LABEL_W + 1).max(1);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(8); // 7 bars + legend

    for (i, (&v, label)) in values.iter().zip(DAY_LABELS.iter()).enumerate() {
        let col = heat_color(v, p33, p66, p90, false);
        let fill = if max_val <= 0.0 {
            0usize
        } else {
            ((v / max_val) * bar_w as f64).round() as usize
        };

        let mut spans: Vec<Span<'static>> = Vec::with_capacity(bar_w + 2);

        let label_style = if i as i64 == today_dow {
            Style::default().fg(palette.accent).bg(HEAT_EMPTY).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.dim).bg(HEAT_EMPTY)
        };
        spans.push(Span::styled(
            format!("{label} "),
            label_style,
        ));

        for j in 0..bar_w {
            if j < fill {
                spans.push(Span::styled(CELL, Style::default().fg(col).bg(HEAT_EMPTY)));
            } else {
                spans.push(Span::styled(CELL, Style::default().fg(HEAT_EMPTY).bg(HEAT_EMPTY)));
            }
        }

        // Right-aligned metric value.
        let val_str = bar_metric_label(v, metric);
        spans.push(Span::styled(
            format!(" {val_str:>width$}", width = METRIC_LABEL_W - 1),
            Style::default().fg(palette.dim).bg(HEAT_EMPTY),
        ));

        lines.push(Line::from(spans));
    }

    lines.push(heat_legend(palette));
    lines
}


fn build_heatmap_yearly(buckets: &[SpendBucket], metric: UsageMetric, palette: &Palette) -> Vec<Line<'static>> {
    let map: HashMap<i64, SpendBucket> = buckets.iter().cloned().map(|b| (b.bucket_epoch, b)).collect();

    let now        = now_secs();
    let tz         = crate::model::usage::local_utc_offset_secs();
    let local_now  = now + tz;
    let today      = local_now - local_now % 86400 - tz; // UTC epoch of local midnight
    let today_dow  = ((local_now / 86400 + 3) % 7) as usize; // Mon=0..Sun=6 in local time

    const COLS: usize = 53;
    const ROWS: usize = 7;
    let grid_start = today - (today_dow as i64 + (ROWS * (COLS - 1)) as i64) * 86400;

    let nonzero: Vec<f64> = map.values().map(|b| metric_val(b, metric)).filter(|&v| v > 0.0).collect();
    let (p33, p66, p90) = percentile_thresholds(&nonzero);

    let row_labels = ["   ", "Mon", "   ", "Wed", "   ", "Fri", "   "];
    let mut result: Vec<Line<'static>> = Vec::with_capacity(ROWS + 2);

    for (row, &label) in row_labels.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(COLS + 1);
        // Row labels use HEAT_EMPTY (fixed dim grey) — structural axis element.
        spans.push(Span::styled(format!("{label} "), Style::default().fg(palette.dim)));
        for col in 0..COLS {
            let day    = grid_start + (col as i64 * ROWS as i64 + row as i64) * 86400;
            let future = day > today;
            let v      = if future { -1.0 } else { map.get(&day).map(|b| metric_val(b, metric)).unwrap_or(0.0) };
            spans.push(Span::styled(CELL, Style::default().fg(heat_color(v, p33, p66, p90, future))));
        }
        result.push(Line::from(spans));
    }

    result.push(Line::default());
    result.push(heat_legend(palette));
    result
}

// ── Heatmap helpers ──────────────────────────────────────────────────────────

fn heat_legend(palette: &Palette) -> Line<'static> {
    Line::from(vec![
        Span::styled("     cheap ", Style::default().fg(palette.dim)),
        Span::styled(CELL, Style::default().fg(HEAT_EMPTY)),
        Span::styled(CELL, Style::default().fg(HEAT_1)),
        Span::styled(CELL, Style::default().fg(HEAT_2)),
        Span::styled(CELL, Style::default().fg(HEAT_3)),
        Span::styled(CELL, Style::default().fg(HEAT_4)),
        Span::styled(" expensive", Style::default().fg(palette.dim)),
    ])
}


/// Map a metric value to a heatmap colour.
///
/// Special cases:
/// - `future = true` → HEAT_EMPTY regardless.
/// - `v == 0.0` → HEAT_EMPTY.
/// - All non-zero values identical (p33 == p90, i.e., uniform) → HEAT_2 (mid),
///   NOT HEAT_1, so a period with identical uniform activity reads as something
///   rather than zero.
fn heat_color(v: f64, p33: f64, p66: f64, p90: f64, future: bool) -> Color {
    if future || v < 0.0 || v == 0.0 {
        return HEAT_EMPTY;
    }
    // Uniform non-zero: all percentiles equal → mid bucket.
    if p33 >= p90 {
        return HEAT_2;
    }
    if v <= p33 { HEAT_1 }
    else if v <= p66 { HEAT_2 }
    else if v <= p90 { HEAT_3 }
    else { HEAT_4 }
}

fn metric_val(b: &SpendBucket, metric: UsageMetric) -> f64 {
    match metric {
        UsageMetric::Cost   => b.cost,
        UsageMetric::Tokens => b.tokens as f64,
    }
}

fn percentile_thresholds(nonzero: &[f64]) -> (f64, f64, f64) {
    if nonzero.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut s = nonzero.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (percentile(&s, 33), percentile(&s, 66), percentile(&s, 90))
}

fn percentile(sorted: &[f64], pct: usize) -> f64 {
    if sorted.is_empty() { return 0.0; }
    sorted[((sorted.len() - 1) * pct) / 100]
}

// ── Bar builder ──────────────────────────────────────────────────────────────

/// Horizontal block-char bar scaled to `max_width` chars (1/8-block precision).
fn build_bar(value: i64, max_val: i64, max_width: usize) -> String {
    if max_width == 0 || max_val <= 0 {
        return " ".repeat(max_width);
    }
    let v = value.max(0) as usize;
    let total_units = max_width * 8;
    let units = ((v as f64 / max_val as f64) * total_units as f64).round() as usize;
    let units = units.min(total_units);
    let full = units / 8;
    let rem  = units % 8;
    let mut s = String::with_capacity(max_width);
    for _ in 0..full { s.push(BAR_CHARS[8]); }
    if full < max_width {
        if rem > 0 {
            s.push(BAR_CHARS[rem]);
            for _ in 0..(max_width - full - 1) { s.push(' '); }
        } else {
            for _ in 0..(max_width - full) { s.push(' '); }
        }
    }
    s
}

// ── Numeric formatters ────────────────────────────────────────────────────────

/// USD cost: `$1.23` for >= $1, `$0.0045` for small values.
fn fmt_cost(cost: f64) -> String {
    if cost >= 1.0 { format!("${cost:.2}") } else { format!("${cost:.4}") }
}

/// Humanise token count: 1_234_567 -> "1.2M", 12_345 -> "12.3k", 999 -> "999".
fn fmt_tokens_i64(n: i64) -> String { fmt_tok(n as f64) }
fn fmt_tokens_u64(n: u64) -> String { fmt_tok(n as f64) }
fn fmt_tok(n: f64) -> String {
    if n >= 1_000_000.0 { format!("{:.1}M", n / 1_000_000.0) }
    else if n >= 1_000.0 { format!("{:.1}k", n / 1_000.0) }
    else { format!("{n:.0}") }
}

// ── String helpers ────────────────────────────────────────────────────────────

/// Truncate to `max` chars, appending `...` if cut.  Char-aware.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 { return String::new(); }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_owned()
    } else {
        let cut: String = chars[..max.saturating_sub(3)].iter().collect();
        cut + "..."
    }
}


// ── Time ──────────────────────────────────────────────────────────────────────

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
