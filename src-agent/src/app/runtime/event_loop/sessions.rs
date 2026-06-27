//! Render-agnostic per-session servicing for the central event loop.
//!
//! [`service_all_sessions`] advances EVERY session's in-flight turn each tick:
//! it drains that session's streaming channel, its deferred tool-task channel,
//! and each of its sub-agents' channels, then resumes any round parked on
//! deferred work. It contains NO rendering / terminal / input code — purely
//! draining async results + driving turn state — so the same core can later run
//! headless in a daemon. The foreground-only / global concerns (the harness
//! verdict, the endpoint/warm/clipboard drains, the loading splash, input, the
//! redraw, …) stay in [`super::run_loop`].
//!
//! Multi-session readiness only: there is still exactly ONE session this stage
//! (no `/new`), so the `idx == 0 == foreground` path here is byte-for-byte the
//! same set of mutations the old inline foreground-only drains performed.

use std::sync::Arc;

use crate::app::state::AppState;
use crate::service::{openrouter::OpenRouterClient, StreamEvent};

use super::drains::apply_compaction_result;
use super::MIN_COMPACT_ANIM;
use super::super::stream::{
    advance_turn, finish_stream, resume_after_subagents, try_start_pending,
};

/// Service every session once: drain its async results and advance its turn
/// state. Returns `true` if anything changed (any event was applied / any turn
/// stepped), so the caller can flag a redraw. Render-agnostic: it never touches
/// the terminal, input, or any foreground-only / global drain.
///
/// For each session index `idx` (so a background session keeps streaming + running
/// tools + advancing its sub-agents while a different session is on screen) it does,
/// in the SAME relative order the old inline drains used for the foreground session:
///   1. drain `sessions[idx].active_rx` (stream tokens/usage/tool-calls/done/error/compacted),
///   2. drain `sessions[idx]`'s sub-agents (collect-then-apply, terminal delivery, queued starts),
///   3. drain `sessions[idx].tool_task_rx` (deferred heavy-tool results),
///   4. fire the resume gate when both deferred lanes for `idx` are empty.
pub(super) fn service_all_sessions(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> bool {
    let mut dirty = false;
    for idx in 0..state.rest.sessions.len() {
        dirty |= service_session(state, idx, client, handle);
    }
    dirty
}

/// Service a single session `idx`. Split out so the borrow patterns read the
/// same as the old foreground-only code with `idx` swapped in for `foreground`.
fn service_session(
    state: &mut AppState,
    idx: usize,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> bool {
    let mut dirty = false;

    // 1. Drain this session's stream events. take() the receiver so the match
    //    arms can mutate other fields of `state.rest` without a borrow conflict;
    //    put it back if the stream is still open.
    if let Some(mut rx) = state.rest.sessions[idx].active_rx.take() {
        let mut still_streaming = true;
        while let Ok(event) = rx.try_recv() {
            dirty = true;
            match event {
                StreamEvent::Token(t) => {
                    state.rest.sessions[idx].append_token(&t);
                    state.rest.status = "streaming".into();
                }
                StreamEvent::Reasoning(t) => {
                    // Accumulate the model's thinking into the parallel buffer;
                    // `dirty` is already set so it animates in like content.
                    state.rest.sessions[idx].append_reasoning(&t);
                    state.rest.status = "thinking".into();
                }
                StreamEvent::Usage { prompt_tokens, completion_tokens, cached_tokens, cost } => {
                    // Stash for the assistant-commit step; do NOT break — usage
                    // arrives just before Done.
                    state.rest.sessions[idx].pending_usage = Some((prompt_tokens, completion_tokens, cost));
                    // Cached-prompt-token count for THIS prompt (current context,
                    // like tokens_in — not cumulative). Set straight away so the
                    // readout can show the cache hit even on a tool round-trip that
                    // commits no assistant text.
                    state.rest.tokens_cached = cached_tokens;
                    // Latch: once any response reports cache hits we know this
                    // provider supports prompt caching. Never reset.
                    if cached_tokens > 0 {
                        state.rest.sessions[idx].provider_caches = true;
                    }
                }
                StreamEvent::ToolCalls(calls) => {
                    // Stash the requested tool calls; do NOT break — Done follows
                    // and `advance_turn` consumes them there.
                    state.rest.sessions[idx].pending_tool_calls = calls;
                }
                StreamEvent::Done => {
                    // Drive the turn: commit the assistant message and either end
                    // the turn or run tools + continue (which spawns the next task
                    // into a fresh active_rx).
                    advance_turn(state, idx, client, handle);
                    still_streaming = false;
                    break;
                }
                StreamEvent::Error(e) => {
                    // Surface the error and halt the whole turn (drop any
                    // half-stashed tool calls / step count / approval machine).
                    finish_stream(&mut state.rest, idx, Some(e));
                    state.rest.sessions[idx].agent_steps = 0;
                    state.rest.sessions[idx].pending_tool_calls.clear();
                    state.rest.sessions[idx].awaiting_approval = false;
                    state.rest.sessions[idx].approval_reason = None;
                    state.rest.sessions[idx].tool_idx = 0;
                    state.rest.sessions[idx].tool_results.clear();
                    // Clear any in-flight compaction animation so a failed
                    // compaction (e.g. null content decode error) doesn't leave the
                    // spinner stuck driving per-tick redraws indefinitely.
                    state.rest.compact_anim_start = None;
                    state.rest.compact_apply_at = None;
                    state.rest.compact_pending = None;
                    still_streaming = false;
                    break;
                }
                StreamEvent::Compacted { summary, kept_tail } => {
                    // The model is done; the task is finished either way.
                    state.rest.sessions[idx].current_task = None;
                    // Enforce a short cosmetic minimum so a fast compaction doesn't
                    // flash the animation. If we haven't shown the animation long
                    // enough yet, stash the result and defer the apply to a later
                    // tick (NON-blocking — never sleep).
                    let elapsed = state
                        .rest
                        .compact_anim_start
                        .map(|t| t.elapsed())
                        .unwrap_or(MIN_COMPACT_ANIM);
                    if elapsed < MIN_COMPACT_ANIM {
                        let start = state.rest.compact_anim_start.unwrap();
                        state.rest.compact_apply_at = Some(start + MIN_COMPACT_ANIM);
                        state.rest.compact_pending = Some((summary, kept_tail));
                        // Keep `waiting` true so the 8ms poll + per-tick redraw keep
                        // the animation running until the gate opens.
                    } else {
                        apply_compaction_result(state, client, handle, summary, kept_tail);
                    }
                    still_streaming = false;
                    break;
                }
                // The advisory PC verdict is delivered on the dedicated
                // `harness_rx` channel (drained in run_loop), and the per-model
                // provider endpoints on `endpoints_rx` (also in run_loop) — never on
                // a streaming request's channel. So these arms are unreachable here;
                // ignore them to keep the match exhaustive without affecting the
                // stream.
                StreamEvent::HarnessVerdict { .. }
                | StreamEvent::EndpointsLoaded { .. }
                | StreamEvent::EndpointsError { .. } => {}
            }
        }
        if still_streaming {
            state.rest.sessions[idx].active_rx = Some(rx);
        }
    }

    // 2. Drain each sub-agent's event channel for THIS session. Sub-agents live
    //    in `state.rest.sessions[idx].subagents`; each has its own `rx`. We
    //    iterate by index so we can reborrow mutably after collecting events into
    //    a local vec (borrow checker: the collect loop holds &mut subagents[i].rx;
    //    the apply loop needs &mut subagents[i].{status,transcript} and later
    //    &mut session). Pattern mirrors warm_rx: try_recv() in a loop, dirty=true
    //    on any event.
    {
        use crate::app::subagent::{AgentEvent, SubAgentStatus};

        // Char-safe truncation helper (avoids panicking on multibyte boundaries).
        fn trunc(s: &str, max: usize) -> String {
            if s.chars().count() <= max {
                s.to_string()
            } else {
                let cut: String = s.chars().take(max).collect();
                format!("{cut}…")
            }
        }

        // Deferred `task`-tool results to deliver into the PARKED tool round
        // (call_id, result_text), accumulated across every sub-agent this tick and
        // applied after the loop. A sub-agent that reaches a terminal state and
        // still has its call id in `pending_subagent_calls` fills its result here
        // (the FULL report on Done, an error/killed note otherwise) so the parked
        // round can resume with no dangling tool_call ids.
        let mut deferred_results: Vec<(String, String)> = Vec::new();

        for i in 0..state.rest.sessions[idx].subagents.len() {
            // --- collect phase: drain rx into a local vec ---
            let mut disconnected = false;
            let events: Vec<AgentEvent> = {
                let sa = &mut state.rest.sessions[idx].subagents[i];
                let mut evs = Vec::new();
                loop {
                    match sa.rx.try_recv() {
                        Ok(ev) => evs.push(ev),
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                evs
            };

            // Channel closed (task ended): mark Killed if still Running.
            if disconnected {
                let sa = &mut state.rest.sessions[idx].subagents[i];
                if matches!(sa.status, SubAgentStatus::Running) {
                    sa.status = SubAgentStatus::Killed;
                    dirty = true;
                }
            }

            // --- apply phase: fold events onto the sub-agent ---
            // The task-tool path delivers its result via `deferred_results`
            // (computed from the settled status below), not here.
            if !events.is_empty() {
                dirty = true;
                let sa = &mut state.rest.sessions[idx].subagents[i];
                for ev in events {
                    match ev {
                        AgentEvent::Token(t) => {
                            // Merge consecutive token chunks into the last transcript
                            // line when it is still a "token" line (not a marker line)
                            // and short. Push a new line otherwise.
                            let is_marker = sa.transcript.last().is_some_and(|l| {
                                l.starts_with("— ")
                                    || l.starts_with("→ ")
                                    || l.starts_with("✓ ")
                                    || l.starts_with("done:")
                                    || l.starts_with("error:")
                            });
                            if !is_marker
                                && sa.transcript.last().is_some_and(|l| l.len() < 200)
                            {
                                if let Some(last) = sa.transcript.last_mut() {
                                    last.push_str(&t);
                                }
                            } else {
                                sa.transcript.push(t);
                            }
                            // Cap growth at ~200 lines.
                            if sa.transcript.len() > 200 {
                                let drop = sa.transcript.len() - 200;
                                sa.transcript.drain(..drop);
                            }
                        }
                        AgentEvent::Step(n) => {
                            sa.transcript.push(format!("— step {n} —"));
                        }
                        AgentEvent::Snapshot(m) => {
                            // Replace the structured history wholesale; drives the
                            // full-screen sub-agent viewer.
                            sa.messages = m;
                        }
                        AgentEvent::ToolStarted { name, args } => {
                            sa.transcript.push(format!("→ {name} {}", trunc(&args, 120)));
                        }
                        AgentEvent::ToolDone { name, result } => {
                            let first = result.lines().next().unwrap_or("").trim();
                            sa.transcript.push(format!("✓ {name}: {}", trunc(first, 120)));
                        }
                        AgentEvent::Done(s) => {
                            sa.transcript.push(format!("done: {}", trunc(&s, 200)));
                            sa.status = SubAgentStatus::Done(s);
                        }
                        AgentEvent::Error(e) => {
                            sa.transcript.push(format!("error: {e}"));
                            sa.status = SubAgentStatus::Error(e);
                        }
                        AgentEvent::UsageReport { model_id, tokens_in, tokens_out, cost } => {
                            // Overwrite with the final report's values; the loop
                            // emits exactly one UsageReport (just before Done).
                            sa.model_id = model_id;
                            sa.usage_tokens_in = tokens_in;
                            sa.usage_tokens_out = tokens_out;
                            sa.usage_cost = cost;
                        }
                    }
                }
            }

            // --- terminal delivery / fold ---
            // Inspect the SETTLED status + origin once events are folded, capturing
            // owned values up front so the immutable borrow of `subagents[i]` is
            // released before any `sessions[idx].session` mutation below. Runs every
            // tick (even when no events arrived, so a disconnect-only Killed is still
            // delivered). The "still in pending_subagent_calls" guard makes a
            // task-tool delivery happen EXACTLY ONCE (the id is removed after the
            // loop, so later ticks skip it).
            //
            // `chat_fold` carries the /task chat-fold note; `defer` carries the
            // (call_id, result) for the task-tool deferred delivery. `sub_usage`
            // carries (model_id, tokens_in, tokens_out, cost) to merge+record when
            // the sub-agent reaches any terminal state. At most one of chat_fold /
            // defer is Some (the two origins are mutually exclusive on tool_call_id).
            // sub_usage is Some whenever the status is terminal and usage > 0.
            let (chat_fold, defer, sub_usage) = {
                let sa = &state.rest.sessions[idx].subagents[i];
                // Capture usage once; only carry it if there is something to record.
                let usage_tuple = if sa.usage_tokens_out > 0 || sa.usage_cost > 0.0 {
                    Some((
                        sa.model_id.clone(),
                        sa.usage_tokens_in,
                        sa.usage_tokens_out,
                        sa.usage_cost,
                    ))
                } else {
                    None
                };
                match (&sa.tool_call_id, &sa.status) {
                    // task-tool path: deliver the deferred result back to the model.
                    (Some(call_id), status)
                        if state.rest.sessions[idx].pending_subagent_calls.contains(call_id) =>
                    {
                        let result = match status {
                            // Deliver the FULL, untruncated report.
                            SubAgentStatus::Done(s) => Some(s.clone()),
                            SubAgentStatus::Error(e) => Some(format!("sub-agent error: {e}")),
                            // Killed (user Ctrl+X / task died) — fill so the round
                            // can't hang waiting on a result that will never come.
                            SubAgentStatus::Killed => Some("[sub-agent killed]".to_string()),
                            // Still running: nothing to deliver this tick.
                            SubAgentStatus::Running => None,
                        };
                        // Only carry usage on a terminal transition (result is Some).
                        let carry_usage = if result.is_some() { usage_tuple } else { None };
                        (None, result.map(|r| (call_id.clone(), r)), carry_usage)
                    }
                    // /task command path (tool_call_id == None): on Done, build the
                    // FULL, untruncated report note (injected as an assistant turn
                    // below). Done is terminal and the agent is pruned this tick, so
                    // it fires once.
                    (None, SubAgentStatus::Done(result)) => (
                        Some(format!(
                            "[sub-agent #{} {}] finished: {result}",
                            sa.id, sa.agent_name
                        )),
                        None,
                        usage_tuple,
                    ),
                    _ => (None, None, None),
                }
            };
            if let Some(note) = chat_fold {
                // /task command path: append the full report as a display-only
                // assistant turn so the session retains a complete record.
                if let Some(sess) = state.rest.sessions[idx].session.as_mut() {
                    // Log to sqlite (no usage/cost for a sub-agent fold).
                    let _ = crate::model::msglog::append(
                        &sess.path,
                        crate::dto::chat::Role::Assistant,
                        &note,
                        None,
                    );
                    sess.conversation.push_assistant(note, None);
                    let _ = sess.save();
                }
            }
            // Merge sub-agent spend into the session total + record a ledger row.
            // Done for BOTH paths (chat_fold = /task, defer = task-tool) at the
            // single point where a terminal status is first observed. Non-fatal:
            // skipped when no usage was ever reported (provider omits it). The
            // cost/tokens_out counters stay GLOBAL on `rest` (kept as-is this stage).
            if let Some((sub_model_id, sub_ti, sub_to, sub_cost)) = sub_usage {
                // Merge into the counters: cost and tokens_out are cumulative
                // (summed); tokens_in is the main-context gauge and must NOT be
                // touched (adding sub-agent prompt size would corrupt the
                // context-window display).
                state.rest.cost += sub_cost;
                state.rest.tokens_out += sub_to;
                // Record one ledger row per sub-agent completion (best-effort).
                let (sess_uuid, pwd_hash) = state
                    .rest
                    .sessions[idx]
                    .session
                    .as_ref()
                    .map(|s| (s.id.clone(), s.pwd_hash.clone()))
                    .unwrap_or_default();
                let sa_name = state.rest.sessions[idx].subagents[i].agent_name.clone();
                crate::model::usage::record_usage(
                    &sub_model_id,
                    &format!("sub:{sa_name}"),
                    &sess_uuid,
                    &pwd_hash,
                    sub_ti,
                    0, // sub-agents never receive cached-tokens data
                    sub_to,
                    sub_cost,
                );
            }
            if let Some(pair) = defer {
                deferred_results.push(pair);
            }
        }

        // Deliver every terminal task-tool result into the parked round's
        // `tool_results` and drop its id from `pending_subagent_calls`. Done AFTER
        // the loop so the per-agent borrow above stays immutable.
        for (call_id, result) in deferred_results {
            state.rest.sessions[idx].pending_subagent_calls.retain(|c| c != &call_id);
            state.rest.sessions[idx].tool_results.push((call_id, result));
            dirty = true;
        }

        // --- keep terminated sub-agents as session history ---
        // Terminated agents (Done, Error, Killed) are NOT pruned: the $ panel is a
        // session history, so every sub-agent that ran stays in the list with its
        // final status + structured `messages` for later viewing.
        // `running_subagents()` still counts only `Running`, so the cap is
        // unaffected. The list only ever grows here, so `subagent_sel` (always <
        // len once set) can never fall out of range — no clamp needed.

        // --- start queued delegations into any freed slots ---
        // A terminal handle above may have freed a slot. Start as many pending
        // sub-agents (FRONT-first) as now fit. Done BEFORE the resume gate below: a
        // queued task-tool delegation keeps its call id in `pending_subagent_calls`
        // across the queued→running transition, and an unstartable entry delivers
        // its error result + drops its id HERE — so the
        // `pending_subagent_calls.is_empty()` test sees the settled set and can't
        // resume a round that still has a queued delegation outstanding.
        if !state.rest.sessions[idx].pending_subagents.is_empty() {
            try_start_pending(state, idx, client, handle);
            dirty = true;
        }

        // --- drain deferred tool-task results (heavy/blocking tools) ---
        // Deferred tools (read/write/edit/delete/bash/grep/glob/remember/
        // web_fetch/web_search) run on a plain std::thread (spawned in
        // `dispatch_deferred`) and send their `(call_id, result)` back over
        // `tool_task_rx`. Fold each into the PARKED round's `tool_results` and drop
        // its id from `pending_tool_tasks`, exactly mirroring the sub-agent deferral
        // — so the resume gate below sees the settled set. Done within this same
        // block (before the gate) so both lanes' results are in place when emptiness
        // is tested. A round runs its deferred tools ONE AT A TIME, so at most one
        // id settles here per resume.
        {
            // Drain into a local vec FIRST inside a narrow scope so the `rx` borrow
            // of this session's runtime is released before we touch
            // `pending_tool_tasks` / `tool_results` on the same runtime below.
            let mut received: Vec<(String, String)> = Vec::new();
            if let Some(rx) = state.rest.sessions[idx].tool_task_rx.as_mut() {
                while let Ok(pair) = rx.try_recv() {
                    received.push(pair);
                }
            }
            // Fold only results whose id is still in pending_tool_tasks; anything
            // else is a stale delivery from a killed/interrupted turn and must be
            // discarded rather than corrupting the next turn.
            for (id, result) in received {
                if let Some(pos) = state.rest.sessions[idx].pending_tool_tasks.iter().position(|c| c == &id) {
                    state.rest.sessions[idx].pending_tool_tasks.remove(pos);
                    state.rest.sessions[idx].tool_results.push((id, result));
                    dirty = true;
                }
                // else: stale delivery — drop silently
            }
        }

        // --- resume a round parked on deferred work (BOTH lanes) ---
        // Unpark only when EVERY deferred id — sub-agent delegations AND deferred
        // tool tasks — has filled its result (above). The resume
        // (`resume_after_subagents`) RE-ENTERS `process_tools` at the advanced
        // `tool_idx` to CONTINUE the round: a deferred heavy tool dispatched the NEXT
        // call (and may park again), making the lane SEQUENTIAL; once the round has
        // no further deferred work it falls through to `finish_tool_round`, which
        // flushes ALL collected `tool_results` and re-streams so the MAIN AGENT
        // reacts. Clearing both awaiting flags drops the parked status; `waiting`
        // stays true through the re-stream. Gating on both lists means a mixed round
        // waits for the last pending id of either kind before resuming — no dangling
        // tool_call ids.
        if (state.rest.sessions[idx].awaiting_subagents || state.rest.sessions[idx].awaiting_tool_tasks)
            && state.rest.sessions[idx].pending_subagent_calls.is_empty()
            && state.rest.sessions[idx].pending_tool_tasks.is_empty()
        {
            state.rest.sessions[idx].awaiting_subagents = false;
            state.rest.sessions[idx].awaiting_tool_tasks = false;
            resume_after_subagents(state, idx, client, handle);
            dirty = true;
        }
    }

    dirty
}
