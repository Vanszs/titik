//! Chat screen renderer: the read-only view of [`AppStateRest`].
//!
//! Last stage of the keystroke -> Action -> state -> render flow. Pure
//! function of state: it borrows the session transcript, the live streaming
//! buffer, the input, and the status line into a structured layout:
//!
//! ```text
//! simple-coder · {name} [{model}]          ← header (1 line)
//! ─────────────────────────────────────────  ← dim bottom border
//! [messages / transcript area]             ← scrollable, fills space,
//!                                             auto-follows the bottom
//! ─────────────────────────────────────────  ← dim top border (input block)
//! › {input buffer}                         ← input line (1 line)
//! ─────────────────────────────────────────  ← dim bottom border (input block)
//! {status}                                 ← status line (1 line)
//! ```
//!
//! The header has a dim bottom border and horizontal padding of 2. The input
//! has dim top + bottom borders and horizontal padding of 2. The transcript
//! itself is flat (no borders); structure comes from spacing and colour.
//!
//! Each message is rendered as a block with a coloured bullet on the first
//! line and continuation lines hanging-indented (2 cols) under the text.
//! A blank line separates consecutive blocks. The transcript auto-follows the
//! bottom as responses stream; scrolling up pauses following, reaching the
//! bottom resumes it.
//!
//! - User messages   → `★` in `palette.accent`
//! - AI / streaming  → `●` in `palette.fg`
//! - System / status → `palette.dim`
//!
//! When the user types `/` followed by a command name with no whitespace, a
//! bordered popup palette floats above the input row listing the matching
//! commands filtered in real time. Up/Down navigate the list; Tab completes.

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
    Frame,
};
use crate::app::state::AppStateRest;
use crate::config::{APP_TITLE, DEFAULT_MODEL};
use crate::controller::command;
use crate::dto::chat::Role;
use crate::view::theme::Palette;

/// Render the chat screen from `rest` using the given colour `palette`.
///
/// Borrows throughout — no per-frame clones of the transcript or streaming
/// buffer. The header has a dim bottom border + padding; the input has dim
/// top + bottom borders + padding; the transcript is flat.
pub fn draw(frame: &mut Frame, rest: &AppStateRest, palette: &Palette) {
    // Layout: header (text + bottom rule) | transcript | input (top+bottom
    // rules) | status. Header/input get thin dim borders so the screen reads
    // as structured, not boxed; the transcript itself stays flat.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header line + bottom border
            Constraint::Min(1),    // transcript
            Constraint::Length(3), // top border + input line + bottom border
            Constraint::Length(1), // status bar
        ])
        .split(frame.area());

    // --- Header --- `simple-coder · {name} [{model}]`, dim bottom border.
    let (name, model): (&str, &str) = match rest.session.as_ref() {
        Some(s) => (s.name.as_str(), s.settings.model.as_str()),
        None => (APP_TITLE, DEFAULT_MODEL),
    };
    let header_line = Line::from(vec![
        Span::styled("simple-coder · ", Style::default().fg(palette.dim)),
        Span::styled(name, Style::default().fg(palette.accent)),
        Span::styled(" [", Style::default().fg(palette.dim)),
        Span::styled(model, Style::default().fg(palette.dim)),
        Span::styled("]", Style::default().fg(palette.dim)),
    ]);
    let header_block = Block::new()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim))
        .padding(Padding::horizontal(2));
    let header_inner = header_block.inner(chunks[0]);
    frame.render_widget(header_block, chunks[0]);
    frame.render_widget(Paragraph::new(header_line), header_inner);

    // --- Transcript ---
    // Padded, flat. Each message is a block: a coloured bullet (★ user / ● ai)
    // on the first line, text hanging-indented under it, blank line between
    // blocks. Pre-wrapped by hand for the hanging indent.
    let body = chunks[1].inner(Margin { horizontal: 2, vertical: 1 });
    let wrap_w = (body.width as usize).saturating_sub(2).max(1);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut first = true;
    if let Some(session) = rest.session.as_ref() {
        for msg in session.conversation.messages() {
            let (color, bullet) = match msg.role {
                Role::System => continue,
                Role::User => (palette.accent, "★ "),
                Role::Assistant => (palette.fg, "● "),
            };
            push_message(&mut lines, &msg.content, color, bullet, wrap_w, &mut first);
        }
    }
    if let Some(buf) = rest.streaming.as_ref() {
        if !buf.is_empty() {
            push_message(&mut lines, buf, palette.fg, "● ", wrap_w, &mut first);
        }
    }

    // Scroll model: follow pins to the bottom (auto-scrolls as content grows);
    // otherwise show the stored offset, clamped. Publish max_scroll so the key/
    // mouse handlers can clamp + detect bottom.
    let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let max_scroll = total.saturating_sub(body.height);
    rest.last_max_scroll.set(max_scroll);
    let top = if rest.follow { max_scroll } else { rest.scroll.min(max_scroll) };
    let messages = Paragraph::new(lines).scroll((top, 0));
    frame.render_widget(messages, body);

    // --- Input line --- `› {input}`, dim top + bottom borders.
    let input_line = Line::from(vec![
        Span::styled("› ", Style::default().fg(palette.accent)),
        Span::raw(rest.input.as_str()),
    ]);
    let input_block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim))
        .padding(Padding::horizontal(2));
    let input_inner = input_block.inner(chunks[2]);
    frame.render_widget(input_block, chunks[2]);
    frame.render_widget(Paragraph::new(input_line), input_inner);

    // --- Status bar --- padded to align with the rest.
    let status_area = chunks[3].inner(Margin { horizontal: 2, vertical: 0 });
    frame.render_widget(
        Paragraph::new(rest.status.as_str()).style(Style::default().fg(palette.dim)),
        status_area,
    );

    // --- Slash command palette --- floats above the input while typing a `/`
    // command name. Cleared background so it overlays the transcript.
    let cmd_matches = command::palette_matches(&rest.input);
    if !cmd_matches.is_empty() {
        let sel = rest.palette_sel.min(cmd_matches.len() - 1);
        let rows: Vec<Line> = cmd_matches
            .iter()
            .enumerate()
            .map(|(i, (name, desc))| {
                if i == sel {
                    let hl = Style::default().fg(palette.sel_fg).bg(palette.sel_bg);
                    Line::from(vec![
                        Span::styled(format!(" {name}  "), hl),
                        Span::styled(format!("{desc} "), hl),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled(format!(" {name}  "), Style::default().fg(palette.accent)),
                        Span::styled(*desc, Style::default().fg(palette.dim)),
                    ])
                }
            })
            .collect();

        // Size the box to the list (+2 for borders), clamped to the space
        // between the header rule and the input, and anchor it just above input.
        let avail = chunks[2].y.saturating_sub(chunks[1].y);
        let h = ((rows.len() as u16) + 2).min(avail.max(3));
        let y = chunks[2].y.saturating_sub(h);
        let popup = Rect { x: chunks[2].x, y, width: chunks[2].width, height: h };

        let block = Block::bordered()
            .border_style(Style::default().fg(palette.dim))
            .title(Span::styled(" commands ", Style::default().fg(palette.dim)))
            .padding(Padding::horizontal(1));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        frame.render_widget(Paragraph::new(rows), inner);
    }

    // --- Help overlay --- opened with `/help`. Same flat box style as the
    // slash-command palette, anchored just above the input. Modal: any key
    // closes it (handled in the input controller).
    if rest.help_open {
        let mut rows: Vec<Line> = Vec::new();
        rows.push(Line::from(Span::styled(
            "commands",
            Style::default().fg(palette.dim),
        )));
        for (name, desc) in command::COMMANDS {
            rows.push(Line::from(vec![
                Span::styled(format!("  {name:<12}"), Style::default().fg(palette.accent)),
                Span::styled(*desc, Style::default().fg(palette.fg)),
            ]));
        }
        rows.push(Line::from(""));
        rows.push(Line::from(Span::styled("keys", Style::default().fg(palette.dim))));
        let keys: &[(&str, &str)] = &[
            ("Enter", "send message / run command"),
            ("Tab", "complete the selected command"),
            ("Ctrl+R", "resend the last message"),
            ("Esc", "interrupt while busy, else quit"),
            ("Up/Down/wheel", "scroll the transcript"),
        ];
        for (k, v) in keys {
            rows.push(Line::from(vec![
                Span::styled(format!("  {k:<14}"), Style::default().fg(palette.accent)),
                Span::styled(*v, Style::default().fg(palette.fg)),
            ]));
        }

        // Anchor just above the input, full input width, growing upward —
        // identical placement to the slash-command palette.
        let avail = chunks[2].y.saturating_sub(chunks[1].y);
        let h = ((rows.len() as u16) + 2).min(avail.max(3));
        let y = chunks[2].y.saturating_sub(h);
        let rect = Rect { x: chunks[2].x, y, width: chunks[2].width, height: h };

        let block = Block::bordered()
            .border_style(Style::default().fg(palette.dim))
            .title(Span::styled(" help ", Style::default().fg(palette.dim)))
            .padding(Padding::horizontal(1));
        let inner = block.inner(rect);
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);
        frame.render_widget(Paragraph::new(rows), inner);
    }
}

/// Append one message block to `lines`: a coloured bullet on the first line,
/// the text hanging-indented (2 cols) under it, preceded by a blank separator
/// line unless it's the first block. `first` is flipped to false after the call.
fn push_message(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    color: Color,
    bullet: &str,
    wrap_w: usize,
    first: &mut bool,
) {
    if !*first {
        lines.push(Line::from(""));
    }
    *first = false;
    for (i, seg) in wrap_words(content, wrap_w).into_iter().enumerate() {
        let prefix = if i == 0 { bullet } else { "  " };
        lines.push(Line::from(Span::styled(
            format!("{prefix}{seg}"),
            Style::default().fg(color),
        )));
    }
}

/// Word-wrap `text` to `width` columns (counted in `char`s — a good-enough
/// approximation for a terminal). Width is clamped to >= 1. Embedded newlines
/// force a break; words longer than `width` are hard-split.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for segment in text.split('\n') {
        let mut line = String::new();
        let mut line_len = 0usize;
        for word in segment.split_whitespace() {
            let wlen = word.chars().count();
            if wlen > width {
                if line_len > 0 {
                    out.push(std::mem::take(&mut line));
                }
                let mut chars: Vec<char> = word.chars().collect();
                while chars.len() > width {
                    let chunk: String = chars[..width].iter().collect();
                    out.push(chunk);
                    chars.drain(..width);
                }
                line = chars.into_iter().collect();
                line_len = line.chars().count();
            } else if line_len == 0 {
                line = word.to_string();
                line_len = wlen;
            } else if line_len + 1 + wlen <= width {
                line.push(' ');
                line.push_str(word);
                line_len += 1 + wlen;
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
                line_len = wlen;
            }
        }
        out.push(line);
    }
    out
}
