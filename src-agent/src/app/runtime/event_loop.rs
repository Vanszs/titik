//! Central event loop: drain stream events, poll terminal input, redraw.

use std::io::{stdout, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

use crate::app::mode::Mode;
use crate::app::state::AppState;
use crate::controller;
use crate::dto::chat::Role;
use crate::service::{openrouter::OpenRouterClient, StreamEvent};
use crate::view;

use super::actions::apply_action;
use super::stream::{advance_turn, finish_stream};
use super::Term;

/// Leave the alternate screen + disable mouse capture, then print the full
/// conversation as plain text so the user can select/copy with the terminal's
/// native selection. Raw mode stays on (we read a single key to return), so
/// lines are terminated with `\r\n`.
fn enter_select(rest: &crate::app::state::AppStateRest) -> Result<()> {
    execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    let mut out = stdout();
    if let Some(sess) = rest.session.as_ref() {
        for m in sess.conversation.messages() {
            let label = match m.role {
                Role::System | Role::Tool => continue,
                Role::User => "you",
                Role::Assistant => "ai",
            };
            write!(out, "\r\n{label}:\r\n")?;
            for line in m.content.split('\n') {
                write!(out, "{line}\r\n")?;
            }
        }
    }
    write!(out, "\r\n-- copy with your mouse, then press any key to return --\r\n")?;
    out.flush()?;
    Ok(())
}

/// Re-enter the alternate screen + mouse capture and force a full repaint.
fn exit_select(terminal: &mut Term) -> Result<()> {
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;
    Ok(())
}

/// Minimum on-screen duration for the `/compact` animation. Cosmetic and short:
/// a fast compaction is held this long (via a deferred apply) so the spinner +
/// progress bar don't merely flash. Deliberately ~1s — long enough to read, not
/// long enough to feel like a stall.
const MIN_COMPACT_ANIM: Duration = Duration::from_millis(1000);

/// Apply a finished compaction to the active session and finalize the UI.
///
/// This is the single apply path shared by both the immediate case (the model
/// already took >= the minimum animation time) and the deferred case (a fast
/// compaction held back by [`MIN_COMPACT_ANIM`]). It:
/// - rebuilds the conversation (`apply_compaction` + `rebuild_system`) and saves,
/// - refreshes the project-awareness summary (best-effort, gated by the setting),
/// - invalidates the transcript cache so the same-length REPLACE doesn't leave a
///   stale prefix (the summary is the new first block),
/// - scrolls to the TOP so the user sees the fresh summary,
/// - surfaces the summary text as a neutral (info) toast under the finish, and
/// - clears the waiting/animation state.
fn apply_compaction_result(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
    summary: String,
    kept_tail: Vec<crate::dto::chat::ChatMessage>,
) {
    if let Some(sess) = state.rest.session.as_mut() {
        sess.conversation
            .apply_compaction(summary.clone(), kept_tail);
        sess.rebuild_system();
        let _ = sess.save();
    }
    // Refresh the project-awareness summary post-compaction: the project is often
    // better understood after a compact, and this also satisfies the "applies on
    // compaction" requirement. Best-effort; gated by `awareness_enabled` inside
    // `summarize`. Clone the inputs out first so the `block_on` doesn't hold a
    // borrow of `state.rest`.
    let aware_inputs = match (client.as_ref(), state.rest.session.as_ref()) {
        (Some(c), Some(sess)) if sess.settings.awareness_enabled => {
            Some((Arc::clone(c), sess.settings.clone(), sess.workdir()))
        }
        _ => None,
    };
    if let Some((c, settings, workdir)) = aware_inputs {
        let s = handle.block_on(crate::app::awareness::summarize(&c, &settings, &workdir));
        state.rest.awareness_summary = s;
    }

    // The transcript cache only rebuilds on a length SHRINK; compaction can be a
    // same-length REPLACE, which would leave a stale prefix. Force a full rebuild
    // so the new summary (first Assistant block) + kept tail render correctly.
    state.rest.transcript_cache.borrow_mut().blocks.clear();
    // Jump to the top of the transcript so the freshly-written summary is what the
    // user sees once the animation clears (instead of the kept tail at the bottom).
    state.rest.follow = false;
    state.rest.scroll = 0;

    // Surface the generated summary "under the finish animation" as a neutral,
    // multi-line info toast (capped so a long summary stays contained).
    state
        .rest
        .set_toast_info(format!("compacted ✓\n{}", cap_summary(&summary, 400)));

    state.rest.waiting = false;
    state.rest.status = "ready".into();
    // Animation is done: stop the per-tick redraw + drop any deferral bookkeeping.
    state.rest.compact_anim_start = None;
    state.rest.compact_apply_at = None;
    state.rest.compact_pending = None;
}

/// Trim and cap a summary for toast display: collapse leading/trailing
/// whitespace, then keep at most `max` characters, appending an ellipsis when cut.
fn cap_summary(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// The central event loop. Each tick: redraw if dirty, drain the active
/// request's events, then drain all buffered terminal input. Rendering is
/// dirty-flagged and polling is adaptive (8ms streaming / 100ms idle) so an
/// idle UI is effectively free while streaming stays at >=60fps.
pub(super) fn run_loop(
    terminal: &mut Term,
    state: &mut AppState,
    handle: &tokio::runtime::Handle,
    client: &mut Option<Arc<OpenRouterClient>>,
) -> Result<()> {
    let mut dirty = true; // paint once on entry
    loop {
        // Perform a pending /select hand-off: drop to the normal terminal and
        // dump the conversation, then suppress TUI painting until a key returns.
        if state.rest.select_pending {
            state.rest.select_pending = false;
            enter_select(&state.rest)?;
            state.rest.select_active = true;
        }

        if dirty && !state.rest.select_active {
            terminal.draw(|f| view::draw(f, state))?;
            dirty = false;
        }

        // 1. Drain the active stream's events. take() the receiver so the match
        //    arms can mutate other fields of state.rest without a borrow
        //    conflict; put it back if the stream is still open.
        if let Some(mut rx) = state.rest.active_rx.take() {
            let mut still_streaming = true;
            while let Ok(event) = rx.try_recv() {
                dirty = true;
                match event {
                    StreamEvent::Token(t) => {
                        state.rest.append_token(&t);
                        state.rest.status = "streaming".into();
                    }
                    StreamEvent::Reasoning(t) => {
                        // Accumulate the model's thinking into the parallel buffer;
                        // `dirty` is already set so it animates in like content.
                        state.rest.append_reasoning(&t);
                        state.rest.status = "thinking".into();
                    }
                    StreamEvent::Usage { prompt_tokens, completion_tokens, cached_tokens, cost } => {
                        // Stash for the assistant-commit step; do NOT break —
                        // usage arrives just before Done.
                        state.rest.pending_usage = Some((prompt_tokens, completion_tokens, cost));
                        // Cached-prompt-token count for THIS prompt (current
                        // context, like tokens_in — not cumulative). Set straight
                        // away so the readout can show the cache hit even on a
                        // tool round-trip that commits no assistant text.
                        state.rest.tokens_cached = cached_tokens;
                        // Latch: once any response reports cache hits we know this
                        // provider supports prompt caching. Never reset.
                        if cached_tokens > 0 {
                            state.rest.provider_caches = true;
                        }
                    }
                    StreamEvent::ToolCalls(calls) => {
                        // Stash the requested tool calls; do NOT break — Done
                        // follows and `advance_turn` consumes them there.
                        state.rest.pending_tool_calls = calls;
                    }
                    StreamEvent::Done => {
                        // Drive the turn: commit the assistant message and either
                        // end the turn or run tools + continue (which spawns the
                        // next task into a fresh active_rx).
                        advance_turn(state, client, handle);
                        still_streaming = false;
                        break;
                    }
                    StreamEvent::Error(e) => {
                        // Surface the error and halt the whole turn (drop any
                        // half-stashed tool calls / step count / approval machine).
                        finish_stream(&mut state.rest, Some(e));
                        state.rest.agent_steps = 0;
                        state.rest.pending_tool_calls.clear();
                        state.rest.awaiting_approval = false;
                        state.rest.approval_reason = None;
                        state.rest.tool_idx = 0;
                        state.rest.tool_results.clear();
                        // Clear any in-flight compaction animation so a failed
                        // compaction (e.g. null content decode error) doesn't leave
                        // the spinner stuck driving per-tick redraws indefinitely.
                        state.rest.compact_anim_start = None;
                        state.rest.compact_apply_at = None;
                        state.rest.compact_pending = None;
                        still_streaming = false;
                        break;
                    }
                    StreamEvent::Compacted { summary, kept_tail } => {
                        // The model is done; the task is finished either way.
                        state.rest.current_task = None;
                        // Enforce a short cosmetic minimum so a fast compaction
                        // doesn't flash the animation. If we haven't shown the
                        // animation long enough yet, stash the result and defer
                        // the apply to a later tick (NON-blocking — never sleep).
                        let elapsed = state
                            .rest
                            .compact_anim_start
                            .map(|t| t.elapsed())
                            .unwrap_or(MIN_COMPACT_ANIM);
                        if elapsed < MIN_COMPACT_ANIM {
                            let start = state.rest.compact_anim_start.unwrap();
                            state.rest.compact_apply_at = Some(start + MIN_COMPACT_ANIM);
                            state.rest.compact_pending = Some((summary, kept_tail));
                            // Keep `waiting` true so the 8ms poll + per-tick redraw
                            // keep the animation running until the gate opens.
                        } else {
                            apply_compaction_result(state, client, handle, summary, kept_tail);
                        }
                        still_streaming = false;
                        break;
                    }
                    // The advisory PC verdict is delivered on the dedicated
                    // `harness_rx` channel (drained below), never on a streaming
                    // request's channel — so this arm is unreachable here. Ignore
                    // it to keep the match exhaustive without affecting the stream.
                    StreamEvent::HarnessVerdict { .. } => {}
                }
            }
            if still_streaming {
                state.rest.active_rx = Some(rx);
            }
        }

        // 1b. Drain the advisory prompt-classifier (PC) channel. This is fully
        //     independent of streaming: a BLOCK verdict only raises a toast; the
        //     turn already proceeded and is never cancelled here. Take() the
        //     receiver so the match can mutate state.rest; put it back unless the
        //     PC task has finished (channel closed) or delivered its verdict.
        if let Some(mut hrx) = state.rest.harness_rx.take() {
            let mut keep = true;
            while let Ok(event) = hrx.try_recv() {
                if let StreamEvent::HarnessVerdict { allow, reason } = event {
                    if !allow {
                        let reason = if reason.is_empty() { "flagged".into() } else { reason };
                        state.rest.set_toast(format!("harness flagged: {reason}"));
                        dirty = true;
                    }
                    // One verdict per turn; stop listening on this channel.
                    keep = false;
                    break;
                }
            }
            if keep {
                state.rest.harness_rx = Some(hrx);
            }
        }

        // 1c. Deferred compaction apply. A fast compaction stashes its result and
        //     an `apply_at` instant so the animation holds for a short minimum
        //     (cosmetic). Apply once that instant passes — driven by the loop tick,
        //     never by sleeping, so input/animation stay responsive meanwhile.
        if let Some(apply_at) = state.rest.compact_apply_at {
            if std::time::Instant::now() >= apply_at {
                if let Some((summary, kept_tail)) = state.rest.compact_pending.take() {
                    apply_compaction_result(state, client, handle, summary, kept_tail);
                }
                state.rest.compact_apply_at = None;
                dirty = true;
            }
        }

        // When a background reindex has SETTLED (not indexing), warn once about
        // any workspace root missing on disk. Keyed on the missing set CHANGING
        // vs what we last warned, so it fires exactly once per change and does
        // not depend on catching the brief indexing=true window (an all-missing
        // reindex can finish before the loop ever observes it).
        let (indexing_now, missing_now) = match state.rest.dir_cache.read() {
            Ok(c) => (c.indexing, c.missing_roots.clone()),
            Err(_) => (true, state.rest.warned_missing_roots.clone()),
        };
        if !indexing_now && missing_now != state.rest.warned_missing_roots {
            if !missing_now.is_empty() {
                state.rest.set_toast_info(format!(
                    "workspace root(s) not found on disk:\n{}\nfix the path in /settings",
                    missing_now.join("\n")
                ));
                dirty = true;
            }
            state.rest.warned_missing_roots = missing_now;
        }

        // Status-line "comet" activity clock. Shimmer is active whenever the app
        // is in a WORKING wait that isn't paused on a y/n approval. Reconcile
        // `work_since` against that on the rising/falling edge here (the single
        // place that sees the settled `waiting`/`awaiting_approval` for the tick),
        // rather than threading set/clear through every scattered mutation site:
        //  - rising edge (active && None)   → stamp `now` so the elapsed counter
        //    and the travelling head start from this moment.
        //  - falling edge (!active && Some) → clear it; idle / approval renders the
        //    status statically with no comet and no timer.
        let shimmer_active = state.rest.waiting && !state.rest.awaiting_approval;
        match (shimmer_active, state.rest.work_since.is_some()) {
            (true, false) => state.rest.work_since = Some(std::time::Instant::now()),
            (false, true) => state.rest.work_since = None,
            _ => {}
        }

        // While a compaction animation is in flight, mark every tick dirty so the
        // spinner/elapsed/bar actually advance (rendering is otherwise only
        // event-driven). The 8ms `waiting` poll above sets the frame cadence.
        // The same applies while the comet shimmer is active: it must keep
        // travelling even when NO stream events arrive (first-token latency, tool
        // exec, the summarizer fold), so force a redraw each tick then too.
        if state.rest.compact_anim_start.is_some() || shimmer_active {
            dirty = true;
        }

        // 2. Input poll cadence. While WORKING (waiting), poll fast so two things
        //    stay smooth: tokens flush at >=60fps when a stream is live, and the
        //    comet redraws at ~12fps (80ms) even when nothing streams (the 8ms
        //    poll is the upper bound on the redraw interval the comet needs). Idle
        //    falls back to 100ms (poll still wakes instantly on a keypress, so
        //    typing latency is 0) so a fully idle UI never busy-spins. Drain EVERY
        //    buffered event each tick so paste / fast typing don't lag.
        let timeout = if state.rest.waiting {
            Duration::from_millis(8)
        } else {
            Duration::from_millis(100)
        };
        if event::poll(timeout)? {
            while event::poll(Duration::ZERO)? {
                match event::read()? {
                    Event::Key(key) => {
                        if state.rest.select_active {
                            // Any key returns from /select copy mode.
                            exit_select(terminal)?;
                            state.rest.select_active = false;
                            dirty = true;
                        } else {
                            let action = controller::input::handle_key(state, key);
                            apply_action(action, state, client, handle)?;
                            dirty = true;
                        }
                    }
                    Event::Mouse(m) => {
                        // Wheel scrolls the chat transcript only.
                        if matches!(state.mode, Mode::Chat) {
                            match m.kind {
                                MouseEventKind::ScrollUp => {
                                    for _ in 0..3 { state.rest.scroll_up(); }
                                    dirty = true;
                                }
                                MouseEventKind::ScrollDown => {
                                    for _ in 0..3 { state.rest.scroll_down(); }
                                    dirty = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    Event::Resize(_, _) => dirty = true,
                    Event::Paste(text) => {
                        if !state.rest.select_active {
                            controller::input::handle_paste(state, &text);
                            dirty = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        // Auto-dismiss an expired error toast.
        if state.rest.tick_toast() {
            dirty = true;
        }

        if state.rest.should_quit {
            break;
        }
    }
    Ok(())
}
