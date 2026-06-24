//! View – in-app agents management dashboard (Agents mode).
//!
//! Two-pane layout, mirroring `/settings`: a narrow sidebar LISTs every agent
//! (with a source tag); the detail pane on the right shows the selected agent
//! read-only (Browse), an editable field form (Edit/Create), or a confirm
//! prompt (DeleteConfirm). A context-sensitive footer shows key hints.
//!
//! Border convention (strict, matches project rules + `/settings`):
//! - Header: `Borders::BOTTOM` only.
//! - List/detail divider: `Borders::RIGHT` on the list pane.
//! - Footer: plain dim line (no full box anywhere).
//!
//! ```text
//!  agents
//! ─────────────────────────────────────────────────────────
//! │ explore  built-in │  name         my-agent
//! │ general  built-in │  description  Does the thing
//! │ my-agent session  │  model        (inherit)
//!                     │  prompt       You are a focused subagent…
//!
//!  ↑/↓ pick · →/Enter edit · n new · d delete · Esc close
//! ```
//!
//! All draft mutation lives in [`crate::app::mode::AgentsState`]; key handling
//! lives in [`crate::controller::input::handle_agents`].

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::app::mode::agents::source_label;
use crate::app::mode::{AgentEditField, AgentSubMode, AgentsState};
use crate::view::theme::Palette;

/// List (sidebar) column width in terminal columns (includes the RIGHT border).
const SIDEBAR_W: u16 = 26;

/// Truncate `s` to at most `max` chars, appending `…` if cut.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let cut = max.saturating_sub(1);
        chars[..cut].iter().collect::<String>() + "…"
    }
}

/// Render the agents dashboard for `st` using the given colour `palette`.
///
/// All colours flow through `palette` — no hardcoded `Color::` values.
pub fn draw(frame: &mut Frame, st: &AgentsState, palette: &Palette) {
    // Outer vertical zones: header | body | footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header text + BOTTOM border
            Constraint::Min(0),    // list + detail
            Constraint::Length(1), // footer key hints
        ])
        .split(frame.area());

    // --- Header ---
    let header_block = Block::new()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim));
    let header_inner = header_block.inner(outer[0]);
    frame.render_widget(header_block, outer[0]);
    frame.render_widget(
        Paragraph::new(Span::styled("agents", Style::default().fg(palette.dim))),
        header_inner.inner(Margin { horizontal: 2, vertical: 0 }),
    );

    // --- Body: list sidebar + detail pane ---
    let body_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(SIDEBAR_W), // list with RIGHT border as divider
            Constraint::Min(0),            // detail pane
        ])
        .split(outer[1]);

    draw_list(frame, st, palette, body_cols[0]);
    draw_detail(frame, st, palette, body_cols[1]);

    // --- Footer ---
    let footer_area = outer[2].inner(Margin { horizontal: 2, vertical: 0 });
    let hint = footer_hint(st);
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(palette.dim)),
        footer_area,
    );
}

/// Render the LIST pane: one row per agent (`name` + source tag), RIGHT border.
fn draw_list(
    frame: &mut Frame,
    st: &AgentsState,
    palette: &Palette,
    area: ratatui::layout::Rect,
) {
    let block = Block::new()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(palette.dim));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content = inner.inner(Margin { horizontal: 1, vertical: 1 });
    // Focus lives in the LIST only while Browsing and not in the detail pane.
    let list_focused = st.mode == AgentSubMode::Browse && !st.in_detail;

    let lines: Vec<Line> = if st.agents.is_empty() {
        vec![Line::from(Span::styled(
            "(no agents)",
            Style::default().fg(palette.dim),
        ))]
    } else {
        let name_w = (content.width as usize).saturating_sub(12).max(4);
        st.agents
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let selected = i == st.list_sel;
                let (marker, color) = if selected {
                    let c = if list_focused { palette.accent } else { palette.dim };
                    ("› ", c)
                } else {
                    ("  ", palette.dim)
                };
                let name = truncate(&a.name, name_w);
                Line::from(vec![
                    Span::styled(marker, Style::default().fg(color)),
                    Span::styled(format!("{name:<width$}", width = name_w), Style::default().fg(color)),
                    Span::styled(" ", Style::default()),
                    Span::styled(source_label(a.source), Style::default().fg(palette.dim)),
                ])
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), content);
}

/// Render the DETAIL pane based on the active sub-mode.
fn draw_detail(
    frame: &mut Frame,
    st: &AgentsState,
    palette: &Palette,
    area: ratatui::layout::Rect,
) {
    let inner = area.inner(Margin { horizontal: 2, vertical: 1 });
    let lines = match st.mode {
        AgentSubMode::Browse => browse_lines(st, palette, inner.width as usize),
        AgentSubMode::Edit | AgentSubMode::Create => editor_lines(st, palette, inner.width as usize),
        AgentSubMode::DeleteConfirm => delete_lines(st, palette),
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Detail rows for Browse: the selected agent's metadata + a body preview.
fn browse_lines<'a>(st: &'a AgentsState, palette: &Palette, width: usize) -> Vec<Line<'a>> {
    let Some(a) = st.current_agent() else {
        return vec![Line::from(Span::styled(
            "no agent selected",
            Style::default().fg(palette.dim),
        ))];
    };
    let value_w = width.saturating_sub(14).max(4);
    let mut lines = Vec::new();

    let row = |label: &str, value: String, color: Color| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{label:<14}"), Style::default().fg(palette.dim)),
            Span::styled(value, Style::default().fg(color)),
        ])
    };

    lines.push(row("name", a.name.clone(), palette.accent));
    lines.push(row("source", source_label(a.source).to_string(), palette.fg));
    lines.push(row(
        "description",
        truncate(&a.description, value_w),
        palette.fg,
    ));
    lines.push(row(
        "model",
        match &a.model {
            Some(m) => truncate(m, value_w),
            None => "(inherit)".to_string(),
        },
        if a.model.is_some() { palette.fg } else { palette.dim },
    ));
    lines.push(row(
        "provider",
        match &a.provider {
            Some(p) => truncate(p, value_w),
            None => "(default)".to_string(),
        },
        if a.provider.is_some() { palette.fg } else { palette.dim },
    ));
    let tools = if a.tools.is_empty() {
        "(read-only default)".to_string()
    } else {
        truncate(&a.tools.join(", "), value_w)
    };
    lines.push(row(
        "tools",
        tools,
        if a.tools.is_empty() { palette.dim } else { palette.fg },
    ));

    // Body preview: a label row, then the first few prompt lines, dim.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "prompt",
        Style::default().fg(palette.dim),
    )));
    let preview_w = width.saturating_sub(2).max(4);
    for raw in a.prompt.lines().take(8) {
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(raw, preview_w)),
            Style::default().fg(palette.fg),
        )));
    }
    lines
}

/// Detail rows for Edit / Create: one labelled draft field per row.
fn editor_lines<'a>(st: &'a AgentsState, palette: &Palette, width: usize) -> Vec<Line<'a>> {
    let value_w = width.saturating_sub(16).max(4);
    let mut lines = Vec::new();

    // Create shows the chosen scope on its own (non-editing) top row.
    if st.mode == AgentSubMode::Create {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(format!("{:<14}", "scope"), Style::default().fg(palette.dim)),
            Span::styled(st.create_scope.label(), Style::default().fg(palette.accent)),
            Span::styled("  (←/→ toggle)", Style::default().fg(palette.dim)),
        ]));
    }

    for &f in st.fields() {
        let selected = f == st.field;
        let editing_here = st.editing && selected;
        let marker = Span::styled(
            if selected { "› " } else { "  " },
            Style::default().fg(palette.accent),
        );
        let label_color = if selected { palette.accent } else { palette.dim };
        let label = Span::styled(
            format!("{:<14}", f.label()),
            Style::default().fg(label_color),
        );

        if f == AgentEditField::Body {
            // Body label row, then the multiline draft beneath it. The active
            // line carries a block cursor while editing this field.
            let mut header = vec![marker, label];
            if editing_here {
                header.push(Span::styled("(editing)", Style::default().fg(palette.dim)));
            }
            lines.push(Line::from(header));
            let body_w = width.saturating_sub(2).max(4);
            let body = &st.draft_body;
            let body_lines: Vec<&str> = if body.is_empty() {
                vec![""]
            } else {
                body.lines().collect()
            };
            let last = body_lines.len().saturating_sub(1);
            for (i, bl) in body_lines.iter().enumerate() {
                let mut text = truncate(bl, body_w);
                if editing_here && i == last {
                    text.push('█');
                }
                lines.push(Line::from(Span::styled(
                    format!("  {text}"),
                    Style::default().fg(palette.fg),
                )));
            }
            continue;
        }

        // Single-line text fields.
        let raw = st.draft(f);
        let (shown, color) = if raw.is_empty() && !editing_here {
            let ph = match f {
                AgentEditField::Name => "(required)",
                AgentEditField::Description => "(required)",
                AgentEditField::Model => "(inherit)",
                AgentEditField::Provider => "(default)",
                AgentEditField::Tools => "(read-only default)",
                AgentEditField::Body => "",
            };
            (ph.to_string(), palette.dim)
        } else {
            let trunc_w = if editing_here { value_w.saturating_sub(1) } else { value_w };
            let mut s = truncate(raw, trunc_w);
            if editing_here {
                s.push('█');
            }
            (s, palette.fg)
        };
        lines.push(Line::from(vec![
            marker,
            label,
            Span::styled(shown, Style::default().fg(color)),
        ]));
    }
    lines
}

/// Detail rows for DeleteConfirm: a one-line `y`/`n` prompt.
fn delete_lines<'a>(st: &'a AgentsState, palette: &Palette) -> Vec<Line<'a>> {
    let name = st
        .current_agent()
        .map(|a| a.name.as_str())
        .unwrap_or("?");
    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("delete ", Style::default().fg(palette.fg)),
            Span::styled(format!("'{name}'"), Style::default().fg(palette.accent)),
            Span::styled("?", Style::default().fg(palette.fg)),
        ]),
        Line::from(Span::styled(
            "this removes the file from disk",
            Style::default().fg(palette.dim),
        )),
    ]
}

/// Context-sensitive footer hint for the active sub-mode.
fn footer_hint(st: &AgentsState) -> &'static str {
    match st.mode {
        AgentSubMode::DeleteConfirm => "y delete · n/Esc cancel",
        AgentSubMode::Create => {
            if st.editing {
                "type to edit · Ctrl+J newline (prompt) · Enter/Esc done"
            } else {
                "↑/↓ field · ←/→ scope · Enter edit · s create · Esc cancel"
            }
        }
        AgentSubMode::Edit => {
            if st.editing {
                "type to edit · Ctrl+J newline (prompt) · Enter/Esc done"
            } else {
                "↑/↓ field · Enter edit · s save · Esc cancel"
            }
        }
        AgentSubMode::Browse => "↑/↓ pick · →/Enter edit · n new · d delete · Esc close",
    }
}
