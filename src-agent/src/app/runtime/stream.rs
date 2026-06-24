//! Async streaming bridge: spawn / abort / finalize a request task.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::app::state::{AgentMode, AppState, AppStateRest};
use crate::dto::chat::{ChatMessage, Role, ToolCall};
use crate::service::openrouter::OpenRouterClient;

/// Hard cap on tool-call rounds within a single user turn. Once exceeded the
/// turn is stopped so a misbehaving model can't loop indefinitely.
const MAX_AGENT_STEPS: usize = 40;


/// Pick the assistant message content + display-reasoning for a FINAL turn.
/// Normally content is the answer and `reasoning` rides along (rendered gray).
/// But when the model left content empty and streamed its answer into the
/// reasoning channel (e.g. deepseek-v4-flash with reasoning on), promote the
/// reasoning to BE the content so it shows in the foreground and persists.
/// Returns (content, reasoning_to_attach). Empty content with no reasoning -> ("", None).
fn final_answer(content: String, reasoning: Option<String>) -> (String, Option<String>) {
    if content.trim().is_empty() {
        match reasoning {
            Some(r) if !r.trim().is_empty() => (r, None), // reasoning becomes the answer
            _ => (String::new(), None),
        }
    } else {
        (content, reasoning) // normal: content is answer, reasoning rendered gray
    }
}

/// Finalize a finished stream: commit any buffered assistant text, clear the
/// waiting flag + task handle, set the status line. `error` is Some on stream
/// failure; a save error is surfaced only if the stream itself succeeded.
pub(super) fn finish_stream(rest: &mut AppStateRest, error: Option<String>) {
    // Take the in-flight usage unconditionally so it can never leak into the
    // next turn, even when the buffer is empty or there's no session to commit.
    let usage = rest.pending_usage.take();
    // Reasoning taken unconditionally so it can't leak; may be promoted to
    // content below when the model streamed its entire answer through that channel.
    let reasoning = rest.take_reasoning();
    let buf = rest.take_stream().unwrap_or_default();
    let (content, msg_reasoning) = final_answer(buf, reasoning);
    let mut save_err = None;
    if !content.is_empty() {
        if let Some(sess) = rest.session.as_mut() {
            let _ = crate::model::msglog::append(
                &sess.path,
                crate::dto::chat::Role::Assistant,
                &content,
                usage,
            );
            sess.conversation.push_assistant(content, msg_reasoning);
            if let Err(e) = sess.save() {
                save_err = Some(e.to_string());
            }
            // tokens_in = current context size (latest prompt), not cumulative.
            // tokens_out and cost are cumulative (each turn adds new spend).
            if let Some((pt, ct, cost)) = usage {
                rest.tokens_in = pt;        // current context size, not a sum
                rest.tokens_out += ct;
                rest.cost += cost;
            }
        }
    }
    rest.waiting = false;
    rest.current_task = None;
    match error.or(save_err) {
        Some(e) => {
            rest.set_toast(e.clone());
            rest.status = format!("error: {e}");
        }
        None => rest.status = "ready".into(),
    }
}

/// Advance a turn after a stream finished cleanly (`StreamEvent::Done`).
///
/// A single user turn may span several model calls when the model requests
/// tools. This commits the just-finished assistant message, then EITHER:
/// - ends the turn (no tool calls → the model gave its final answer), or
/// - runs the requested tools, appends their results, and starts the next
///   model call to continue the turn (`waiting` stays true throughout).
///
/// Mirrors the usage/counter bookkeeping of [`finish_stream`]: `tokens_in` is
/// the latest prompt size (current context), `tokens_out` / `cost` accumulate.
pub(super) fn advance_turn(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) {
    // 1. Take the stashed tool calls + the streamed text + the in-flight usage
    //    out of state up front so nothing leaks into the next model call.
    let pending = state.rest.pending_tool_calls.clone();
    let buf = state.rest.take_stream();
    let usage = state.rest.pending_usage.take();
    // Display-only reasoning streamed this round. Taken unconditionally (so it
    // can never leak into the next round) and folded onto the committed message
    // below; never logged to disk or sent to the API.
    let reasoning = state.rest.take_reasoning();

    // 2. Commit the assistant message (and log + count it). The assistant text
    //    may be empty on a tool-call turn — we still record the row so usage
    //    accounting stays correct across rounds.
    let mut save_err = None;
    {
        let rest = &mut state.rest;
        if let Some(sess) = rest.session.as_mut() {
            if !pending.is_empty() {
                let content = buf.clone().unwrap_or_default();
                let _ = crate::model::msglog::append(&sess.path, Role::Assistant, &content, usage);
                sess.conversation
                    .push_assistant_with_tools(content, pending.clone(), reasoning);
                if let Err(e) = sess.save() {
                    save_err = Some(e.to_string());
                }
            } else {
                let (content, msg_reasoning) =
                    final_answer(buf.clone().unwrap_or_default(), reasoning);
                if !content.is_empty() {
                    let _ = crate::model::msglog::append(&sess.path, Role::Assistant, &content, usage);
                    sess.conversation.push_assistant(content, msg_reasoning);
                    if let Err(e) = sess.save() {
                        save_err = Some(e.to_string());
                    }
                }
            }
            // Counter update: disjoint fields of `rest`, accessed after the
            // session push so the borrows don't overlap problematically.
            if let Some((pt, ct, cost)) = usage {
                rest.tokens_in = pt; // current context size, not a sum
                rest.tokens_out += ct;
                rest.cost += cost;
            }
        }
    }

    // 3. No tool calls → the model produced its final answer; the turn is done.
    if pending.is_empty() {
        state.rest.waiting = false;
        state.rest.current_task = None;
        state.rest.agent_steps = 0;
        state.rest.status = match save_err {
            Some(e) => {
                state.rest.set_toast(e.clone());
                format!("error: {e}")
            }
            None => "ready".into(),
        };
        return;
    }

    // 4. Step cap: stop the turn if the model keeps asking for tools forever.
    if state.rest.agent_steps >= MAX_AGENT_STEPS {
        if let Some(sess) = state.rest.session.as_mut() {
            let _ = sess.save();
        }
        state.rest.waiting = false;
        state.rest.current_task = None;
        state.rest.agent_steps = 0;
        state.rest.status = "stopped: max tool steps".into();
        return;
    }
    state.rest.agent_steps += 1;

    // 4b. Workspace check (WC): the deterministic harness gate. When the harness
    //     is enabled and the session workdir is NOT an allowed folder (the launch
    //     dir or an allow-list entry), refuse to run ANY tool this turn. Every
    //     pending call is answered with a refusal (so the conversation stays
    //     API-valid — no dangling tool_call ids) and the turn is stopped. When
    //     the harness is disabled this is skipped entirely (zero behaviour
    //     change). The check runs once per round, before the plan gate / tools.
    let wc_blocked = state
        .rest
        .session
        .as_ref()
        .is_some_and(|sess| {
            sess.settings.classifier_enabled
                && !crate::app::harness::workspace_allowed(
                    &sess.settings,
                    &sess.workdir(),
                    &state.rest.launch_dir,
                )
        });
    if wc_blocked {
        deny_all_pending(state, "workspace not in allowed folders");
        state.rest.set_toast("workspace not in allowed folders".into());
        state.rest.status = "stopped: workspace not allowed".into();
        return;
    }

    // 5b. Hand off to the tool-approval state machine. The pending calls were
    //     already stashed into `state.rest.pending_tool_calls` by the event loop
    //     (`StreamEvent::ToolCalls`); `process_tools` walks them from index 0,
    //     running safe calls inline and — in Normal mode — pausing on the first
    //     risky one for a `y/n`. `pending` (the local copy) is no longer needed.
    drop(pending);
    state.rest.tool_idx = 0;
    state.rest.tool_results.clear();
    process_tools(state, client, handle);
}

/// True for tools that mutate the workspace (or run arbitrary shell commands)
/// and therefore require approval in Normal mode. Deterministic, name-based —
/// no classifier / network call.
fn tool_is_risky(name: &str) -> bool {
    matches!(name, "write" | "delete" | "edit" | "bash")
}

/// Inputs for a tool-call-classifier (TAC) call, or `None` when TAC should not
/// run: the harness is disabled, or there's no client/session. `None` makes the
/// caller fall back to the ORIGINAL approval behaviour (Normal prompts a risky
/// call, Auto runs it) — the unchanged path when the harness is off. The
/// `Settings` and client `Arc` are cloned out so the caller's `block_on` doesn't
/// hold a borrow of `state`.
fn tac_inputs(
    state: &AppState,
    client: &Option<Arc<OpenRouterClient>>,
) -> Option<(Arc<OpenRouterClient>, crate::model::settings::Settings)> {
    match (client.as_ref(), state.rest.session.as_ref()) {
        (Some(c), Some(sess)) if sess.settings.classifier_enabled => {
            Some((Arc::clone(c), sess.settings.clone()))
        }
        _ => None,
    }
}

/// Drive the tool-approval state machine for the current round.
///
/// Walks `pending_tool_calls` from `tool_idx`, running each call and collecting
/// its `(id, result)` into `tool_results`. Non-risky calls always run inline. A
/// risky call (write/edit/delete/bash) is the decision point, and the policy
/// depends on whether the tool-call classifier (TAC) is enabled:
///
/// **Classifier enabled** ([`tac_inputs`] is `Some`) — TAC runs in BOTH modes,
/// intent-aware (it sees the last user message). Per verdict:
/// - available + allow → run the call inline (both modes).
/// - available + block → Auto records a `blocked by harness: <reason>` result
///   and continues the loop WITHOUT a prompt; Normal pauses for `y/n` with the
///   reason.
/// - unavailable (error/timeout) → BOTH modes pause for `y/n` ("classifier
///   unavailable"), degrading to a human decision rather than freezing.
///
/// **Classifier disabled** (`tac_inputs` is `None`) — original behaviour: Normal
/// pauses a risky call for `y/n`; Auto runs it inline.
///
/// A pause sets `awaiting_approval` and returns; the turn is resumed later by
/// [`Action::ApproveTool`] / [`Action::DenyTool`] (which run/deny that one call,
/// advance `tool_idx`, and call back in here). Once every call in the round has
/// resolved it calls [`finish_tool_round`].
///
/// Each call/string is cloned out of `state.rest` before `run_tool` (which
/// borrows `state` mutably) so there's no overlapping borrow of the vec. Reached
/// only from the sync loop, so the `block_on` TAC call is safe.
pub(super) fn process_tools(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) {
    let mode = state.rest.agent_mode;
    // The user's latest request, used to make TAC intent-aware. Cloned out once
    // (empty when there's no user message) so the per-call `block_on` below holds
    // no borrow of `state`. The most-recent User message is the real request even
    // after the assistant's tool-call + tool-result messages were pushed.
    let user_intent = state
        .rest
        .session
        .as_ref()
        .and_then(|sess| sess.conversation.last_user_content())
        .unwrap_or_default();
    while state.rest.tool_idx < state.rest.pending_tool_calls.len() {
        let call = state.rest.pending_tool_calls[state.rest.tool_idx].clone();
        if tool_is_risky(&call.function.name) {
            match tac_inputs(state, client) {
                // Classifier enabled → run TAC in both modes and act on its verdict.
                Some((c, settings)) => {
                    let verdict = handle.block_on(crate::app::harness::classify_toolcall(
                        &c,
                        &settings,
                        &user_intent,
                        &call.function.name,
                        &call.function.arguments,
                    ));
                    if verdict.available && verdict.allow {
                        // Definite allow. Auto runs it inline (no prompt — the user
                        // delegated decisions); Normal still asks, because in Normal
                        // mode the USER approves every risky op and the classifier
                        // only informs. The allowed reason is surfaced so the prompt
                        // shows the verdict was "ok".
                        if mode == AgentMode::Auto {
                            // Fall through and run it inline (no prompt).
                            state.rest.approval_reason = None;
                        } else {
                            state.rest.approval_reason =
                                Some(format!("classifier: ok — {}", verdict.reason));
                            state.rest.awaiting_approval = true;
                            state.rest.status =
                                format!("approve {}? [y/n]", call.function.name);
                            return;
                        }
                    } else if verdict.available {
                        // Definite block. Auto records it and continues; Normal asks.
                        if mode == AgentMode::Auto {
                            state.rest.tool_results.push((
                                call.id.clone(),
                                format!("blocked by harness: {}", verdict.reason),
                            ));
                            state.rest.tool_idx += 1;
                            continue;
                        }
                        state.rest.approval_reason = Some(verdict.reason);
                        state.rest.awaiting_approval = true;
                        state.rest.status = format!("approve {}? [y/n]", call.function.name);
                        return;
                    } else {
                        // Classifier unavailable. `verdict.reason` now carries the
                        // REAL cause (e.g. "classifier error: 402 …", "classifier
                        // timeout", "unparseable verdict: …") — surface it so the
                        // user sees the actual diagnostic, not a generic string.
                        // Normal: degrade to a human y/n prompt (human decides).
                        // Auto: fail-open — user has delegated decisions; a
                        //       classifier outage must not halt or interrupt them.
                        //       Run inline and surface a toast so the degradation
                        //       is visible.
                        if mode == AgentMode::Normal {
                            state.rest.approval_reason = Some(verdict.reason.clone());
                            state.rest.awaiting_approval = true;
                            state.rest.status =
                                format!("approve {}? [y/n]", call.function.name);
                            return;
                        }
                        // Auto + unavailable → run inline, no prompt.
                        state.rest.set_toast(format!(
                            "harness: {} — auto-ran {}",
                            verdict.reason, call.function.name
                        ));
                        // fall through to run_tool below
                    }
                }
                // Classifier disabled → original behaviour: Normal asks, Auto runs.
                None => {
                    if mode == AgentMode::Normal {
                        state.rest.awaiting_approval = true;
                        state.rest.status = format!("approve {}? [y/n]", call.function.name);
                        return;
                    }
                    // Auto + classifier disabled → fall through and run inline.
                }
            }
        }
        let result = run_tool(state, &call);
        state.rest.tool_results.push((call.id.clone(), result));
        state.rest.tool_idx += 1;
    }
    finish_tool_round(state, client, handle);
}

/// Finish a completed tool round: flush every collected result into the
/// conversation + log, clear the machine, and re-stream so the model sees the
/// tool outputs and continues the turn (`waiting` stays true throughout).
///
/// Bails cleanly if there is no session or client to continue against
/// (defensive — a turn in flight normally implies both are present).
fn finish_tool_round(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) {
    // Push the collected tool results into the conversation + log them.
    if let Some(sess) = state.rest.session.as_mut() {
        for (id, result) in &state.rest.tool_results {
            let _ = crate::model::msglog::append(&sess.path, Role::Tool, result, None);
            sess.conversation.push_tool(id.clone(), result.clone());
        }
        let _ = sess.save();
    }
    // Round done: clear the per-round machine before the next model call.
    state.rest.pending_tool_calls.clear();
    state.rest.tool_idx = 0;
    state.rest.tool_results.clear();

    // Continue the turn: hand the updated history back to the model. The
    // streaming buffer is re-armed so the next assistant text accumulates
    // cleanly. `waiting` stays true (the turn isn't finished yet).
    let history = match (state.rest.session.as_ref(), client.as_ref()) {
        (Some(sess), Some(_)) => sess.conversation.history(),
        _ => {
            state.rest.waiting = false;
            state.rest.current_task = None;
            state.rest.agent_steps = 0;
            state.rest.status = "no active session".into();
            return;
        }
    };
    state.rest.status = "running tools…".into();
    state.rest.begin_stream();
    start_stream_task(history, state, client, handle);
}

/// Run a single tool call against the session workspace and return its result
/// string (an `error: …` line on failure / unknown tool). Reads the session for
/// the workspace path and clones the shared dir cache up front, then dispatches
/// to the matching [`crate::tool::Tool`].
///
/// `pub(super)` so the approve/deny action handlers can run a single tool when
/// resuming the approval machine.
pub(super) fn run_tool(state: &mut AppState, call: &ToolCall) -> String {
    let workspace = state
        .rest
        .session
        .as_ref()
        .map(|s| s.workdir())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let ctx = crate::tool::ToolCtx {
        workspace,
        dir_cache: state.rest.dir_cache.clone(),
    };
    // OpenAI/OpenRouter send `arguments` as a JSON-encoded string; an empty or
    // malformed payload degrades to `{}` so the tool sees no arguments.
    let args: serde_json::Value =
        serde_json::from_str(&call.function.arguments).unwrap_or_else(|_| serde_json::json!({}));
    for tool in crate::tool::all_tools() {
        if tool.name() == call.function.name {
            return match tool.run(&ctx, &args) {
                Ok(s) => s,
                Err(e) => format!("error: {e}"),
            };
        }
    }
    format!("error: unknown tool '{}'", call.function.name)
}

/// Halt the current turn by answering every still-pending tool call with
/// `reason` (and flushing any results already collected this round), so the
/// stored conversation keeps every `tool_call` id answered — then reset the
/// agentic-loop machine and end the turn WITHOUT re-streaming.
///
/// Shares the shape of [`super::actions`]'s `DenyTool` handler; used by the
/// harness workspace check (WC) to refuse a turn whose workspace isn't allowed.
/// Pending calls from `tool_idx` onward are the unanswered ones.
pub(super) fn deny_all_pending(state: &mut AppState, reason: &str) {
    let results = state.rest.tool_results.clone();
    let pending_ids: Vec<String> = state
        .rest
        .pending_tool_calls
        .iter()
        .skip(state.rest.tool_idx)
        .map(|c| c.id.clone())
        .collect();
    if let Some(sess) = state.rest.session.as_mut() {
        for (id, result) in &results {
            let _ = crate::model::msglog::append(&sess.path, Role::Tool, result, None);
            sess.conversation.push_tool(id.clone(), result.clone());
        }
        for id in &pending_ids {
            let _ = crate::model::msglog::append(&sess.path, Role::Tool, reason, None);
            sess.conversation.push_tool(id.clone(), reason.to_string());
        }
        let _ = sess.save();
    }
    state.rest.pending_tool_calls.clear();
    state.rest.tool_idx = 0;
    state.rest.tool_results.clear();
    state.rest.agent_steps = 0;
    state.rest.awaiting_approval = false;
    state.rest.approval_reason = None;
    state.rest.waiting = false;
    state.rest.current_task = None;
}

/// Abort the in-flight task and stop listening to it: aborts the task handle,
/// drops the active receiver (so any late events from the task vanish), and
/// clears the waiting flag.
pub(super) fn abort_current(rest: &mut AppStateRest) {
    if let Some(h) = rest.current_task.take() {
        h.abort();
    }
    rest.active_rx = None;
    rest.waiting = false;
    // Tear down any in-flight compaction animation / deferred apply so an
    // interrupt (Esc) or `/new` mid-compact doesn't leave the spinner stuck (and
    // forcing a per-tick redraw) forever.
    rest.compact_anim_start = None;
    rest.compact_apply_at = None;
    rest.compact_pending = None;
}

/// Spawn a streaming task for `history`. Opens a fresh channel, stashes the
/// receiver in state, and hands the sender to the task — so this request's
/// events are isolated from any previous one (no generation tagging needed).
pub(super) fn start_stream_task(
    mut history: Vec<ChatMessage>,
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) {
    // Assemble the System message so the prompt-caching breakpoint covers only the
    // STABLE head (which is byte-identical across the session, so the cache hits):
    //
    //   [ stable base system prompt (already in history[0]) ]
    // + [ plan-word steer (same word every request, chosen once per client) ]
    // + CACHE_SPLIT_MARK                                     <- cache breakpoint here
    // + [ "\n\n# Project files (top level)" listing ]       (volatile: changes with files)
    // + [ "\n\n# Project summary" awareness block ]          (volatile: project-dependent)
    //
    // The plan-word steer + the mark go in FIRST, before the volatile tail, so the
    // head ends at the mark and the listing/awareness land after it. `to_wire`
    // splits on the mark and attaches `cache_control` to the head only; the tail
    // rides as a second, uncached content part. Injecting here (BEFORE `to_wire`)
    // keeps the steer inside the cached block. The tail may be empty (no listing /
    // no summary) — `to_wire` handles that by emitting a single cached part.
    if let Some(first) = history.first_mut() {
        if first.role == Role::System {
            // Plan-word steer: lead the FIRST plan with the session's whimsical
            // word instead of "Plan:". `plan_word` is chosen once per client, so
            // the SAME word every request keeps the cached head byte-stable.
            if let Some(c) = client.as_ref() {
                let word = c.plan_word();
                first.content.push_str(&format!(
                    "\n\nWhen you write your plan for this task, lead with the single word \"{word}:\" (a whimsical lead-in) instead of \"Plan:\"."
                ));
            }
            // Boundary between the stable cached head (everything above) and the
            // volatile tail (everything below). Inserted unconditionally so the
            // split point always exists, even when the tail ends up empty.
            first.content.push_str(crate::dto::chat::CACHE_SPLIT_MARK);
            // Volatile tail begins here — project layout + awareness summary. Sent
            // every request (so they survive compaction too) but kept AFTER the
            // cache breakpoint so file changes never bust the cached prefix.
            if let Ok(cache) = state.rest.dir_cache.read() {
                let listing = cache.children(".");
                if !listing.is_empty() {
                    first.content.push_str("\n\n# Project files (top level)\n");
                    first.content.push_str(&listing.join("\n"));
                }
            }
            if let Some(summary) = state.rest.awareness_summary.as_deref() {
                if !summary.is_empty() {
                    first.content.push_str("\n\n# Project summary\n");
                    first.content.push_str(summary);
                }
            }
        }
    }
    // Short-send reshape inputs, snapshotted out of `state` BEFORE the spawn so
    // the task holds no borrow of `state`. Cloning the session dir + settings +
    // latest user message lets `shortsend::shape` run its fold/router off the UI
    // thread (the task already shows the "waiting" state, so the UI never freezes
    // on these secondary-model calls). `None` when there's no session — the task
    // then sends the injected history unchanged.
    //
    // DUAL RAIL: `shape` only transforms this API-bound `history` Vec (built from
    // `sess.conversation.history()` by the caller). It reads `messages.sqlite` and
    // returns a NEW Vec; it does not touch `sess.conversation`, `messages.json`,
    // or the rendered transcript — display is entirely unaffected.
    //
    // The OLD per-send "is the history near the window?" gate moves HERE (out of
    // shape) so it can read the live cache-warmth + sticky engage state, which only
    // exists on `state`. We compute the engage decision (a bool) + the token budget
    // (`usable`) into locals FIRST — all the `state.rest` reads happen up front so
    // they don't borrow-conflict with the per-session snapshot or the two writes
    // below. Everything here is a no-op (`summarizing` stays false, the task sends
    // the history unchanged) when there's no active session.
    //
    // The per-session snapshot the reshape task needs: (dir, settings, latest user
    // message). Cloned out of the session up front so the spawned task holds no
    // borrow of `state`, and so `settings` is available to size the window + read
    // `sliding_cache` below without re-borrowing the session.
    let reshape: Option<(std::path::PathBuf, crate::model::settings::Settings, String)> =
        state.rest.session.as_ref().map(|sess| {
            let user_intent = sess.conversation.last_user_content().unwrap_or_default();
            (sess.path.clone(), sess.settings.clone(), user_intent)
        });

    // 1. Window: the model's context-window size in tokens, from the cached
    //    catalogue. 128k is a safe fallback (the min-window policy is 100k+).
    let window = reshape
        .as_ref()
        .and_then(|(_, settings, _)| {
            state
                .rest
                .models_cache
                .as_deref()
                .and_then(|models| {
                    crate::service::openrouter::context_length_for(models, &settings.model)
                })
        })
        .unwrap_or(128_000);
    // 2. Usable budget: the window minus the fixed system/tools/memory overhead,
    //    floored so the percentages below never go degenerate on a tiny window.
    let usable = window
        .saturating_sub(super::shortsend::BASE_OVERHEAD)
        .max(8_000);
    // 3. Conversation size estimate (~4 chars/token over content + tool args).
    let conv_tokens = super::shortsend::estimate_conv_tokens(&history);
    // 4. Cache warmth: a warm cache (provider supports caching, the cache holds
    //    tokens, and the last send was recent enough that it hasn't gone cold)
    //    lets the conversation grow far larger before we summarize. The cold
    //    window is longer when the provider runs a sliding/refreshing cache.
    let sliding_cache = reshape
        .as_ref()
        .is_some_and(|(_, settings, _)| settings.sliding_cache);
    let gap = state.rest.last_send_at.map(|t| t.elapsed());
    let cold_window = if sliding_cache {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(120)
    };
    let cache_warm = state.rest.provider_caches
        && state.rest.tokens_cached > 0
        && gap.is_some_and(|g| g < cold_window);
    let engage_pct = if cache_warm {
        super::shortsend::ENGAGE_WARM_PCT
    } else {
        super::shortsend::ENGAGE_COLD_PCT
    };
    // 5. Sticky engage hysteresis: cross the (warmth-dependent) engage threshold to
    //    turn summarizing ON; only fall back below DISENGAGE_PCT to turn it OFF.
    //    The dead-zone between the two prevents flapping on/off each turn.
    let enter = conv_tokens > engage_pct * usable / 100;
    let exit = conv_tokens < super::shortsend::DISENGAGE_PCT * usable / 100;
    if !state.rest.summarizing && enter {
        state.rest.summarizing = true;
    } else if state.rest.summarizing && exit {
        state.rest.summarizing = false;
    }
    let summarizing = state.rest.summarizing;
    // 6. Stamp the send instant so the NEXT turn can measure cache warmth from the
    //    gap since this send.
    state.rest.last_send_at = Some(Instant::now());
    let (tx, rx) = mpsc::unbounded_channel();
    state.rest.active_rx = Some(rx);
    let c = Arc::clone(client.as_ref().unwrap());
    let jh = handle.spawn(async move {
        // Reshape the wire payload just before POSTing. `shape` preserves the
        // system message at index 0 (with the project-files/awareness injection
        // applied above, plus — when engaged — the condensed-history summary
        // appended to its uncached tail), so the model still receives the real
        // system prompt. It fails open — any error returns the original history —
        // so this can never break the send. `summarizing` is the upstream engage
        // decision; `usable` is the token budget the fold's band sizing uses.
        let history = match reshape {
            Some((session_dir, settings, user_intent)) => {
                super::shortsend::shape(
                    history,
                    &session_dir,
                    &c,
                    &settings,
                    &user_intent,
                    summarizing,
                    usable,
                )
                .await
            }
            None => history,
        };
        let _ = c.stream_complete(history, tx).await;
    });
    state.rest.current_task = Some(jh.abort_handle());
}
