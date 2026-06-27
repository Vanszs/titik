//! Central event loop: drain stream events, poll terminal input, redraw.

mod drains;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, MouseEventKind};

use crate::app::mode::{Mode, WarmStatus};
use crate::app::state::AppState;
use crate::controller;
use crate::service::{openrouter::OpenRouterClient, StreamEvent, WarmEvent};
use crate::view;

use super::actions::apply_action;
use super::stream::{advance_turn, finish_stream};
use super::Term;

use drains::{apply_compaction_result, enter_select, exit_select};

/// Minimum on-screen duration for the `/compact` animation. Cosmetic and short:
/// a fast compaction is held this long (via a deferred apply) so the spinner +
/// progress bar don't merely flash. Deliberately ~1s — long enough to read, not
/// long enough to feel like a stall.
pub(super) const MIN_COMPACT_ANIM: Duration = Duration::from_millis(1000);

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
                    // `harness_rx` channel (drained below), and the per-model
                    // provider endpoints on `endpoints_rx` (drained below) — never
                    // on a streaming request's channel. So these arms are
                    // unreachable here; ignore them to keep the match exhaustive
                    // without affecting the stream.
                    StreamEvent::HarnessVerdict { .. }
                    | StreamEvent::EndpointsLoaded { .. }
                    | StreamEvent::EndpointsError { .. } => {}
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

        // 1b-2. Drain the per-model provider-endpoints channel. Fully independent
        //       of streaming and the harness channel: the background fetch sends
        //       exactly one EndpointsLoaded / EndpointsError, which is folded into
        //       the open model modal — but ONLY when its `model_id` still matches
        //       the modal's `endpoints_for` (the stale-guard, so a rapid
        //       re-selection can't show a previous model's providers). Take() the
        //       receiver so the match can mutate the mode; put it back unless the
        //       fetch resolved (or the channel closed).
        if let Some(mut erx) = state.rest.endpoints_rx.take() {
            let mut keep = true;
            while let Ok(ev) = erx.try_recv() {
                match ev {
                    StreamEvent::EndpointsLoaded { model_id, endpoints } => {
                        if let Mode::Settings(s) = &mut state.mode {
                            if let Some(m) = s.model_modal.as_mut() {
                                if m.endpoints_for.as_deref() == Some(model_id.as_str()) {
                                    m.endpoints = Some(endpoints);
                                    m.endpoints_loading = false;
                                }
                            }
                        }
                        dirty = true;
                        keep = false;
                    }
                    StreamEvent::EndpointsError { model_id, .. } => {
                        if let Mode::Settings(s) = &mut state.mode {
                            if let Some(m) = s.model_modal.as_mut() {
                                if m.endpoints_for.as_deref() == Some(model_id.as_str()) {
                                    // Empty list => "no providers found" display.
                                    m.endpoints = Some(Vec::new());
                                    m.endpoints_loading = false;
                                }
                            }
                        }
                        dirty = true;
                        keep = false;
                    }
                    _ => {}
                }
            }
            if keep {
                state.rest.endpoints_rx = Some(erx);
            }
        }

        // 1b-3. Drain the startup-warming channel. Fully independent of streaming:
        //        the background catalogue + awareness tasks each send one
        //        [`WarmEvent`]. ALWAYS fold the result into `state.rest.*` (the
        //        cache / summary) regardless of the current mode — a result that
        //        lands AFTER an Esc-to-chat must still populate them — and update
        //        the live `LoadingState` step marker only while still in
        //        `Mode::Loading`. Take() the receiver so the arms can mutate the
        //        mode + rest; put it back unless the channel has closed (both warm
        //        tasks finished and dropped their senders → `Disconnected`).
        if let Some(mut wrx) = state.rest.warm_rx.take() {
            let mut keep = true;
            loop {
                match wrx.try_recv() {
                    Ok(WarmEvent::WarmCatalogue { endpoint, models }) => {
                        // Key the on-demand cache to the endpoint it was fetched
                        // for; the omnisearch filters locally only while
                        // `models_cache_endpoint` matches the active endpoint.
                        state.rest.models_cache = Some(models);
                        state.rest.models_cache_endpoint = Some(endpoint.clone());
                        // Clear the in-flight guard for this endpoint so a later
                        // endpoint change can fetch again.
                        if state.rest.catalogue_fetching.as_deref() == Some(endpoint.as_str()) {
                            state.rest.catalogue_fetching = None;
                        }
                        dirty = true;
                    }
                    Ok(WarmEvent::WarmCatalogueFailed { endpoint }) => {
                        // TERMINAL empty result for this endpoint: record an empty
                        // catalogue keyed to it so the omnisearch degrades to manual
                        // model-id entry and does NOT retry in a loop (the
                        // request_catalogue no-op guard sees a matching endpoint).
                        state.rest.models_cache = Some(Vec::new());
                        state.rest.models_cache_endpoint = Some(endpoint.clone());
                        if state.rest.catalogue_fetching.as_deref() == Some(endpoint.as_str()) {
                            state.rest.catalogue_fetching = None;
                        }
                        dirty = true;
                    }
                    Ok(WarmEvent::WarmAwareness(summary)) => {
                        let had = summary.is_some();
                        // Always populate the summary (appended to the system
                        // message on every request), even if we've already skipped
                        // to chat.
                        state.rest.awareness_summary = summary;
                        if let Mode::Loading(s) = &mut state.mode {
                            // Some → ready; None → "no docs" (treated as a benign
                            // terminal Done detail, not a hard failure).
                            s.awareness = if had {
                                WarmStatus::Done("ready".into())
                            } else {
                                WarmStatus::Done("no docs".into())
                            };
                        }
                        dirty = true;
                    }
                    // Channel drained for now: keep listening on later ticks.
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    // Both warm tasks finished and dropped their senders: done.
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        keep = false;
                        break;
                    }
                }
            }
            if keep {
                state.rest.warm_rx = Some(wrx);
            }
        }

        // 1b-3b. Fire a DEBOUNCED, on-demand model-catalogue fetch. The model
        //        omnisearch arms `catalogue_pending` (via `request_catalogue`) on
        //        each keystroke / provider change, pushing `due` ~300ms forward so a
        //        typing burst collapses into one request. Fire here — where `handle`
        //        + `client` are in scope — once `due` passes and nothing is already
        //        in flight. Reuse the shared `warm_rx` channel (no new channel): the
        //        drain above folds the result into the per-endpoint cache. On
        //        failure send `WarmCatalogueFailed { endpoint }` so the drain records
        //        a terminal empty result (no infinite re-fetch on a dead endpoint).
        if let Some(pending) = state.rest.catalogue_pending.as_ref() {
            if state.rest.catalogue_fetching.is_none()
                && std::time::Instant::now() >= pending.due
            {
                // Take the pending request and mark its endpoint in-flight.
                let pending = state.rest.catalogue_pending.take().unwrap();
                let endpoint = pending.endpoint;
                let api_key = pending.api_key;
                state.rest.catalogue_fetching = Some(endpoint.clone());
                // Open a fresh warm channel for this fetch and stash its receiver.
                // Senders aren't stored in state (only the receiver), so this is the
                // only way to obtain one. This is safe wrt the awareness warm task:
                // the omnisearch (the sole `request_catalogue` caller) only runs in
                // Chat-mode modals / the first-run wizard, by which point the startup
                // awareness task has already resolved + closed its channel — so no
                // live awareness send can be stranded on a replaced receiver.
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                state.rest.warm_rx = Some(rx);
                // Reuse the pinned client, or build a keyless one (the first-run
                // wizard fetches before any client is pinned — `Conn` carries the
                // endpoint+key, so a keyless client is enough). The fetch is just
                // `GET {endpoint}/models`; on error send WarmCatalogueFailed so the
                // drain records a terminal empty result (no infinite re-fetch).
                let c = match client.as_ref() {
                    Some(c) => Arc::clone(c),
                    None => super::build_client(),
                };
                handle.spawn(async move {
                    let conn = crate::service::openrouter::Conn {
                        endpoint: &endpoint,
                        api_key: &api_key,
                    };
                    let ev = match c.list_models(conn).await {
                        Ok(models) => WarmEvent::WarmCatalogue { endpoint, models },
                        Err(_) => WarmEvent::WarmCatalogueFailed { endpoint },
                    };
                    // A dropped receiver (app closing) makes this a no-op.
                    let _ = tx.send(ev);
                });
                dirty = true;
            }
        }

        // 1b-3c. Drain each sub-agent's event channel. Sub-agents live in
        //        `state.rest.subagents`; each has its own `rx`. We iterate by index so
        //        we can reborrow mutably after collecting events into a local vec (borrow
        //        checker: the collect loop holds &mut subagents[i].rx; the apply loop
        //        needs &mut subagents[i].{status,transcript} and later &mut session).
        //        Pattern mirrors warm_rx: try_recv() in a loop, dirty=true on any event.
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
            // (call_id, result_text), accumulated across every sub-agent this tick
            // and applied after the loop. A sub-agent that reaches a terminal state
            // and still has its call id in `pending_subagent_calls` fills its result
            // here (the FULL report on Done, an error/killed note otherwise) so the
            // parked round can resume with no dangling tool_call ids.
            let mut deferred_results: Vec<(String, String)> = Vec::new();

            for i in 0..state.rest.subagents.len() {
                // --- collect phase: drain rx into a local vec ---
                let mut disconnected = false;
                let events: Vec<AgentEvent> = {
                    let sa = &mut state.rest.subagents[i];
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
                    let sa = &mut state.rest.subagents[i];
                    if matches!(sa.status, SubAgentStatus::Running) {
                        sa.status = SubAgentStatus::Killed;
                        dirty = true;
                    }
                }

                // --- apply phase: fold events onto the sub-agent ---
                // done_fold carries (id, agent_name, result) for the /task chat fold
                // below; only set on a Done event for a `/task` sub-agent
                // (tool_call_id == None). The task-tool path delivers its result via
                // `deferred_results` (computed from the settled status below), not here.
                if !events.is_empty() {
                    dirty = true;
                    let sa = &mut state.rest.subagents[i];
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
                                // Replace the structured history wholesale; drives
                                // the full-screen sub-agent viewer.
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
                // released before any `state.rest.session` mutation below. Runs every
                // tick (even when no events arrived, so a disconnect-only Killed is
                // still delivered). The "still in pending_subagent_calls" guard makes a
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
                    let sa = &state.rest.subagents[i];
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
                            if state.rest.pending_subagent_calls.contains(call_id) =>
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
                    // assistant turn so the main session retains a complete record.
                    if let Some(sess) = state.rest.session.as_mut() {
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
                // skipped when no usage was ever reported (provider omits it).
                if let Some((sub_model_id, sub_ti, sub_to, sub_cost)) = sub_usage {
                    // Merge into the session counters: cost and tokens_out are
                    // cumulative (summed); tokens_in is the main-context gauge and
                    // must NOT be touched (adding sub-agent prompt size would corrupt
                    // the context-window display).
                    state.rest.cost += sub_cost;
                    state.rest.tokens_out += sub_to;
                    // Record one ledger row per sub-agent completion (best-effort).
                    let (sess_uuid, pwd_hash) = state
                        .rest
                        .session
                        .as_ref()
                        .map(|s| (s.id.clone(), s.pwd_hash.clone()))
                        .unwrap_or_default();
                    let sa_name = state.rest.subagents[i].agent_name.clone();
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
            // `tool_results` and drop its id from `pending_subagent_calls`. Done
            // AFTER the loop so the per-agent borrow above stays immutable.
            for (call_id, result) in deferred_results {
                state.rest.pending_subagent_calls.retain(|c| c != &call_id);
                state.rest.tool_results.push((call_id, result));
                dirty = true;
            }

            // --- keep terminated sub-agents as session history ---
            // Terminated agents (Done, Error, Killed) are NOT pruned: the $ panel
            // is a session history, so every sub-agent that ran stays in the list
            // with its final status + structured `messages` for later viewing.
            // `running_subagents()` still counts only `Running`, so the cap is
            // unaffected. The list only ever grows here, so `subagent_sel` (always
            // < len once set) can never fall out of range — no clamp needed.

            // --- start queued delegations into any freed slots ---
            // A terminal handle above may have freed a slot. Start as many pending
            // sub-agents (FRONT-first) as now fit. Done BEFORE the resume gate
            // below: a queued task-tool delegation keeps its call id in
            // `pending_subagent_calls` across the queued→running transition, and an
            // unstartable entry delivers its error result + drops its id HERE — so
            // the `pending_subagent_calls.is_empty()` test sees the settled set and
            // can't resume a round that still has a queued delegation outstanding.
            if !state.rest.pending_subagents.is_empty() {
                super::stream::try_start_pending(state, client, handle);
                dirty = true;
            }

            // --- drain deferred tool-task results (heavy/blocking tools) ---
            // Deferred tools (read/write/edit/delete/bash/grep/glob/remember/
            // web_fetch/web_search) run on a plain std::thread (spawned in
            // `dispatch_deferred`) and send their `(call_id, result)` back over
            // `tool_task_rx`. Fold each into the PARKED round's `tool_results` and
            // drop its id from `pending_tool_tasks`, exactly mirroring the sub-agent
            // deferral — so the resume gate below sees the settled set. Done within
            // this same block (before the gate) so both lanes' results are in place
            // when emptiness is tested. A round runs its deferred tools ONE AT A
            // TIME, so at most one id settles here per resume.
            if let Some(rx) = state.rest.tool_task_rx.as_mut() {
                // Drain into a local vec first to release the rx borrow before
                // touching pending_tool_tasks / tool_results on state.rest.
                let mut received: Vec<(String, String)> = Vec::new();
                while let Ok(pair) = rx.try_recv() {
                    received.push(pair);
                }
                // Fold only results whose id is still in pending_tool_tasks;
                // anything else is a stale delivery from a killed/interrupted
                // turn and must be discarded rather than corrupting the next turn.
                for (id, result) in received {
                    if let Some(pos) = state.rest.pending_tool_tasks.iter().position(|c| c == &id) {
                        state.rest.pending_tool_tasks.remove(pos);
                        state.rest.tool_results.push((id, result));
                        dirty = true;
                    }
                    // else: stale delivery — drop silently
                }
            }

            // --- resume a round parked on deferred work (BOTH lanes) ---
            // Unpark only when EVERY deferred id — sub-agent delegations AND
            // deferred tool tasks — has filled its result (above). The resume
            // (`resume_after_subagents`) RE-ENTERS `process_tools` at the advanced
            // `tool_idx` to CONTINUE the round: a deferred heavy tool dispatched the
            // NEXT call (and may park again), making the lane SEQUENTIAL; once the
            // round has no further deferred work it falls through to
            // `finish_tool_round`, which flushes ALL collected `tool_results` and
            // re-streams so the MAIN AGENT reacts. Clearing both awaiting flags drops
            // the parked status; `waiting` stays true through the re-stream. Gating
            // on both lists means a mixed round waits for the last pending id of
            // either kind before resuming — no dangling tool_call ids.
            if (state.rest.awaiting_subagents || state.rest.awaiting_tool_tasks)
                && state.rest.pending_subagent_calls.is_empty()
                && state.rest.pending_tool_tasks.is_empty()
            {
                state.rest.awaiting_subagents = false;
                state.rest.awaiting_tool_tasks = false;
                super::stream::resume_after_subagents(state, client, handle);
                dirty = true;
            }
        }

        // 1b-3d. Drain the clipboard-image fetch result (Ctrl+V). The background
        //        thread sends Ok(bytes) (PNG data) or Err(reason) (tool absent / no image).
        //        On Ok: ingest into the session images dir + insert marker. On Err: toast.
        //        One send per Ctrl+V; clear the receiver once drained.
        if let Some(rx) = state.rest.clipboard_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(bytes)) => {
                    // Ingest the bytes; basename "pasted.png" + explicit png mime.
                    let attached = state.rest.try_attach_image_bytes(bytes, "image/png", "pasted.png");
                    if attached {
                        state.rest.set_toast_info("image attached from clipboard".to_string());
                    } else {
                        state.rest.set_toast("clipboard image: no active session or ingest failed".to_string());
                    }
                    state.rest.clipboard_rx = None;
                    dirty = true;
                }
                Ok(Err(reason)) => {
                    state.rest.set_toast(format!("clipboard image: {reason}"));
                    state.rest.clipboard_rx = None;
                    dirty = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still waiting — keep the receiver for the next tick.
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Thread exited without sending (shouldn't happen, but clean up).
                    state.rest.clipboard_rx = None;
                    dirty = true;
                }
            }
        }

        // 1b-4. Loading splash: workspace step, transition, and animation. While in
        //        `Mode::Loading` the splash is driven entirely from the loop tick.
        if let Mode::Loading(s) = &mut state.mode {
            // Workspace step: mark Done once the background reindex has SETTLED
            // (indexing flag cleared). Poll the cache readiness each tick; this
            // never gates the transition (a slow reindex must not hold up chat).
            if matches!(s.workspace, WarmStatus::Running) {
                let settled = state
                    .rest
                    .dir_cache
                    .read()
                    .map(|c| !c.indexing)
                    .unwrap_or(false);
                if settled {
                    s.workspace = WarmStatus::Done(String::new());
                }
            }
            // TRANSITION: once the catalogue + awareness steps are both terminal
            // (Done / Skipped / Failed) switch into Chat. The session/chat state was
            // already set up by the activation path; we only swap the mode. The
            // workspace step is intentionally excluded from this gate.
            if s.ready_to_enter() {
                state.mode = Mode::Chat;
                dirty = true;
            } else {
                // ANIMATION: still loading — advance the spinner and force a redraw
                // each tick so the braille frames actually cycle. Paired with the
                // fast (8ms) poll cadence below (which also wakes on `Mode::Loading`)
                // so the loop never idle-sleeps the spinner.
                s.frame = s.frame.wrapping_add(1);
                dirty = true;
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
        // Similarly, while any sub-agent is running (background `/task` agents
        // that don't set `waiting`), force redraws so the in-chat spinner animates.
        let has_running_subagents = state
            .rest
            .subagents
            .iter()
            .any(|s| matches!(s.status, crate::app::subagent::SubAgentStatus::Running));
        if state.rest.compact_anim_start.is_some() || shimmer_active || has_running_subagents {
            dirty = true;
        }

        // 2. Input poll cadence. While WORKING (waiting), poll fast so two things
        //    stay smooth: tokens flush at >=60fps when a stream is live, and the
        //    comet redraws at ~12fps (80ms) even when nothing streams (the 8ms
        //    poll is the upper bound on the redraw interval the comet needs). Idle
        //    falls back to 100ms (poll still wakes instantly on a keypress, so
        //    typing latency is 0) so a fully idle UI never busy-spins. Drain EVERY
        //    buffered event each tick so paste / fast typing don't lag.
        // Also poll fast while the loading splash is up so its braille spinner
        // animates smoothly (the per-tick `frame`++ above needs the loop to wake
        // at the fast cadence, not idle-sleep for 100ms between frames). And while a
        // debounced catalogue fetch is pending, so its ~300ms `due` fires promptly
        // rather than waiting out a 100ms idle sleep (treat it like the splash).
        let timeout = if state.rest.waiting
            || state.rest.catalogue_pending.is_some()
            || matches!(state.mode, Mode::Loading(_))
            || has_running_subagents
        {
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
