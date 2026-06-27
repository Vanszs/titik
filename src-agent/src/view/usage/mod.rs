//! View — `/usage` Bloomberg-terminal cost and token usage dashboard.
//!
//! Two views toggled with Tab:
//! - **View A (Global)**: KPI strip, range-adaptive heatmap, top-models table,
//!   per-model token bars, role split, spend sparkline.
//! - **View B (Session)**: models-used table, hourly heatmap, session KPI totals.
//!
//! All DB queries are non-fatal (return empty/zero on missing ledger).
//!
//! # Bloomberg aesthetic
//! Fixed RGB colours independent of the user theme:
//! - Background: black (terminal default).
//! - Labels/borders: amber `Rgb(255,176,0)`.
//! - Numeric values: near-white `Rgb(230,230,230)`.
//! - Heatmap ramp (cheap->expensive): dim-grey -> green -> yellow-green -> amber -> red.
//!
//! # Layout (View A)
//! ```text
//! header: "koma / usage  [tab: global]  [1:today 2:week 3:month 4:year]  [m: cost]"
//! ┌─ KPI STRIP ────────────────────────────────────────────────────────────────┐
//! │ total $  in  cached  out  calls  avg/call                                  │
//! └────────────────────────────────────────────────────────────────────────────┘
//! ┌─ HEATMAP (range-adaptive) ──────┐ ┌─ TOP MODELS ───────────────────────── ┐
//! │ hourly / daily / github grid    │ │ model  cost  tokens  calls  %          │
//! └─────────────────────────────────┘ └──────────────────────────────────────  ┘
//! ┌─ ROLE SPLIT ─────────┐ ┌─ SPEND OVER TIME ──────────────────────────────  ┐
//! │ main vs sub bars     │ │ ▁▂▃▄▅▆▇█ sparkline                                │
//! └──────────────────────┘ └────────────────────────────────────────────────── ┘
//! footer: [Tab] view  [1-4] range  [m] metric  [Esc] exit
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
use crate::model::usage::{
    range_totals, role_split, session_hourly, session_models, spend_buckets,
    top_models_in_range, BucketSize, SpendBucket,
};
use crate::view::theme::Palette;

// ── Bloomberg fixed palette ──────────────────────────────────────────────────

/// Panel border and title colour: amber.
const BB_AMBER: Color = Color::Rgb(255, 176, 0);
/// Numeric value colour: near-white.
const BB_VALUE: Color = Color::Rgb(230, 230, 230);
/// Secondary label / separator colour: dim grey.
const BB_DIM: Color = Color::Rgb(80, 80, 80);
/// Active range-tab highlight background.
const BB_TAB_BG: Color = Color::Rgb(60, 40, 0);

// ── Heatmap ramp (cheap -> expensive) ────────────────────────────────────────

const HEAT_EMPTY: Color = Color::Rgb(35, 35, 35);   // no data
const HEAT_1: Color = Color::Rgb(0, 120, 60);       // green  (cheap)
const HEAT_2: Color = Color::Rgb(100, 160, 50);     // yellow-green
const HEAT_3: Color = Color::Rgb(200, 140, 0);      // amber
const HEAT_4: Color = Color::Rgb(220, 50, 50);      // red   (expensive)

/// Full-block cell character used in every heatmap.
const CELL: &str = "\u{2588}";

// ── Bar / sparkline character sets ───────────────────────────────────────────

/// 8-level block chars: index 0 = space (empty), index 8 = full block.
const BAR_CHARS: [char; 9] = [
    ' ',
    '\u{258F}', '\u{258E}', '\u{258D}', '\u{258C}',
    '\u{258B}', '\u{258A}', '\u{2589}', '\u{2588}',
];

/// 8-level sparkline chars: empty -> full.
const SPARK_CHARS: [char; 9] = [
    ' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█',
];

/// Max bar width for per-model token bars (chars).
const BAR_MAX_WIDTH: usize = 20;

// ── Entry point ──────────────────────────────────────────────────────────────

/// Render the `/usage` dashboard every frame while `Mode::Usage` is active.
pub fn draw(frame: &mut Frame, rest: &AppStateRest, nav: &UsageNavState, _palette: &Palette) {
    let area = frame.area();

    // Minimum-size guard — nothing below panics on a very small terminal.
    if area.width < 20 || area.height < 6 {
        frame.render_widget(
            Paragraph::new(Span::styled("terminal too small", Style::default().fg(BB_AMBER))),
            area,
        );
        return;
    }

    // Three vertical zones: header | panels | footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // title line + BOTTOM border
            Constraint::Min(0),    // panel grid
            Constraint::Length(1), // hotkey legend
        ])
        .split(area);

    draw_header(frame, nav, outer[0]);
    draw_footer(frame, outer[2]);

    match nav.view {
        UsageView::Global  => draw_global(frame, nav, outer[1]),
        UsageView::Session => draw_session(frame, rest, nav, outer[1]),
    }
}

// ── Header ───────────────────────────────────────────────────────────────────

fn draw_header(frame: &mut Frame, nav: &UsageNavState, area: Rect) {
    let view_label = match nav.view {
        UsageView::Global  => "global",
        UsageView::Session => "session",
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        "koma / usage  ",
        Style::default().fg(BB_AMBER).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!("[tab: {view_label}]  "),
        Style::default().fg(BB_DIM),
    ));

    if nav.view == UsageView::Global {
        let ranges: &[(UsageRange, &str)] = &[
            (UsageRange::Today, "1:today"),
            (UsageRange::Week,  "2:week"),
            (UsageRange::Month, "3:month"),
            (UsageRange::Year,  "4:year"),
        ];
        for (r, label) in ranges {
            if *r == nav.range {
                spans.push(Span::styled(
                    format!(" {label} "),
                    Style::default().fg(BB_AMBER).bg(BB_TAB_BG).add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    format!(" {label} "),
                    Style::default().fg(BB_DIM),
                ));
            }
            spans.push(Span::raw("  "));
        }
        let metric_label = match nav.metric {
            UsageMetric::Cost   => "[m: cost]",
            UsageMetric::Tokens => "[m: tokens]",
        };
        spans.push(Span::styled(metric_label, Style::default().fg(BB_DIM)));
    }

    let header_block = Block::new()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(BB_AMBER));
    let inner = header_block.inner(area);
    frame.render_widget(header_block, area);
    let margin = inner.inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(Paragraph::new(Line::from(spans)), margin);
}

// ── Footer ────────────────────────────────────────────────────────────────────

fn draw_footer(frame: &mut Frame, area: Rect) {
    let margin = area.inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(
        Paragraph::new(Span::styled(
            "[Tab] view  [1-4] range  [m] metric  [Esc] exit",
            Style::default().fg(BB_DIM),
        )),
        margin,
    );
}

// ── View A: Global ────────────────────────────────────────────────────────────

fn draw_global(frame: &mut Frame, nav: &UsageNavState, area: Rect) {
    if area.height < 4 {
        return;
    }

    let since   = nav.range.since_secs();
    let totals  = range_totals(since);
    let models  = top_models_in_range(since, 8);
    let rsplit  = role_split(since);
    let (bucket, n_buckets) = range_bucket(nav.range);
    let buckets = spend_buckets(since, bucket, n_buckets);

    // Vertical: KPI(3) | middle(min) | bottom(5)
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(5),
        ])
        .split(area);

    draw_kpi_strip(frame, &totals, rows[0]);

    // Middle: heatmap 45% | top-models 55%
    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[1]);
    draw_heatmap_panel(frame, nav, since, mid[0]);
    draw_models_panel(frame, &models, &totals, nav, mid[1]);

    // Bottom: role-split 35% | sparkline 65%
    let bot = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(rows[2]);
    draw_role_split_panel(frame, &rsplit, bot[0]);
    draw_sparkline_panel(frame, &buckets, nav, bot[1]);
}

// ── KPI strip ────────────────────────────────────────────────────────────────

fn draw_kpi_strip(frame: &mut Frame, totals: &crate::model::usage::RangeTotals, area: Rect) {
    let block = amber_panel("KPI");
    let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, area);

    if inner.width < 10 {
        return;
    }

    let avg = if totals.calls > 0 { totals.cost / totals.calls as f64 } else { 0.0 };

    let line = Line::from(vec![
        kv("total",    &fmt_cost(totals.cost)),
        dim_sep(),
        kv("in",       &fmt_tokens_i64(totals.tokens_in)),
        dim_sep(),
        kv("cached",   &fmt_tokens_i64(totals.tokens_cached)),
        dim_sep(),
        kv("out",      &fmt_tokens_i64(totals.tokens_out)),
        dim_sep(),
        kv("calls",    &totals.calls.to_string()),
        dim_sep(),
        kv("avg/call", &fmt_cost(avg)),
    ]);

    frame.render_widget(Paragraph::new(line), inner);
}

// ── Heatmap panel ────────────────────────────────────────────────────────────

fn draw_heatmap_panel(frame: &mut Frame, nav: &UsageNavState, since: i64, area: Rect) {
    let title = match nav.range {
        UsageRange::Today => "HEATMAP (hourly)",
        UsageRange::Week  => "HEATMAP (daily)",
        UsageRange::Month => "HEATMAP (daily)",
        UsageRange::Year  => "HEATMAP (yearly)",
    };
    let block = amber_panel(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 8 || inner.height < 2 {
        return;
    }

    let lines = build_heatmap(nav, since, inner.width as usize);
    let visible: Vec<Line<'static>> = lines.into_iter().take(inner.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

// ── Top models + per-model token bars panel ───────────────────────────────────

fn draw_models_panel(
    frame: &mut Frame,
    models: &[crate::model::usage::ModelCostRange],
    totals: &crate::model::usage::RangeTotals,
    nav: &UsageNavState,
    area: Rect,
) {
    let block = amber_panel("TOP MODELS");
    let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, area);

    if inner.width < 20 || inner.height < 2 {
        return;
    }

    let total_cost = totals.cost;
    let max_tokens: i64 = models.iter().map(|m| m.tokens_in + m.tokens_out).max().unwrap_or(1).max(1);

    // Fit the model-name column into available width.
    let fixed_cols = 34usize; // cost(9) + tokens(9) + calls(6) + pct(6) + sep spaces
    let col_model = (inner.width as usize).saturating_sub(fixed_cols).clamp(8, 24);

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header row.
    lines.push(Line::from(Span::styled(
        format!(
            "{:<col_model$}  {:>9}  {:>9}  {:>6}  {:>5}",
            "model", "cost", "tokens", "calls", "%"
        ),
        Style::default().fg(BB_DIM),
    )));

    for m in models {
        let id  = truncate(&m.model_id, col_model);
        let pct = if total_cost > 0.0 { (m.total_cost / total_cost * 100.0).round() as u64 } else { 0 };
        let total_tok = m.tokens_in + m.tokens_out;

        lines.push(Line::from(vec![
            Span::styled(format!("{:<col_model$}", id),             Style::default().fg(BB_VALUE)),
            Span::styled(format!("  {:>9}", fmt_cost(m.total_cost)),Style::default().fg(BB_VALUE)),
            Span::styled(format!("  {:>9}", fmt_tokens_i64(total_tok)), Style::default().fg(BB_DIM)),
            Span::styled(format!("  {:>6}", m.call_count),          Style::default().fg(BB_DIM)),
            Span::styled(format!("  {:>4}%", pct),                  Style::default().fg(BB_DIM)),
        ]));

        // Per-model bar below the row, scaled to the metric.
        let bar_w = (inner.width as usize).saturating_sub(col_model + 3).min(BAR_MAX_WIDTH);
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
            Span::styled(bar, Style::default().fg(BB_AMBER)),
        ]));
    }

    if models.is_empty() {
        lines.push(Line::from(Span::styled("no data for range", Style::default().fg(BB_DIM))));
    }

    let visible: Vec<Line<'static>> = lines.into_iter().take(inner.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

// ── Role split panel ──────────────────────────────────────────────────────────

fn draw_role_split_panel(frame: &mut Frame, split: &crate::model::usage::RoleSplit, area: Rect) {
    let block = amber_panel("ROLE SPLIT");
    let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    let total = (split.main_cost + split.sub_cost).max(1e-12);
    let main_pct = (split.main_cost / total * 100.0).round() as u64;
    let sub_pct  = (split.sub_cost  / total * 100.0).round() as u64;

    let bar_w = (inner.width as usize).saturating_sub(22).min(BAR_MAX_WIDTH);
    let total_i = (total * 1_000_000.0) as i64;
    let main_bar = build_bar((split.main_cost * 1_000_000.0) as i64, total_i, bar_w);
    let sub_bar  = build_bar((split.sub_cost  * 1_000_000.0) as i64, total_i, bar_w);

    let lines = vec![
        Line::from(vec![
            Span::styled("main ", Style::default().fg(BB_DIM)),
            Span::styled(format!("{:>8}  ", fmt_cost(split.main_cost)), Style::default().fg(BB_VALUE)),
            Span::styled(main_bar, Style::default().fg(HEAT_1)),
            Span::styled(format!("  {:>3}%  {:>3}c", main_pct, split.main_calls), Style::default().fg(BB_DIM)),
        ]),
        Line::from(vec![
            Span::styled("sub  ", Style::default().fg(BB_DIM)),
            Span::styled(format!("{:>8}  ", fmt_cost(split.sub_cost)), Style::default().fg(BB_VALUE)),
            Span::styled(sub_bar, Style::default().fg(HEAT_3)),
            Span::styled(format!("  {:>3}%  {:>3}c", sub_pct, split.sub_calls), Style::default().fg(BB_DIM)),
        ]),
    ];

    let visible: Vec<Line<'static>> = lines.into_iter().take(inner.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

// ── Spend-over-time sparkline panel ───────────────────────────────────────────

fn draw_sparkline_panel(
    frame: &mut Frame,
    buckets: &[SpendBucket],
    nav: &UsageNavState,
    area: Rect,
) {
    let block = amber_panel("SPEND OVER TIME");
    let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, area);

    if inner.width < 4 || inner.height < 1 {
        return;
    }

    let values: Vec<f64> = buckets
        .iter()
        .map(|b| match nav.metric {
            UsageMetric::Cost   => b.cost,
            UsageMetric::Tokens => b.tokens as f64,
        })
        .collect();

    let spark = build_sparkline(&values, inner.width as usize);
    let visible = vec![Line::from(Span::styled(spark, Style::default().fg(BB_AMBER)))];
    frame.render_widget(Paragraph::new(visible), inner);
}

// ── View B: Session ───────────────────────────────────────────────────────────

fn draw_session(frame: &mut Frame, rest: &AppStateRest, _nav: &UsageNavState, area: Rect) {
    if area.height < 4 {
        return;
    }

    let uuid = rest.session.as_ref().map(|s| s.id.clone()).unwrap_or_default();
    let sess_models = session_models(&uuid);
    let hourly      = session_hourly(&uuid);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    draw_session_kpi(frame, rest, rows[0]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(rows[1]);

    draw_session_models(frame, &sess_models, cols[0]);
    draw_session_hourly(frame, &hourly, cols[1]);
}

fn draw_session_kpi(frame: &mut Frame, rest: &AppStateRest, area: Rect) {
    let block = amber_panel("SESSION TOTALS");
    let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, area);

    if inner.width < 10 {
        return;
    }

    let line = Line::from(vec![
        kv("in",     &fmt_tokens_u64(rest.tokens_in)),
        dim_sep(),
        kv("cached", &fmt_tokens_u64(rest.tokens_cached)),
        dim_sep(),
        kv("out",    &fmt_tokens_u64(rest.tokens_out)),
        dim_sep(),
        kv("cost",   &fmt_cost(rest.cost)),
    ]);
    frame.render_widget(Paragraph::new(line), inner);
}

fn draw_session_models(frame: &mut Frame, models: &[crate::model::usage::ModelCostRange], area: Rect) {
    let block = amber_panel("MODELS USED");
    let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, area);

    if inner.height < 2 || inner.width < 20 {
        return;
    }

    let fixed_cols = 30usize;
    let col_model = (inner.width as usize).saturating_sub(fixed_cols).clamp(8, 24);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("{:<col_model$}  {:>9}  {:>9}  {:>6}", "model", "cost", "tokens", "calls"),
        Style::default().fg(BB_DIM),
    )));

    for m in models {
        let id = truncate(&m.model_id, col_model);
        let total_tok = m.tokens_in + m.tokens_out;
        lines.push(Line::from(vec![
            Span::styled(format!("{:<col_model$}", id),              Style::default().fg(BB_VALUE)),
            Span::styled(format!("  {:>9}", fmt_cost(m.total_cost)), Style::default().fg(BB_VALUE)),
            Span::styled(format!("  {:>9}", fmt_tokens_i64(total_tok)), Style::default().fg(BB_DIM)),
            Span::styled(format!("  {:>6}", m.call_count),           Style::default().fg(BB_DIM)),
        ]));
    }

    if models.is_empty() {
        lines.push(Line::from(Span::styled("no usage recorded yet", Style::default().fg(BB_DIM))));
    }

    let visible: Vec<Line<'static>> = lines.into_iter().take(inner.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn draw_session_hourly(frame: &mut Frame, hourly: &[SpendBucket], area: Rect) {
    let block = amber_panel("HOURLY SPEND");
    let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, area);

    if inner.height < 1 || inner.width < 8 {
        return;
    }

    let values: Vec<f64> = hourly.iter().map(|b| b.cost).collect();
    let spark = build_sparkline(&values, inner.width as usize);
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(spark, Style::default().fg(BB_AMBER))),
    ];

    // Hour labels on the second line if space permits.
    if hourly.len() > 1 && inner.height >= 2 {
        let label: String = hourly
            .iter()
            .map(|b| format!("{:02}", (b.bucket_epoch / 3600) % 24))
            .take(inner.width as usize / 2)
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(Line::from(Span::styled(
            truncate_str(&label, inner.width as usize),
            Style::default().fg(BB_DIM),
        )));
    }

    let visible: Vec<Line<'static>> = lines.into_iter().take(inner.height as usize).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

// ── Heatmap builder ──────────────────────────────────────────────────────────

/// Build range-adaptive heatmap lines driven by `nav.range` and `nav.metric`.
///
/// | Range | Grid style               |
/// |-------|--------------------------|
/// | Today | 24 hourly cells (1 row)  |
/// | Week  | 7 daily cells (1 row)    |
/// | Month | 30 daily cells (1 row)   |
/// | Year  | 7 rows x 53 cols Github  |
fn build_heatmap(nav: &UsageNavState, since: i64, max_width: usize) -> Vec<Line<'static>> {
    match nav.range {
        UsageRange::Today => build_heatmap_hourly(since, nav.metric, max_width),
        UsageRange::Week  => build_heatmap_daily(since, 7,  nav.metric),
        UsageRange::Month => build_heatmap_daily(since, 30, nav.metric),
        UsageRange::Year  => build_heatmap_yearly(since, nav.metric),
    }
}

fn build_heatmap_hourly(since: i64, metric: UsageMetric, max_width: usize) -> Vec<Line<'static>> {
    let buckets = spend_buckets(since, BucketSize::Hour, 24);
    let map: HashMap<i64, SpendBucket> = buckets.into_iter().map(|b| (b.bucket_epoch, b)).collect();

    let now = now_secs();
    let cur_hour   = now   - now   % 3600;
    let start_hour = since - since % 3600;
    let n = (((cur_hour - start_hour) / 3600) + 1).max(1) as usize;
    let n = n.min(24).min(max_width.saturating_sub(4));

    let nonzero: Vec<f64> = map.values().map(|b| metric_val(b, metric)).filter(|&v| v > 0.0).collect();
    let (p33, p66, p90) = percentile_thresholds(&nonzero);

    let mut cells: Vec<Span<'static>> = vec![Span::styled("   ", Style::default().fg(BB_DIM))];
    let mut labels: Vec<Span<'static>> = vec![Span::raw("   ")];

    for i in 0..n {
        let epoch = start_hour + i as i64 * 3600;
        let v     = map.get(&epoch).map(|b| metric_val(b, metric)).unwrap_or(0.0);
        let col   = heat_color(v, p33, p66, p90, epoch > cur_hour);
        cells.push(Span::styled(CELL, Style::default().fg(col)));

        let h = ((epoch / 3600) % 24) as u8;
        if h.is_multiple_of(4) {
            labels.push(Span::styled(format!("{h:02}"), Style::default().fg(BB_DIM)));
        } else {
            labels.push(Span::raw(" "));
        }
    }

    vec![Line::from(cells), Line::from(labels), heat_legend()]
}

fn build_heatmap_daily(since: i64, days: usize, metric: UsageMetric) -> Vec<Line<'static>> {
    let buckets = spend_buckets(since, BucketSize::Day, days);
    let map: HashMap<i64, SpendBucket> = buckets.into_iter().map(|b| (b.bucket_epoch, b)).collect();

    let today = { let n = now_secs(); n - n % 86400 };

    let nonzero: Vec<f64> = map.values().map(|b| metric_val(b, metric)).filter(|&v| v > 0.0).collect();
    let (p33, p66, p90) = percentile_thresholds(&nonzero);

    let mut spans: Vec<Span<'static>> = vec![Span::styled("   ", Style::default().fg(BB_DIM))];
    for i in 0..days {
        let epoch = today - (days as i64 - 1 - i as i64) * 86400;
        let v     = map.get(&epoch).map(|b| metric_val(b, metric)).unwrap_or(0.0);
        let col   = heat_color(v, p33, p66, p90, false);
        spans.push(Span::styled(CELL, Style::default().fg(col)));
    }

    vec![Line::from(spans), heat_legend()]
}

fn build_heatmap_yearly(since: i64, metric: UsageMetric) -> Vec<Line<'static>> {
    let buckets = spend_buckets(since, BucketSize::Day, 371);
    let map: HashMap<i64, SpendBucket> = buckets.into_iter().map(|b| (b.bucket_epoch, b)).collect();

    let now        = now_secs();
    let today      = now - now % 86400;
    let today_dow  = ((today / 86400 + 4) % 7) as usize; // 0=Sun..6=Sat

    const COLS: usize = 53;
    const ROWS: usize = 7;
    let grid_start = today - (today_dow as i64 + (ROWS * (COLS - 1)) as i64) * 86400;

    let nonzero: Vec<f64> = map.values().map(|b| metric_val(b, metric)).filter(|&v| v > 0.0).collect();
    let (p33, p66, p90) = percentile_thresholds(&nonzero);

    let row_labels = ["   ", "Mon", "   ", "Wed", "   ", "Fri", "   "];
    let mut result: Vec<Line<'static>> = Vec::with_capacity(ROWS + 2);

    for (row, &label) in row_labels.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(COLS + 1);
        spans.push(Span::styled(format!("{label} "), Style::default().fg(BB_DIM)));
        for col in 0..COLS {
            let day    = grid_start + (col as i64 * ROWS as i64 + row as i64) * 86400;
            let future = day > today;
            let v      = if future { -1.0 } else { map.get(&day).map(|b| metric_val(b, metric)).unwrap_or(0.0) };
            spans.push(Span::styled(CELL, Style::default().fg(heat_color(v, p33, p66, p90, future))));
        }
        result.push(Line::from(spans));
    }

    result.push(Line::default());
    result.push(heat_legend());
    result
}

// ── Heatmap helpers ──────────────────────────────────────────────────────────

fn heat_legend() -> Line<'static> {
    Line::from(vec![
        Span::styled("     cheap ", Style::default().fg(BB_DIM)),
        Span::styled(CELL, Style::default().fg(HEAT_EMPTY)),
        Span::styled(CELL, Style::default().fg(HEAT_1)),
        Span::styled(CELL, Style::default().fg(HEAT_2)),
        Span::styled(CELL, Style::default().fg(HEAT_3)),
        Span::styled(CELL, Style::default().fg(HEAT_4)),
        Span::styled(" expensive", Style::default().fg(BB_DIM)),
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

// ── Bar / sparkline builders ─────────────────────────────────────────────────

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

/// Sparkline: down/up-sample `values` to exactly `width` block chars.
fn build_sparkline(values: &[f64], width: usize) -> String {
    if values.is_empty() || width == 0 {
        return " ".repeat(width);
    }
    let max = values.iter().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return " ".repeat(width);
    }
    let mut out = String::with_capacity(width);
    for i in 0..width {
        let idx   = (i * values.len()) / width;
        let v     = values.get(idx).copied().unwrap_or(0.0);
        let level = ((v / max) * 8.0).round() as usize;
        out.push(SPARK_CHARS[level.min(8)]);
    }
    out
}

// ── Range -> bucket granularity ──────────────────────────────────────────────

fn range_bucket(range: UsageRange) -> (BucketSize, usize) {
    match range {
        UsageRange::Today => (BucketSize::Hour, 24),
        UsageRange::Week  => (BucketSize::Day,   7),
        UsageRange::Month => (BucketSize::Day,  30),
        UsageRange::Year  => (BucketSize::Week, 53),
    }
}

// ── Panel primitive ───────────────────────────────────────────────────────────

/// Amber-bordered panel block with a bold title on the top rail.
/// Call `block.inner(area)` to get the inner rect after rendering.
fn amber_panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_style(Style::default().fg(BB_AMBER))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(BB_AMBER).add_modifier(Modifier::BOLD),
        ))
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

// ── Span helpers ─────────────────────────────────────────────────────────────

/// `label: VALUE  ` with value in near-white.
fn kv(label: &'static str, value: &str) -> Span<'static> {
    Span::styled(
        format!("{label}: {value}  "),
        Style::default().fg(BB_VALUE),
    )
}

/// Dim pipe separator between KPI fields.
fn dim_sep() -> Span<'static> {
    Span::styled("| ", Style::default().fg(BB_DIM))
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

fn truncate_str(s: &str, max: usize) -> String { truncate(s, max) }

// ── Time ──────────────────────────────────────────────────────────────────────

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
