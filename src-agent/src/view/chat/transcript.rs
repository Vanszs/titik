//! Transcript area: committed messages, live streaming buffer, sub-agent
//! inline indicator, and the follow-scroll logic.

use ratatui::{
    layout::{Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use crate::app::state::AppStateRest;
use crate::dto::chat::Role;
use crate::view::theme::Palette;
use super::helpers::{
    push_thinking_line, render_block, split_thinking, truncate_chars,
};
use serde_json::Value;

/// Render the transcript area into `body_chunk`.
///
/// Padded, flat. Each message is a block: a coloured bullet (★ user / ● ai)
/// on the first line, text hanging-indented under it, blank line between
/// blocks. Pre-wrapped by hand for the hanging indent.
pub(super) fn render_transcript(
    frame: &mut Frame,
    body_chunk: Rect,
    rest: &AppStateRest,
    palette: &Palette,
) {
    let body = body_chunk.inner(Margin { horizontal: 2, vertical: 0 });
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
            .fg()
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

        // Collapse consecutive bash_output polls for the same job into a single
        // visual entry. This is a view-layer transformation; the underlying
        // conversation history stays unchanged for the model.
        let visual = build_visual_entries(&committed);

        // Which tool calls have COMPLETED: a `tool`-role result message exists
        // whose `tool_call_id` points back at the call. Built fresh every frame
        // from the live conversation so the gear→check flip is NOT baked into the
        // (one-shot) cached Assistant block — the result message is committed a
        // round LATER than the assistant call, so the cached block can't carry the
        // final glyph. The tool-call lines are therefore rendered fresh at frame
        // assembly (below), consulting this set, while the heavy markdown body
        // stays cached. `&str` borrows from `committed`, valid for this frame.
        let completed_tool_ids: std::collections::HashSet<&str> = committed
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        if cache.blocks.len() > visual.len() {
            cache.blocks.clear(); // shrank → stale prefix can't be trusted
        }
        // A bash_output group at the tail can still receive new poll results, so
        // its cached block is volatile. Force a rebuild of the last block when it
        // is a group; the rest of the cache stays stable.
        if matches!(visual.last(), Some(VisualEntry::BashOutputGroup { .. })) {
            cache.blocks.pop();
        }
        let start = cache.blocks.len();
        for entry in visual.iter().skip(start) {
            // One block per visual entry, index-aligned with `visual`. A hidden
            // harness tool result yields an EMPTY block (skipped at assembly), so
            // the cache never falls out of step with the message list.
            let block = match entry {
                VisualEntry::Single(msg) => render_message_block(msg, palette, wrap_w),
                VisualEntry::BashOutputGroup { .. } => {
                    render_bash_output_group(entry, palette, wrap_w)
                }
            };
            cache.blocks.push(block);
        }

        // Assemble the frame: cached blocks (with blank separators) + the live
        // streaming line (rendered fresh — it changes every token). `cache.blocks`
        // is index-aligned with `committed` (one block per non-system message), so
        // we zip them: the block carries the cached body, and for an Assistant turn
        // the tool-call lines are appended fresh here (glued to the same block, no
        // separator) with a live ⚙/✓ glyph from `completed_tool_ids`.
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut first = true;
        for (i, block) in cache.blocks.iter().enumerate() {
            // The fresh tool-call lines for this block, if it's an assistant turn
            // that requested calls. A finished call (its id is in the completed set)
            // gets an accent `✓ `; an in-flight one keeps the dim `⚙ `. Normally
            // indented 2 cols so they hang under the `●` bullet, BUT when the
            // assistant body is empty (a pure tool-call turn → empty cached block)
            // the FIRST tool line takes the `● ` bullet so the block isn't a
            // bullet-less orphan.
            //
            // Bash_output groups already render their own compact block, so they
            // get no extra tool lines here.
            let has_body = !block.is_empty();
            let tool_lines: Vec<Line<'static>> = visual
                .get(i)
                .and_then(|entry| match entry {
                    VisualEntry::Single(msg) => Some(*msg),
                    VisualEntry::BashOutputGroup { .. } => None,
                })
                .map(|m| render_tool_lines(m, &completed_tool_ids, has_body, palette))
                .unwrap_or_default();

            // Empty blocks (hidden harness messages) with no tool lines leave no
            // visual trace: skip both the block AND its blank separator so the
            // transcript is clean. (A hidden message never carries tool calls.)
            if block.is_empty() && tool_lines.is_empty() {
                continue;
            }
            if !first {
                lines.push(Line::from(""));
            }
            first = false;
            lines.extend(block.iter().cloned());
            lines.extend(tool_lines);
        }
        // Live partial turn: the in-progress reasoning (dim+italic, on top) and
        // content (fg). Reasoning typically streams first (the model thinks, then
        // answers), so the block shows whenever EITHER buffer has text — they
        // share one `●` bullet. Stream renders plain (not markdown) for perf +
        // partial-fence safety.
        let partial_content = rest.fg().streaming.as_deref().unwrap_or("");
        let partial_reasoning = rest.fg().stream_reasoning.as_str();
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
            // Strip residual tool-call markup so tags don't flash mid-stream; the
            // "unmatched open → cut to end" rule in the stripper naturally hides a
            // call that is still being emitted. Render nothing if the result is empty.
            if !partial_content.is_empty() {
                let stripped = crate::dto::chat::strip_tool_call_tags(partial_content);
                if !stripped.is_empty() {
                    logical.push(vec![Span::styled(
                        stripped,
                        Style::default().fg(palette.fg),
                    )]);
                }
            }
            lines.extend(render_block(logical, "● ", palette.fg, wrap_w, true));
        }

        // Sub-agent inline indicator: one animated line per RUNNING sub-agent,
        // appended at the bottom of the transcript so it sits just above the input
        // box and has full width. Uses the same time-driven braille spinner as the
        // compact animation (80ms/frame cadence). Only rendered while at least one
        // sub-agent is Running; disappears automatically when all finish.
        const SA_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let running_agents: Vec<&crate::app::subagent::SubAgent> = rest
            .fg()
            .subagents
            .iter()
            .filter(|s| matches!(s.status, crate::app::subagent::SubAgentStatus::Running))
            .collect();
        if !running_agents.is_empty() {
            let elapsed_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let frame_idx = (elapsed_ms / 80) as usize;
            let glyph = SA_SPINNER[frame_idx % SA_SPINNER.len()];
            if !first {
                lines.push(Line::from(""));
            }
            for sa in &running_agents {
                // Last meaningful transcript line as the "current action"; fall
                // back to "starting…" when the transcript is still empty.
                let action = sa
                    .transcript
                    .last()
                    .map(|s| s.as_str())
                    .unwrap_or("starting…");
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(glyph.to_string(), Style::default().fg(palette.accent)),
                    Span::styled(
                        format!(" {} · {}", sa.agent_name, action),
                        Style::default().fg(palette.dim),
                    ),
                ]));
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
}

/// Build ONE message's visual block (the body, sans the fresh tool-call lines).
///
/// This is the per-message renderer the main transcript caches AND the
/// full-screen sub-agent viewer reuses, so both paths render identical markdown,
/// reasoning/thinking blocks, and compact tool-result rows.
///
/// - `User`     → `★` accent bullet, plain text.
/// - `Assistant`→ `●` bullet; native reasoning + wanderer "thinking" prefix
///   rendered dim+italic with the blockquote bar, then the body as markdown. The
///   per-tool-call lines are NOT included here — they carry a live ⚙→✓ glyph and
///   are appended fresh by [`render_tool_lines`] at assembly time.
/// - `Tool`     → compact dim one-liner (first line, truncated); a hidden harness
///   nudge yields an EMPTY block (no visual trace).
/// - `System`   → EMPTY block (never shown).
///
/// An empty `Vec` means "no visual block"; callers skip it (and its separator).

/// A visual unit for transcript rendering. Most messages map 1:1 to [`Single`],
/// but consecutive `bash_output` polls against the same background job are
/// collapsed into one [`VisualEntry::BashOutputGroup`] so the transcript does
/// not show repeated `bash_output({"job_id":"..."})` calls.
#[derive(Clone)]
enum VisualEntry<'a> {
    Single(&'a crate::dto::chat::ChatMessage),
    BashOutputGroup {
        last_call: &'a crate::dto::chat::ChatMessage,
        results: Vec<&'a crate::dto::chat::ChatMessage>,
    },
}

struct BashGroup<'a> {
    last_call: &'a crate::dto::chat::ChatMessage,
    results: Vec<&'a crate::dto::chat::ChatMessage>,
    end: usize,
}

fn build_visual_entries<'a>(
    committed: &[&'a crate::dto::chat::ChatMessage],
) -> Vec<VisualEntry<'a>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < committed.len() {
        if let Some(group) = try_read_bash_group(committed, i) {
            out.push(VisualEntry::BashOutputGroup {
                last_call: group.last_call,
                results: group.results,
            });
            i = group.end;
        } else {
            out.push(VisualEntry::Single(committed[i]));
            i += 1;
        }
    }
    out
}

fn parse_bash_output_job_id(args_json: &str) -> Option<String> {
    let sanitized = crate::dto::chat::sanitize_tool_arguments(args_json);
    let v: Value = serde_json::from_str(&sanitized).ok()?;
    v.get("job_id").and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn try_read_bash_group<'a>(
    committed: &[&'a crate::dto::chat::ChatMessage],
    start: usize,
) -> Option<BashGroup<'a>> {
    let call_msg = committed.get(start)?;
    let calls = call_msg.tool_calls.as_ref()?;
    if calls.len() != 1 || calls[0].function.name != "bash_output" {
        return None;
    }
    let job_id = parse_bash_output_job_id(&calls[0].function.arguments)?;
    let mut results = Vec::new();
    let mut i = start + 1;

    let res = committed.get(i)?;
    if res.role != Role::Tool || res.tool_call_id.as_deref() != Some(calls[0].id.as_str()) {
        return None;
    }
    results.push(*res);
    i += 1;

    loop {
        let next_call = committed.get(i)?;
        let next_calls = next_call.tool_calls.as_ref()?;
        if next_calls.len() != 1 || next_calls[0].function.name != "bash_output" {
            break;
        }
        let next_job_id = parse_bash_output_job_id(&next_calls[0].function.arguments)?;
        if next_job_id != job_id {
            break;
        }
        i += 1;
        let next_res = committed.get(i)?;
        if next_res.role != Role::Tool
            || next_res.tool_call_id.as_deref() != Some(next_calls[0].id.as_str())
        {
            break;
        }
        results.push(*next_res);
        i += 1;
    }

    Some(BashGroup {
        last_call: *call_msg,
        results,
        end: i,
    })
}

/// Render a collapsed `bash_output` poll group as one compact block:
///   ↳ background bash · bash-1 · [running] → [exit 0]
///       <captured output>
fn render_bash_output_group(
    group: &VisualEntry<'_>,
    palette: &Palette,
    _wrap_w: usize,
) -> Vec<Line<'static>> {
    let VisualEntry::BashOutputGroup { last_call, results } = group else {
        return Vec::new();
    };
    let mut lines = Vec::new();

    let job_id = last_call
        .tool_calls
        .as_ref()
        .and_then(|c| c.first())
        .and_then(|c| parse_bash_output_job_id(&c.function.arguments))
        .unwrap_or_else(|| "unknown".to_string());

    let mut statuses: Vec<&str> = Vec::new();
    for res in results {
        let status = res.content.lines().next().unwrap_or("").trim();
        if !status.is_empty() && statuses.last() != Some(&status) {
            statuses.push(status);
        }
    }
    let status_line = if statuses.is_empty() {
        String::new()
    } else {
        format!(" · {}", statuses.join(" → "))
    };

    let dim = Style::default().fg(palette.dim);
    let italic = Style::default()
        .fg(palette.dim)
        .add_modifier(Modifier::ITALIC);
    lines.push(Line::from(vec![
        Span::styled("↳ ".to_string(), italic),
        Span::styled(
            format!("background bash · {job_id}{status_line}"),
            italic,
        ),
    ]));

    let last_body = results.last().map(|m| m.content.as_str()).unwrap_or("");
    let output = last_body
        .lines()
        .skip(1)
        .skip_while(|l| l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if output.trim().is_empty() {
        lines.push(Line::from(vec![Span::styled("    (no output)", dim)]));
    } else {
        for line in output.lines() {
            lines.push(Line::from(vec![Span::styled(format!("    {line}"), dim)]));
        }
    }
    lines
}

pub(super) fn render_message_block(
    msg: &crate::dto::chat::ChatMessage,
    palette: &Palette,
    wrap_w: usize,
) -> Vec<Line<'static>> {
    match msg.role {
        Role::User => {
            // bg-bash completion nudge: render as ONE compact dim line with a green
            // `✓` (just the `[bash-N] status` summary, line 1 of the body). The model-
            // only context on the remaining lines is NOT shown. NOT a `★` user turn.
            if let Some(body) = msg.content.strip_prefix(crate::dto::chat::BASH_NUDGE_MARK) {
                return render_bash_nudge_block(body, palette);
            }
            // `!` user-shell shortcut entry: a SHELL_MARK-prefixed user message
            // carrying `$ <cmd>\n<output>`. Render it DISTINCTLY (not a `★` user
            // turn): a `$ <cmd>` header in the accent, then the captured output dim
            // and wrapped under it — visually a command + its result, not a message.
            if let Some(body) = msg.content.strip_prefix(crate::dto::chat::SHELL_MARK) {
                return render_shell_block(body, palette, wrap_w);
            }
            // The typed message (with any `[Image #N]` markers) in the accent
            // colour, then -- when the message carries image attachments -- a
            // permanent yellow/orange warn-style card listing them. The card is
            // ALWAYS yellow (a warn cue): titik can't guarantee the model read the
            // image, and the model-visible strip warning is injected separately at
            // send. Styled like a tool-call card (icon + dim text) but in warn.
            let mut lines = render_block(
                vec![vec![Span::styled(
                    msg.content.to_string(),
                    Style::default().fg(palette.accent),
                )]],
                "★ ",
                palette.accent,
                wrap_w,
                true,
            );
            lines.extend(render_attachment_card(&msg.attachments));
            lines
        }
        Role::Assistant => {
            // If the message contains wanderer lead-in lines (`Word: ...`), the
            // entire block up to and including the LAST such line is rendered
            // dim+italic (the "thinking" block); the remainder is markdown.
            let (thinking_block, response_body) = split_thinking(&msg.content);
            let thinking_style = Style::default()
                .fg(palette.dim)
                .add_modifier(Modifier::ITALIC);
            let bar_style = Style::default().fg(palette.dim);
            let mut logical: Vec<Vec<Span<'static>>> = Vec::new();
            // Native reasoning channel (the model's streamed `reasoning`, captured
            // separately from `content`). Rendered first, dim + italic, each line
            // prefixed with the blockquote bar so the whole thinking block reads as
            // quoted text. Display-only — it never re-enters the conversation or disk.
            let has_reasoning = msg.reasoning.as_deref().map(|r| !r.is_empty()).unwrap_or(false);
            let has_thinking = thinking_block.map(|t| !t.is_empty()).unwrap_or(false);
            if has_reasoning || has_thinking {
                logical.push(vec![Span::styled(
                    "\u{256d} thinking".to_string(),
                    Style::default().fg(palette.dim).add_modifier(Modifier::DIM),
                )]);
                if let Some(r2) = msg.reasoning.as_deref() {
                    if !r2.is_empty() {
                        for line in r2.lines() {
                            push_thinking_line(&mut logical, line, thinking_style, bar_style, wrap_w);
                        }
                    }
                }
                if let Some(tb) = thinking_block {
                    if !tb.is_empty() {
                        for line in tb.lines() {
                            push_thinking_line(&mut logical, line, thinking_style, bar_style, wrap_w);
                        }
                    }
                }
                logical.push(vec![Span::styled(
                    "\u{2570}\u{2500}".to_string(),
                    Style::default().fg(palette.dim).add_modifier(Modifier::DIM),
                )]);
            }
            // Blank line between the (barred) thinking block and the answer so the
            // quote→answer transition is clear. Only when there IS both.
            if !logical.is_empty() && !response_body.is_empty() {
                logical.push(vec![]);
            }
            if !response_body.is_empty() {
                logical.extend(crate::view::markdown::render(response_body, palette, wrap_w));
            }
            render_block(logical, "● ", palette.fg, wrap_w, false)
        }
        Role::Tool => {
            // Harness-internal tool results (the silent "plan first" nudge) carry a
            // hide-marker: fed to the model, never shown → EMPTY block.
            if msg.content.starts_with(crate::dto::chat::PLAN_NUDGE_MARK) {
                return Vec::new();
            }
            // Compact dim block: first non-empty line of the tool output, prefixed
            // with `\u21b3` so it reads as "output from the call above". Skips blank
            // leading lines (e.g. tool results that start with a blank line).
            let first = msg
                .content
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            let first = truncate_chars(first, 76);
            render_block(
                vec![vec![Span::styled(first, Style::default().fg(palette.dim))]],
                "  \u{21b3} ",
                palette.dim,
                wrap_w,
                false,
            )
        }
        Role::System => Vec::new(),
    }
}

/// Render a `!` user-shell entry's block: a `$ <cmd>` header (accent bullet +
/// command) over the captured output (dim, wrapped, hanging-indented).
///
/// `body` is the message content with the [`crate::dto::chat::SHELL_MARK`] prefix
/// already stripped, shaped `"$ <cmd>\n<output…>"`. The first line is the command
/// header; the remainder is the captured stdout+stderr (already ANSI-stripped and
/// output-capped at run time). The `$ ` bullet is split off the header so the
/// command renders right after an accent `$` glyph (no double `$`); an unexpectedly
/// header-less body degrades gracefully (the whole first line becomes the header).
fn render_shell_block(body: &str, palette: &Palette, wrap_w: usize) -> Vec<Line<'static>> {
    let mut lines = body.lines();
    let header = lines.next().unwrap_or("$");
    // Strip the leading "$ " so it can be re-emitted as the accent bullet.
    let cmd = header.strip_prefix("$ ").unwrap_or(header);

    let mut logical: Vec<Vec<Span<'static>>> = Vec::new();
    // Header line: the command in the accent colour (the `$ ` bullet is supplied by
    // render_block below).
    logical.push(vec![Span::styled(
        cmd.to_string(),
        Style::default().fg(palette.accent),
    )]);
    // Output lines: dim, one logical line each (wrapped by render_block). A blank
    // line is preserved as an empty logical line so output spacing is kept.
    for line in lines {
        logical.push(vec![Span::styled(
            line.to_string(),
            Style::default().fg(palette.dim),
        )]);
    }
    render_block(logical, "$ ", palette.accent, wrap_w, true)
}

/// Render a background-bash completion nudge as a single compact line: a GREEN
/// `✓` glyph followed by the dim per-job summary (line 1 of `body`). The remaining
/// lines of `body` are model-only context and are NOT displayed. Styled like a
/// tool-call sub-line (2-col indent + dim text), not a `★` user turn. The green is
/// hardcoded (theme-independent, like the orange attachment card) so the check
/// always reads as "success".
fn render_bash_nudge_block(body: &str, palette: &Palette) -> Vec<Line<'static>> {
    let summary = body.lines().next().unwrap_or("").to_string();
    let green = Color::Rgb(0, 200, 83);
    vec![Line::from(vec![
        Span::raw("  "),
        Span::styled("\u{2713} ", Style::default().fg(green)),
        Span::styled(summary, Style::default().fg(palette.dim)),
    ])]
}

/// Render the orange attachment folder-tree lines for a user message that
/// carries image attachments. Minimalist design: an "images" root line, then
/// one tree branch per attachment (├─ for non-last, └─ for the last).
/// Returns an empty `Vec` when there are no attachments.
///
/// ALWAYS orange-coloured (fixed Color::Rgb(255, 180, 60)), matching the approval
/// card in overlays.rs — independent of the theme palette so it always reads as
/// a warn cue.
fn render_attachment_card(
    attachments: &[crate::dto::chat::Attachment],
) -> Vec<Line<'static>> {
    if attachments.is_empty() {
        return Vec::new();
    }
    // Fixed orange colour matching the tool-approval card in overlays.rs.
    let orange = Color::Rgb(255, 180, 60);
    let style = Style::default().fg(orange);
    let dim = Style::default().fg(orange).add_modifier(Modifier::DIM);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Root: "  images"
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("images", style),
    ]));

    // One line per attachment, using tree connectors.
    let last_idx = attachments.len().saturating_sub(1);
    for (i, att) in attachments.iter().enumerate() {
        let connector = if i == last_idx {
            Span::styled("\u{2514}\u{2500} ", dim)  // └─
        } else {
            Span::styled("\u{251C}\u{2500} ", dim)  // ├─
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            connector,
            Span::styled(
                format!("[Image #{}] {}", att.marker_n, att.file_name()),
                style,
            ),
        ]));
    }
    lines
}

/// The fresh per-tool-call lines for an Assistant turn that requested calls.
///
/// Rendered fresh (never cached) so the leading glyph flips `⚙`→`✓` the moment
/// the matching tool result lands (a later round): a finished call (its id in
/// `completed`) gets an accent `✓ `; an in-flight one keeps the dim `⚙ `. Lines
/// hang under the `●` bullet with a 2-col indent, EXCEPT when the assistant body
/// is empty (`has_body == false`) — then the first tool line takes the `● ` bullet
/// so a pure tool-call turn isn't a bullet-less orphan. A non-Assistant message
/// or one with no tool calls yields no lines.
pub(super) fn render_tool_lines(
    msg: &crate::dto::chat::ChatMessage,
    completed: &std::collections::HashSet<&str>,
    has_body: bool,
    palette: &Palette,
) -> Vec<Line<'static>> {
    if msg.role != Role::Assistant {
        return Vec::new();
    }
    let Some(calls) = msg.tool_calls.as_ref() else {
        return Vec::new();
    };
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(calls.len());
    for (ci, call) in calls.iter().enumerate() {
        // For common tools, extract just the key arg instead of raw JSON so the
        // line is readable at a glance. Falls back to truncated raw JSON.
        let args = format_tool_args(&call.function.name, &call.function.arguments);
        let done = completed.contains(call.id.as_str());
        let (glyph, glyph_style) = if done {
            ("✓ ", Style::default().fg(palette.accent))
        } else {
            ("⚙ ", Style::default().fg(palette.dim))
        };
        let prefix = if !has_body && ci == 0 {
            Span::styled("● ", Style::default().fg(palette.fg))
        } else {
            Span::raw("  ")
        };
        lines.push(Line::from(vec![
            prefix,
            Span::styled(glyph, glyph_style),
            Span::styled(
                format!("{}({})", call.function.name, args),
                Style::default().fg(palette.dim),
            ),
        ]));

        // For background bash calls, append a dim+italic annotation sub-line.
        if call.function.name == "bash" {
            let parsed = serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                .unwrap_or_else(|_| serde_json::json!({}));
            let is_background = parsed
                .get("run_in_background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_background {
                let annotation_style = Style::default()
                    .fg(palette.dim)
                    .add_modifier(Modifier::ITALIC);
                lines.push(Line::from(vec![Span::styled(
                    "  ↳ running in background · /bash to manage",
                    annotation_style,
                )]));
            }
        }
    }
    lines
}


/// Extract a readable summary from a tool call's JSON arguments.
///
/// For the common built-in tools (read, write, edit, delete, bash, grep, glob,
/// dir_list) pull the most meaningful single field so the call line stays
/// readable without raw JSON. Falls back to truncated raw JSON for unknown tools.
fn format_tool_args(name: &str, args_json: &str) -> String {
    // Try to parse; if it fails, fall back to truncated raw.
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return truncate_chars(args_json, 60);
    };
    let key_field: Option<&str> = match name {
        "read" | "write" | "delete" => v.get("path").and_then(|v| v.as_str()),
        "edit" => v.get("path").and_then(|v| v.as_str()),
        "bash" => v.get("command").and_then(|v| v.as_str()),
        "grep" => v.get("pattern").and_then(|v| v.as_str()),
        "glob" => v.get("pattern").and_then(|v| v.as_str()),
        "dir_list" => {
            // paths is an array; show first element
            v.get("paths")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
        }
        _ => None,
    };
    match key_field {
        Some(s) => truncate_chars(s, 60),
        None => truncate_chars(args_json, 60),
    }
}

/// Assemble a full transcript from a flat `&[ChatMessage]` slice into styled
/// visual lines, EXACTLY like the main chat (markdown bodies, reasoning/thinking
/// blocks, blank separators, and live ⚙/✓ tool-call lines).
///
/// Used by the full-screen sub-agent viewer, which renders a sub-agent's
/// structured `messages` view-only. Unlike the main transcript this does NOT
/// cache (the viewer is opened occasionally, not every frame), but it reuses the
/// very same per-message renderer + tool-line builder, so the output is identical
/// to the main chat. System messages are skipped; hidden harness tool nudges
/// leave no trace.
pub(super) fn assemble_messages(
    messages: &[crate::dto::chat::ChatMessage],
    palette: &Palette,
    wrap_w: usize,
) -> Vec<Line<'static>> {
    // Which tool calls have COMPLETED: a `tool`-role result message whose
    // `tool_call_id` points back at the call. Built from the same slice so the
    // glyph state matches what the sub-agent actually did.
    let completed: std::collections::HashSet<&str> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut first = true;
    for msg in messages {
        let block = render_message_block(msg, palette, wrap_w);
        let has_body = !block.is_empty();
        let tool_lines = render_tool_lines(msg, &completed, has_body, palette);
        // Empty block with no tool lines (system / hidden harness) → no trace.
        if block.is_empty() && tool_lines.is_empty() {
            continue;
        }
        if !first {
            lines.push(Line::from(""));
        }
        first = false;
        lines.extend(block);
        lines.extend(tool_lines);
    }
    lines
}
