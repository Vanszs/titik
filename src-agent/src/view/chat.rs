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
    // While compacting, the input box shows the animation instead of the editor;
    // reserve 2 inner rows (spinner line + progress bar) regardless of input text.
    let input_rows = if rest.compact_anim_start.is_some() {
        2
    } else {
        input_rows.clamp(1, 8)
    };
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
                    //
                    // If the message contains wanderer lead-in lines (`Word: ...`),
                    // the entire block up to and including the LAST such line is
                    // rendered dim+italic (the "thinking" block); the remainder is
                    // rendered as markdown.
                    let (thinking_block, response_body) = split_thinking(&msg.content);
                    let thinking_style = Style::default()
                        .fg(palette.dim)
                        .add_modifier(Modifier::ITALIC);
                    let bar_style = Style::default().fg(palette.dim);
                    let mut logical: Vec<Vec<Span<'static>>> = Vec::new();
                    // Native reasoning channel (the model's streamed `reasoning`,
                    // captured separately from `content`). Rendered first, dim +
                    // italic, each line prefixed with the blockquote bar so the
                    // whole thinking block reads as quoted text. Display-only — it
                    // never re-enters the conversation or disk (`#[serde(skip)]`).
                    if let Some(reasoning) = msg.reasoning.as_deref() {
                        if !reasoning.is_empty() {
                            for line in reasoning.lines() {
                                push_thinking_line(
                                    &mut logical,
                                    line,
                                    thinking_style,
                                    bar_style,
                                    wrap_w,
                                );
                            }
                        }
                    }
                    if let Some(thinking) = thinking_block {
                        // Render each line of the thinking block dim+italic with the
                        // blockquote bar; wrapping + blank-line rows are handled by
                        // `push_thinking_line` so the bar survives every wrap.
                        for line in thinking.lines() {
                            push_thinking_line(
                                &mut logical,
                                line,
                                thinking_style,
                                bar_style,
                                wrap_w,
                            );
                        }
                    }
                    // Blank line between the (barred) thinking block and the answer
                    // so the quote→answer transition is clear. Only when there IS a
                    // thinking block AND an answer to separate.
                    if !logical.is_empty() && !response_body.is_empty() {
                        logical.push(vec![]);
                    }
                    if !response_body.is_empty() {
                        logical.extend(crate::view::markdown::render(response_body, palette, wrap_w));
                    }
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
        // Live partial turn: the in-progress reasoning (dim+italic, on top) and
        // content (fg). Reasoning typically streams first (the model thinks, then
        // answers), so the block shows whenever EITHER buffer has text — they
        // share one `●` bullet. Stream renders plain (not markdown) for perf +
        // partial-fence safety.
        let partial_content = rest.streaming.as_deref().unwrap_or("");
        let partial_reasoning = rest.stream_reasoning.as_str();
        if !partial_content.is_empty() || !partial_reasoning.is_empty() {
            if !first {
                lines.push(Line::from(""));
            }
            let thinking_style = Style::default()
                .fg(palette.dim)
                .add_modifier(Modifier::ITALIC);
            let bar_style = Style::default().fg(palette.dim);
            let mut logical: Vec<Vec<Span<'static>>> = Vec::new();
            // Partial reasoning first, dim+italic, each line prefixed with the
            // blockquote bar (mirrors the committed-message reasoning render).
            // These are emitted pre-wrapped, so render_block passes them through.
            if !partial_reasoning.is_empty() {
                for line in partial_reasoning.lines() {
                    push_thinking_line(&mut logical, line, thinking_style, bar_style, wrap_w);
                }
            }
            // Blank line between the barred thinking block and the answer so the
            // transition is clear, when both are present.
            if !logical.is_empty() && !partial_content.is_empty() {
                logical.push(vec![]);
            }
            // Then the partial answer in the theme fg (one logical line; wraps).
            if !partial_content.is_empty() {
                logical.push(vec![Span::styled(
                    partial_content.to_string(),
                    Style::default().fg(palette.fg),
                )]);
            }
            lines.extend(render_block(logical, "● ", palette.fg, wrap_w, true));
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

    // --- Input box / compaction animation --- dim top + bottom borders. While a
    // `/compact` is in flight we replace the input contents with an animated
    // indicator (spinner + elapsed + indeterminate sweep) so the wait is legible;
    // otherwise the normal `[$] {input}` editor is drawn.
    let input_block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim))
        .padding(Padding::horizontal(2));
    let input_inner = input_block.inner(chunks[2]);
    frame.render_widget(input_block, chunks[2]);
    if let Some(start) = rest.compact_anim_start {
        render_compact_anim(frame, input_inner, start, palette);
    } else {
        // Multiline editor: logical lines split on '\n'; the first carries the
        // accent prompt, every continuation a 4-col indent so it hangs under the
        // prompt. A non-blinking block caret is painted AT `rest.cursor` (a char
        // index into the whole input, counting the '\n's), so mid-text edits show
        // the caret in place rather than always at the end. The box height
        // (`input_h`, computed above) grows with the wrapped content up to the cap.
        //
        // Map the flat char-index caret to (logical line, column): walk the lines
        // accumulating their char counts plus 1 per consumed '\n'. The caret sits
        // on the line where `consumed <= cursor <= consumed + line_chars` (the
        // upper bound is the line's end, just before its '\n').
        let mut input_lines: Vec<Line> = Vec::new();
        let cursor = rest.cursor;
        let mut consumed = 0usize; // chars before the current logical line
        let logicals: Vec<&str> = rest.input.split('\n').collect();
        let last_idx = logicals.len().saturating_sub(1);
        for (i, logical) in logicals.iter().enumerate() {
            let line_chars = logical.chars().count();
            // The caret falls on this line when its flat index lands within the
            // line's char span. Use `<=` on the end so an end-of-line caret shows
            // here; for non-final lines the '\n' position belongs to the NEXT line
            // (handled by the `< end` guard) so it isn't drawn twice.
            let on_this_line = if i == last_idx {
                cursor >= consumed && cursor <= consumed + line_chars
            } else {
                cursor >= consumed && cursor < consumed + line_chars + 1
            };
            // Prompt prefix: accent "[$] " on the first line, 4-col hang otherwise.
            let prefix: Span = if i == 0 {
                Span::styled("[$] ", Style::default().fg(palette.accent))
            } else {
                Span::raw("    ")
            };
            let mut spans: Vec<Span> = vec![prefix];
            if on_this_line {
                let col = (cursor - consumed).min(line_chars);
                let before: String = logical.chars().take(col).collect();
                let at: String = logical.chars().nth(col).map(String::from).unwrap_or_default();
                let after: String = logical.chars().skip(col + 1).collect();
                if !before.is_empty() {
                    spans.push(Span::raw(before));
                }
                // The caret cell: reverse-video over the char under it, or a solid
                // block when the caret is at end-of-line (no char to invert).
                if at.is_empty() {
                    spans.push(Span::styled("█", Style::default().fg(palette.accent)));
                } else {
                    spans.push(Span::styled(
                        at,
                        Style::default().add_modifier(Modifier::REVERSED),
                    ));
                }
                if !after.is_empty() {
                    spans.push(Span::raw(after));
                }
            } else {
                spans.push(Span::raw(*logical));
            }
            input_lines.push(Line::from(spans));
            // Advance past this line's chars plus the '\n' that split consumed.
            consumed += line_chars + 1;
        }
        frame.render_widget(
            Paragraph::new(input_lines).wrap(Wrap { trim: false }),
            input_inner,
        );
    }

    // --- Status bar --- padded to align with the rest. Status text on the
    // left (dim); the cumulative token/cost readout right-aligned (accent).
    let status_area = chunks[3].inner(Margin { horizontal: 2, vertical: 0 });
    let readout = if rest.tokens_in > 0 || rest.tokens_out > 0 || rest.cost > 0.0 {
        // Show the cached-prompt-token count right after the input arrow when the
        // last response hit the prompt cache (`cached:N`), so the saving is
        // visible; omitted entirely on a cold prefix to keep the readout quiet.
        let cached = if rest.tokens_cached > 0 {
            format!(" cached:{}", fmt_count(rest.tokens_cached))
        } else {
            String::new()
        };
        Some(format!(
            "↑{}{} ↓{}  ${:.4}",
            fmt_count(rest.tokens_in),
            cached,
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

    // --- File reference palette --- `@`-triggered global substring file search,
    // same box style as the command palette, anchored above the input. Only
    // shown when the command palette is NOT active and the current token is
    // `@partial`. Uses `search` so deep files appear on every keystroke.
    let file_palette_active = cmd_matches.is_empty();
    if file_palette_active {
        if let Some(partial) = crate::controller::input::file_ref_partial(&rest.input) {
            const MAX_VIS: usize = 10;
            let files: Vec<String> = rest.dir_cache.read().map(|c| c.search(partial, MAX_VIS)).unwrap_or_default();
            if !files.is_empty() {
                let sel = rest.palette_sel.min(files.len() - 1);
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

    // --- Toast --- transient box pinned to the top of the transcript. `Error`
    // is a red box ("error"); `Info` is a neutral accent box ("info") used for
    // notices like the post-compaction summary. Both wrap to width and span
    // multiple lines; the info box is capped taller so a short summary fits.
    if let Some((msg, _, kind)) = rest.toast.as_ref() {
        let (border_color, title, max_rows) = match kind {
            crate::app::state::ToastKind::Error => (Color::Rgb(255, 90, 90), " error ", 6u16),
            crate::app::state::ToastKind::Info => (palette.accent, " info ", 10u16),
        };
        let tw = chunks[1].width;
        let inner_w = (tw as usize).saturating_sub(4).max(1);
        // Wrap each logical line (the summary toast embeds a leading newline so the
        // "compacted ✓" header sits on its own row above the body).
        let rows: Vec<Line> = msg
            .split('\n')
            .flat_map(|logical| {
                crate::view::markdown::wrap_spans(
                    &[Span::styled(logical.to_string(), Style::default().fg(palette.fg))],
                    inner_w,
                )
            })
            .map(Line::from)
            .collect();
        // Cap the box height: never exceed the transcript, and clamp Info/Error to
        // their own row budget so a long message stays contained.
        let body_rows = (rows.len() as u16).min(max_rows);
        let h = (body_rows + 2).min(chunks[1].height.max(3));
        let rect = Rect { x: chunks[1].x, y: chunks[1].y, width: tw, height: h };
        let block = Block::bordered()
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(title, Style::default().fg(border_color)))
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
            // When the harness (TAC) forced this approval, show its reason so the
            // user knows WHY the call was flagged rather than auto-run.
            if let Some(reason) = rest.approval_reason.as_ref() {
                if !reason.is_empty() {
                    rows.push(Line::from(Span::styled(
                        format!(" harness: {reason}"),
                        Style::default().fg(palette.dim),
                    )));
                }
            }
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

/// Render the `/compact` waiting animation into `area` (the input box interior):
/// a cycling braille spinner + "Compacting conversation… ({elapsed}s)" on the
/// first row, and an indeterminate progress bar (a block sweeping across a hatch
/// track) on the second row when there's height for it. Driven purely by
/// `start.elapsed()` so it advances every redraw without any stored counter.
fn render_compact_anim(frame: &mut Frame, area: Rect, start: std::time::Instant, palette: &Palette) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let elapsed = start.elapsed();
    let secs = elapsed.as_secs();
    // ~12.5 fps spinner cadence (80ms/frame) — smooth but not frantic.
    let frame_idx = (elapsed.as_millis() / 80) as usize;
    let glyph = SPINNER[frame_idx % SPINNER.len()];

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(palette.accent)),
        Span::styled(
            format!("Compacting conversation… ({secs}s)"),
            Style::default().fg(palette.dim),
        ),
    ]));

    // Indeterminate bar: a short solid block bounces across a hatched track. The
    // position ping-pongs over the free span so it never just wraps/jumps.
    if area.height >= 2 {
        let track = (area.width as usize).max(1);
        let block_w = 6usize.min(track);
        let span = track.saturating_sub(block_w);
        let pos = if span == 0 {
            0
        } else {
            // Advance one cell per ~60ms, ping-ponging over [0, span].
            let step = (elapsed.as_millis() / 60) as usize % (span * 2);
            if step <= span { step } else { span * 2 - step }
        };
        let mut spans: Vec<Span> = Vec::with_capacity(3);
        if pos > 0 {
            spans.push(Span::styled("░".repeat(pos), Style::default().fg(palette.dim)));
        }
        spans.push(Span::styled(
            "▓".repeat(block_w),
            Style::default().fg(palette.accent),
        ));
        let trailing = track - pos - block_w;
        if trailing > 0 {
            spans.push(Span::styled(
                "░".repeat(trailing),
                Style::default().fg(palette.dim),
            ));
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), area);
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

/// Split an assistant message into (thinking, response).
///
/// `thinking` = the prefix up to and INCLUDING the last line that starts with a
/// wanderer-word lead-in (`Word:` where `Word` is in the wanderer corpus,
/// case-insensitive). `response` = the remainder (leading blank lines trimmed).
/// Returns `(None, full)` when no wanderer-led line exists (normal message).
///
/// Only lines whose FIRST colon-delimited token is a wanderer word count; a
/// wanderer word appearing mid-sentence (no leading `Word:` pattern) is ignored.
fn split_thinking(content: &str) -> (Option<&str>, &str) {
    let corpus = crate::resources::wanderer_words();
    // Walk lines recording byte offsets. For each line, check whether the token
    // before the first ':' (trimmed, lowercased) is in the wanderer corpus.
    // Track the byte offset just past the last matching line's trailing '\n' (or
    // end of string when the matching line is the final line).
    let mut last_end: Option<usize> = None;
    let mut offset: usize = 0;
    for line in content.split('\n') {
        // `line` does not include the '\n'; `line_end` is the byte offset of the
        // character after the '\n' (or end of string for the final segment).
        let line_end = offset + line.len();
        // Check whether this line has a wanderer lead-in.
        let trimmed = line.trim();
        if let Some(colon_pos) = trimmed.find(':') {
            let token = trimmed[..colon_pos].trim().to_lowercase();
            if corpus.iter().any(|w| w == &token) {
                // Include the '\n' if present; clamp to content length.
                last_end = Some((line_end + 1).min(content.len()));
            }
        }
        // Advance past the '\n' separator (the split consumes it but we account
        // for it in the offset so our byte positions stay aligned with `content`).
        offset = line_end + 1;
    }
    match last_end {
        Some(e) => {
            let thinking = &content[..e];
            // Trim only leading newlines from the response so internal structure
            // of the response body is preserved.
            let response = content[e..].trim_start_matches('\n');
            (Some(thinking), response)
        }
        None => (None, content),
    }
}

/// The blockquote bar drawn to the LEFT of every thinking/reasoning line, so the
/// gray-italic "thinking" reads as quoted text distinct from the answer. A single
/// dim vertical bar (U+258F) + a space — honours the minimalist top-down border
/// style (one bar, never a box). The answer + tool lines get no bar.
const THINK_BAR: &str = "▏ ";

/// Render one logical line of THINKING text into barred visual lines.
///
/// Wraps `text` to `wrap_w` MINUS the bar width, then prefixes each wrapped
/// visual line with a dim `THINK_BAR`, so the quote bar survives wrapping (it
/// appears on every wrapped line, not just the first). `text` styling (dim +
/// italic) is applied to the content spans; the bar is dim. A blank input line
/// yields a single bar-only row so paragraph breaks inside the block keep the
/// quote rail unbroken. The result is pushed as logical lines into `out` (the
/// caller's accumulator), where they are later passed through `render_block`
/// unwrapped (already exact-width).
fn push_thinking_line(
    out: &mut Vec<Vec<Span<'static>>>,
    text: &str,
    style: Style,
    bar_style: Style,
    wrap_w: usize,
) {
    let bar = Span::styled(THINK_BAR, bar_style);
    // The bar eats 2 columns; wrap the text to the remainder so bar+text stays
    // within wrap_w. Floor at 1 so a pathologically narrow pane can't wrap to 0.
    let inner_w = wrap_w.saturating_sub(THINK_BAR.chars().count()).max(1);
    if text.trim().is_empty() {
        // Blank line inside the thinking block: keep the rail with a bar-only row.
        out.push(vec![bar]);
        return;
    }
    let spans = vec![Span::styled(text.to_string(), style)];
    for visual in crate::view::markdown::wrap_spans(&spans, inner_w) {
        let mut line = Vec::with_capacity(visual.len() + 1);
        line.push(bar.clone());
        line.extend(visual);
        out.push(line);
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
