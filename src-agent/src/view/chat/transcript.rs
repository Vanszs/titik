//! Transcript area: committed messages, live streaming buffer, sub-agent
//! inline indicator, and the follow-scroll logic.

use ratatui::{
    layout::{Margin, Rect},
    style::{Modifier, Style},
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
                    // Markdown body only. The per-tool-call lines are NOT cached
                    // here: their leading glyph flips ⚙→✓ when the matching tool
                    // result arrives (a later round), so they're rendered fresh at
                    // frame assembly against `completed_tool_ids`. Caching them
                    // would freeze a stuck gear forever.
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
                    // Tool-call lines deliberately omitted here — rendered fresh at
                    // assembly so the ⚙→✓ completion glyph stays live (see above).
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
                    // truncated. Tool results are not markdown-rendered. The `↳`
                    // enter-arrow is dropped in favour of a plain dim indent so the
                    // line reads as a sub-item under its now-checked (`✓`) call —
                    // the finished turn renders as a checklist.
                    let first = msg.content.lines().next().unwrap_or("");
                    let first = truncate_chars(first, 80);
                    render_block(
                        vec![vec![Span::styled(first, Style::default().fg(palette.dim))]],
                        "    ",
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
            let has_body = !block.is_empty();
            let tool_lines: Vec<Line<'static>> = committed
                .get(i)
                .filter(|m| m.role == Role::Assistant)
                .and_then(|m| m.tool_calls.as_ref())
                .map(|calls| {
                    calls
                        .iter()
                        .enumerate()
                        .map(|(ci, call)| {
                            let args = truncate_chars(&call.function.arguments, 60);
                            let done = completed_tool_ids.contains(call.id.as_str());
                            let (glyph, glyph_style) = if done {
                                ("✓ ", Style::default().fg(palette.accent))
                            } else {
                                ("⚙ ", Style::default().fg(palette.dim))
                            };
                            // First line of a body-less block carries the bullet;
                            // every other tool line hangs under it with a 2-col indent.
                            let prefix = if !has_body && ci == 0 {
                                Span::styled("● ", Style::default().fg(palette.fg))
                            } else {
                                Span::raw("  ")
                            };
                            Line::from(vec![
                                prefix,
                                Span::styled(glyph, glyph_style),
                                Span::styled(
                                    format!("{}({})", call.function.name, args),
                                    Style::default().fg(palette.dim),
                                ),
                            ])
                        })
                        .collect()
                })
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

        // Sub-agent inline indicator: one animated line per RUNNING sub-agent,
        // appended at the bottom of the transcript so it sits just above the input
        // box and has full width. Uses the same time-driven braille spinner as the
        // compact animation (80ms/frame cadence). Only rendered while at least one
        // sub-agent is Running; disappears automatically when all finish.
        const SA_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let running_agents: Vec<&crate::app::subagent::SubAgent> = rest
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
