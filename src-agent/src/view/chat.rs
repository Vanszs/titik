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
//! Assistant replies render as **Markdown** (headings, lists, bold, inline
//! `code`, boxed and syntax-highlighted fenced code blocks, and aligned GFM
//! tables) via the in-tree [`crate::view::markdown`] renderer, built on
//! `pulldown-cmark` and `syntect`. It returns FINAL visual lines (already
//! wrapped/boxed/aligned) while the bullet and hanging indent use the palette.
//! User messages and the live streaming buffer stay plain (the buffer renders
//! plain for perf and partial-fence safety). Everything is pre-wrapped — the
//! markdown included — so the emitted line count equals the exact visual line
//! count, which the follow-scroll math depends on.
//!
//! - User messages   → `★` in `palette.accent`, plain text
//! - AI messages     → `●` in `palette.fg`, markdown body
//! - Streaming buffer → `●` in `palette.fg`, plain text
//! - System / status → `palette.dim`
//!
//! When the user types `/` followed by a command name with no whitespace, a
//! bordered popup palette floats above the input row listing the matching
//! commands filtered in real time. Up/Down navigate the list; Tab completes.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
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
    // --- Input height ---
    // The input box grows to fit its wrapped content (capped). Compute the row
    // count BEFORE the layout split so the layout can reserve the right height.
    // Inner content width = frame width minus 2 borders and 4 cols of horizontal
    // padding (2 left + 2 right). Logical lines split on '\n'; the first is
    // visually prefixed by the prompt "[$] " (4 cols), continuations by 4 spaces
    // so they hang under the prompt. Each prefixed line wraps to inner_w.
    let inner_w = (frame.area().width.saturating_sub(2 + 4) as usize).max(1);
    let mut input_rows = 0usize;
    for line in rest.input.split('\n') {
        // 4 cols for the prompt on the first line, 4 for the hanging indent on
        // continuations — both happen to be the same width here.
        let prefixed = line.chars().count() + 4;
        input_rows += 1usize.max(prefixed.div_ceil(inner_w));
    }
    let input_rows = input_rows.clamp(1, 8);
    let input_h = (input_rows as u16) + 2; // + top & bottom borders

    // Layout: header (text + bottom rule) | transcript | input (top+bottom
    // rules) | status. Header/input get thin dim borders so the screen reads
    // as structured, not boxed; the transcript itself stays flat.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),       // header line + bottom border
            Constraint::Min(1),          // transcript
            Constraint::Length(input_h), // top border + input row(s) + bottom border
            Constraint::Length(1),       // status bar
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
        Span::styled(" · ", Style::default().fg(palette.dim)),
        Span::styled(rest.agent_mode.label(), Style::default().fg(palette.accent)),
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

    // Render (or reuse) each committed message's lines. Cache is keyed by width
    // + palette; only NEW messages are rendered, so syntect doesn't re-run every
    // frame. A shrink (compaction / resend) or key change forces a full rebuild.
    {
        let mut cache = rest.transcript_cache.borrow_mut();
        if cache.width != wrap_w || cache.palette != Some(*palette) {
            cache.width = wrap_w;
            cache.palette = Some(*palette);
            cache.blocks.clear();
        }
        let committed: Vec<&crate::dto::chat::ChatMessage> = rest
            .session
            .as_ref()
            .map(|s| {
                s.conversation
                    .messages()
                    .iter()
                    .filter(|m| m.role != Role::System)
                    .collect()
            })
            .unwrap_or_default();
        if cache.blocks.len() > committed.len() {
            cache.blocks.clear(); // shrank → stale prefix can't be trusted
        }
        let start = cache.blocks.len();
        for msg in committed.iter().skip(start) {
            let block = match msg.role {
                Role::User => render_block(
                    vec![vec![Span::styled(
                        msg.content.to_string(),
                        Style::default().fg(palette.accent),
                    )]],
                    "★ ",
                    palette.accent,
                    wrap_w,
                    true,
                ),
                Role::Assistant => {
                    // Markdown body, then one dim line per requested tool call so
                    // the user can see what the agent invoked. Appended as logical
                    // lines so they get the hanging 2-col indent under the bullet.
                    let mut logical = crate::view::markdown::render(&msg.content, palette, wrap_w);
                    if let Some(calls) = msg.tool_calls.as_ref() {
                        for call in calls {
                            let args = truncate_chars(&call.function.arguments, 60);
                            logical.push(vec![Span::styled(
                                format!("  ⚙ {}({})", call.function.name, args),
                                Style::default().fg(palette.dim),
                            )]);
                        }
                    }
                    render_block(logical, "● ", palette.fg, wrap_w, false)
                }
                Role::Tool => {
                    // Harness-internal tool results (the silent "plan first"
                    // nudge) carry a hide-marker: fed to the model, never shown.
                    // Cache an EMPTY block (not `continue`) so the cache stays
                    // index-aligned with `committed`; empty blocks are skipped
                    // entirely during frame assembly (no block, no separator).
                    if msg.content.starts_with(crate::dto::chat::PLAN_NUDGE_MARK) {
                        cache.blocks.push(Vec::new());
                        continue;
                    }
                    // Compact dim block: just the first line of the tool output,
                    // truncated. Tool results are not markdown-rendered.
                    let first = msg.content.lines().next().unwrap_or("");
                    let first = truncate_chars(first, 80);
                    render_block(
                        vec![vec![Span::styled(first, Style::default().fg(palette.dim))]],
                        "  ↳ ",
                        palette.dim,
                        wrap_w,
                        false,
                    )
                }
                Role::System => continue,
            };
            cache.blocks.push(block);
        }

        // Assemble the frame: cached blocks (with blank separators) + the live
        // streaming line (rendered fresh — it changes every token).
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut first = true;
        for block in &cache.blocks {
            // Empty blocks (hidden harness messages) leave no visual trace: skip
            // both the block AND its blank separator so the transcript is clean.
            if block.is_empty() {
                continue;
            }
            if !first {
                lines.push(Line::from(""));
            }
            first = false;
            lines.extend(block.iter().cloned());
        }
        if let Some(buf) = rest.streaming.as_ref() {
            if !buf.is_empty() {
                // Stream renders plain (not markdown) for perf + partial-fence safety.
                if !first {
                    lines.push(Line::from(""));
                }
                lines.extend(render_block(
                    vec![vec![Span::styled(
                        buf.to_string(),
                        Style::default().fg(palette.fg),
                    )]],
                    "● ",
                    palette.fg,
                    wrap_w,
                    true,
                ));
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
    } // cache borrow ends

    // --- Input box --- `[$] {input}`, dim top + bottom borders. Multiline:
    // logical lines split on '\n'; the first carries the accent prompt, every
    // continuation a 4-col indent so it hangs under the prompt. A non-blinking
    // block cursor is painted at the very end of the last line. The box height
    // (`input_h`, computed above) grows with the wrapped content up to the cap.
    let mut input_lines: Vec<Line> = Vec::new();
    for (i, logical) in rest.input.split('\n').enumerate() {
        if i == 0 {
            input_lines.push(Line::from(vec![
                Span::styled("[$] ", Style::default().fg(palette.accent)),
                Span::raw(logical),
            ]));
        } else {
            input_lines.push(Line::from(vec![Span::raw("    "), Span::raw(logical)]));
        }
    }
    // Append a non-blinking block cursor to the last line so the caret is visible.
    if let Some(last) = input_lines.last_mut() {
        last.spans
            .push(Span::styled("█", Style::default().fg(palette.accent)));
    }
    let input_block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim))
        .padding(Padding::horizontal(2));
    let input_inner = input_block.inner(chunks[2]);
    frame.render_widget(input_block, chunks[2]);
    frame.render_widget(
        Paragraph::new(input_lines).wrap(Wrap { trim: false }),
        input_inner,
    );

    // --- Status bar --- padded to align with the rest. Status text on the
    // left (dim); the cumulative token/cost readout right-aligned (accent).
    let status_area = chunks[3].inner(Margin { horizontal: 2, vertical: 0 });
    let readout = if rest.tokens_in > 0 || rest.tokens_out > 0 || rest.cost > 0.0 {
        Some(format!(
            "↑{} ↓{}  ${:.4}",
            fmt_count(rest.tokens_in),
            fmt_count(rest.tokens_out),
            rest.cost
        ))
    } else {
        None
    };
    match &readout {
        Some(r) => {
            // `↑ ↓ $` and digits are each one display column, so a char count is
            // the exact width; +1 keeps a gap from the status text.
            let w = u16::try_from(r.chars().count() + 1).unwrap_or(u16::MAX);
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(w)])
                .split(status_area);
            frame.render_widget(
                Paragraph::new(rest.status.as_str()).style(Style::default().fg(palette.dim)),
                cols[0],
            );
            frame.render_widget(
                Paragraph::new(r.as_str())
                    .style(Style::default().fg(palette.accent))
                    .alignment(Alignment::Right),
                cols[1],
            );
        }
        None => {
            frame.render_widget(
                Paragraph::new(rest.status.as_str()).style(Style::default().fg(palette.dim)),
                status_area,
            );
        }
    }

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

    // --- File reference palette --- `@`-triggered depth-1 file browser, same
    // box style as the command palette, anchored above the input. Only shown
    // when the command palette is NOT active and the current token is `@partial`.
    let file_palette_active = cmd_matches.is_empty();
    if file_palette_active {
        if let Some(partial) = crate::controller::input::file_ref_partial(&rest.input) {
            let files: Vec<String> = rest.dir_cache.read().map(|c| c.list_at(partial)).unwrap_or_default();
            if !files.is_empty() {
                let sel = rest.palette_sel.min(files.len() - 1);
                const MAX_VIS: usize = 10;
                // window start keeps `sel` visible (anchors to bottom when scrolling down)
                let start = if sel < MAX_VIS { 0 } else { sel + 1 - MAX_VIS };
                let end = (start + MAX_VIS).min(files.len());
                let rows: Vec<Line> = files[start..end].iter().enumerate().map(|(vi, f)| {
                    let i = start + vi;
                    if i == sel {
                        let hl = Style::default().fg(palette.sel_fg).bg(palette.sel_bg);
                        Line::from(Span::styled(format!(" {f} "), hl))
                    } else {
                        Line::from(Span::styled(format!(" {f} "), Style::default().fg(palette.fg)))
                    }
                }).collect();
                // title shows position when there are more entries than fit
                let title = if files.len() > MAX_VIS {
                    format!(" files {}/{} ", sel + 1, files.len())
                } else {
                    " files ".to_string()
                };
                let avail = chunks[2].y.saturating_sub(chunks[1].y);
                let h = ((rows.len() as u16) + 2).min(avail.max(3));
                let y = chunks[2].y.saturating_sub(h);
                let popup = Rect { x: chunks[2].x, y, width: chunks[2].width, height: h };
                let block = Block::bordered()
                    .border_style(Style::default().fg(palette.dim))
                    .title(Span::styled(title, Style::default().fg(palette.dim)))
                    .padding(Padding::horizontal(1));
                let inner = block.inner(popup);
                frame.render_widget(Clear, popup);
                frame.render_widget(block, popup);
                frame.render_widget(Paragraph::new(rows), inner);
            }
        }
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

    // --- Error toast --- transient red box pinned to the top of the transcript.
    if let Some((msg, _)) = rest.toast.as_ref() {
        let err_color = Color::Rgb(255, 90, 90);
        let tw = chunks[1].width;
        let inner_w = (tw as usize).saturating_sub(4).max(1);
        let rows: Vec<Line> = crate::view::markdown::wrap_spans(
            &[Span::styled(msg.clone(), Style::default().fg(palette.fg))],
            inner_w,
        )
        .into_iter()
        .map(Line::from)
        .collect();
        let h = ((rows.len() as u16) + 2).min(chunks[1].height.max(3));
        let rect = Rect { x: chunks[1].x, y: chunks[1].y, width: tw, height: h };
        let block = Block::bordered()
            .border_style(Style::default().fg(err_color))
            .title(Span::styled(" error ", Style::default().fg(err_color)))
            .padding(Padding::horizontal(1));
        let inner = block.inner(rect);
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);
        frame.render_widget(Paragraph::new(rows), inner);
    }

    // --- Tool-approval prompt --- shown while a risky tool call is paused for
    // the user's y/n (Normal mode). A small warning-coloured box anchored just
    // above the input, listing the pending call. Modal: keys are routed to the
    // approve/deny handlers in the input controller, not into the transcript.
    if rest.awaiting_approval {
        let warn = Color::Rgb(255, 180, 60);
        let mut rows: Vec<Line> = Vec::new();
        if let Some(call) = rest.pending_tool_calls.get(rest.tool_idx) {
            let name = call.function.name.as_str();
            // header: which tool
            rows.push(Line::from(Span::styled(
                format!(" ⚙ {name}"),
                Style::default().fg(warn).add_modifier(Modifier::BOLD),
            )));
            let v: serde_json::Value =
                serde_json::from_str(&call.function.arguments).unwrap_or_default();
            let inner_w = (chunks[2].width as usize)
                .saturating_sub(2 /*borders*/ + 2 /*padding*/ + 3 /*indent*/)
                .max(8);
            match name {
                "write" => {
                    let path = v["path"].as_str().unwrap_or("?");
                    let content = v["content"].as_str().unwrap_or("");
                    let n_lines = content.lines().count().max(1);
                    rows.push(Line::from(vec![
                        Span::styled(" target:  ", Style::default().fg(palette.dim)),
                        Span::styled(path.to_string(), Style::default().fg(palette.fg)),
                    ]));
                    rows.push(Line::from(vec![
                        Span::styled(" payload: ", Style::default().fg(palette.dim)),
                        Span::styled(
                            format!("{n_lines} lines, {} bytes", content.len()),
                            Style::default().fg(palette.fg),
                        ),
                    ]));
                    // preview up to 8 content lines, each truncated to the box width
                    for line in content.lines().take(8) {
                        rows.push(Line::from(Span::styled(
                            format!("   {}", truncate_chars(line, inner_w)),
                            Style::default().fg(palette.dim),
                        )));
                    }
                    if n_lines > 8 {
                        rows.push(Line::from(Span::styled(
                            format!("   … (+{} more lines)", n_lines - 8),
                            Style::default().fg(palette.dim),
                        )));
                    }
                }
                "delete" => {
                    let path = v["path"].as_str().unwrap_or("?");
                    rows.push(Line::from(vec![
                        Span::styled(" target:  ", Style::default().fg(palette.dim)),
                        Span::styled(path.to_string(), Style::default().fg(palette.fg)),
                    ]));
                }
                "edit" => {
                    let path = v["path"].as_str().unwrap_or("?");
                    let old = v["old"].as_str().unwrap_or("");
                    let new = v["new"].as_str().unwrap_or("");
                    rows.push(Line::from(vec![
                        Span::styled(" target:  ", Style::default().fg(palette.dim)),
                        Span::styled(path.to_string(), Style::default().fg(palette.fg)),
                    ]));
                    rows.push(Line::from(vec![
                        Span::styled(" payload: ", Style::default().fg(palette.dim)),
                        Span::styled(
                            format!(
                                "{} → {}",
                                truncate_chars(old, inner_w / 2),
                                truncate_chars(new, inner_w / 2)
                            ),
                            Style::default().fg(palette.fg),
                        ),
                    ]));
                }
                "bash" => {
                    let cmd = v["command"].as_str().unwrap_or("?");
                    rows.push(Line::from(vec![
                        Span::styled(" command: ", Style::default().fg(palette.dim)),
                        Span::styled(
                            truncate_chars(cmd, inner_w),
                            Style::default().fg(palette.fg),
                        ),
                    ]));
                    // Show additional lines of multi-line commands.
                    let lines: Vec<&str> = cmd.lines().collect();
                    for line in lines.iter().skip(1).take(6) {
                        rows.push(Line::from(Span::styled(
                            format!("   {}", truncate_chars(line, inner_w)),
                            Style::default().fg(palette.dim),
                        )));
                    }
                    if lines.len() > 7 {
                        rows.push(Line::from(Span::styled(
                            format!("   … (+{} more lines)", lines.len() - 7),
                            Style::default().fg(palette.dim),
                        )));
                    }
                }
                _ => {
                    rows.push(Line::from(Span::styled(
                        format!(" {}", truncate_chars(&call.function.arguments, 120)),
                        Style::default().fg(palette.dim),
                    )));
                }
            }
        }
        rows.push(Line::from(vec![
            Span::styled(" [y] run   ", Style::default().fg(warn)),
            Span::styled("[n] deny", Style::default().fg(palette.dim)),
        ]));

        // Anchor just above the input, full input width, growing upward —
        // identical placement to the slash-command / help boxes.
        let avail = chunks[2].y.saturating_sub(chunks[1].y);
        let h = ((rows.len() as u16) + 2).min(avail.max(3));
        let y = chunks[2].y.saturating_sub(h);
        let rect = Rect { x: chunks[2].x, y, width: chunks[2].width, height: h };

        let block = Block::bordered()
            .border_style(Style::default().fg(warn))
            .title(Span::styled(" approve ", Style::default().fg(warn)))
            .padding(Padding::horizontal(1));
        let inner = block.inner(rect);
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);
        frame.render_widget(Paragraph::new(rows), inner);
    }
}

/// Truncate `s` to at most `max` characters (not bytes), appending `…` when it
/// was cut. Used to keep tool-call / tool-result preview lines on one row.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Compact token count: raw below 10k, else "10,1k" / "1,1m" (one decimal,
/// comma as the decimal mark, k=thousand m=million).
fn fmt_count(n: u64) -> String {
    if n < 10_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0).replace('.', ",")
    } else {
        format!("{:.1}m", n as f64 / 1_000_000.0).replace('.', ",")
    }
}

/// One message's visual lines: bullet on the first line, 2-col indent on the
/// rest. `wrap` = wrap each logical line with `markdown::wrap_spans` (plain text
/// / user / streaming); pre-wrapped markdown passes its lines through unwrapped.
///
/// Returns the block in isolation — NO blank separator and NO `first` handling;
/// the caller stitches blocks together with blank lines. `bullet_color` styles
/// only the bullet; the wrapped/pre-wrapped spans keep their own styles. The
/// emitted line count equals the exact on-screen line count the follow-scroll
/// math relies on.
fn render_block(
    logical: Vec<Vec<Span<'static>>>,
    bullet: &str,
    bullet_color: Color,
    wrap_w: usize,
    wrap: bool,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut first_visual = true;
    for logical_line in logical {
        let visuals: Vec<Vec<Span<'static>>> = if wrap {
            crate::view::markdown::wrap_spans(&logical_line, wrap_w)
        } else {
            vec![logical_line]
        };
        for visual in visuals {
            // First visual line of the whole block gets the bullet; the rest get
            // a 2-col indent so wrapped/continuation/boxed content hangs under it.
            let prefix = if first_visual {
                Span::styled(bullet.to_string(), Style::default().fg(bullet_color))
            } else {
                Span::raw("  ")
            };
            first_visual = false;
            let mut spans = Vec::with_capacity(visual.len() + 1);
            spans.push(prefix);
            spans.extend(visual);
            out.push(Line::from(spans));
        }
    }
    out
}
