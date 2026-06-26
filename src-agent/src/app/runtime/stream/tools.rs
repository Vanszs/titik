//! Tool-approval state machine: classify, run, deny, finish tool rounds.

use std::sync::Arc;

use crate::app::state::AppState;
use crate::dto::chat::{Role, ToolCall};
use crate::service::openrouter::OpenRouterClient;
use crate::app::state::AgentMode;

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
) -> Option<(
    Arc<OpenRouterClient>,
    crate::model::app_config::AppConfig,
    crate::model::settings::Settings,
)> {
    match (client.as_ref(), state.rest.session.as_ref()) {
        (Some(c), Some(sess)) if sess.settings.classifier_enabled => Some((
            Arc::clone(c),
            state.rest.config.clone(),
            sess.settings.clone(),
        )),
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
pub(crate) fn process_tools(
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
        // Intercept the model-callable `task` tool BEFORE the generic
        // classify/dispatch path: spawn a background sub-agent (never classify it
        // as risky, never await it inline). UNLIKE the generic path, a SUCCESSFUL
        // spawn does NOT push a tool result here — instead it DEFERS, recording the
        // call id in `pending_subagent_calls` so the round parks (below) and the
        // event-loop drain delivers the sub-agent's FULL report as the tool result
        // once it finishes. The main agent then reacts to the real report rather
        // than a fire-and-forget "started" line. A parse error / unknown agent
        // spawns nothing, so it still pushes an IMMEDIATE error result for that call
        // id (keeping the conversation API-valid). Either way `tool_idx` advances so
        // the remaining calls in the round still process.
        if call.function.name == "task" {
            let sanitized =
                crate::dto::chat::sanitize_tool_arguments(&call.function.arguments);
            let args: serde_json::Value =
                serde_json::from_str(&sanitized).unwrap_or_else(|_| serde_json::json!({}));
            let agent = args.get("agent").and_then(|v| v.as_str()).unwrap_or("").trim();
            let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or("").trim();
            if agent.is_empty() || prompt.is_empty() {
                state.rest.tool_results.push((
                    call.id.clone(),
                    "error: task requires non-empty 'agent' and 'prompt'".to_string(),
                ));
            } else if super::spawn::running_subagents(state) >= crate::app::subagent::MAX_SUBAGENTS {
                // Concurrency cap hit: do NOT spawn. Answer the call now with an
                // error so the conversation stays API-valid and the main agent sees
                // it must wait for a running sub-agent to finish before delegating
                // again (rather than parking forever on an un-spawned delegation).
                state.rest.tool_results.push((
                    call.id.clone(),
                    "error: sub-agent limit reached (5 running). Wait for one to finish before delegating again.".to_string(),
                ));
            } else {
                let agent = agent.to_string();
                let prompt = prompt.to_string();
                match super::spawn::spawn_task(state, client, handle, &agent, &prompt, Some(call.id.clone())) {
                    // Spawned: DEFER the result. The drain fills it on terminal.
                    Some(_) => state.rest.pending_subagent_calls.push(call.id.clone()),
                    // Nothing spawned → answer the call now so it isn't left dangling.
                    None => state
                        .rest
                        .tool_results
                        .push((call.id.clone(), format!("error: unknown agent '{agent}'"))),
                }
            }
            state.rest.tool_idx += 1;
            continue;
        }
        if tool_is_risky(&call.function.name) {
            match tac_inputs(state, client) {
                // Classifier enabled → run TAC in both modes and act on its verdict.
                Some((c, config, settings)) => {
                    let verdict = handle.block_on(crate::app::harness::classify_toolcall(
                        &c,
                        &config,
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
        // Phase label for the comet: name the tool being executed so the
        // shimmering status surfaces what the agent is doing this round.
        state.rest.status = format!("running {}", call.function.name);
        let result = run_tool(state, &call);
        state.rest.tool_results.push((call.id.clone(), result));
        state.rest.tool_idx += 1;
    }
    // PARK on deferred `task`-tool delegations. If any sub-agent spawned this round
    // is still awaiting its result, DON'T finish the round yet — the conversation
    // would have dangling tool_call ids. Mark the round parked and return; the
    // event-loop sub-agent drain fills each pending result into `tool_results` as
    // its sub-agent terminates, and once `pending_subagent_calls` empties it calls
    // `resume_after_subagents` (which runs `finish_tool_round`). `waiting` stays
    // true and `awaiting_approval` stays false, so the comet keeps shimmering.
    if !state.rest.pending_subagent_calls.is_empty() {
        state.rest.awaiting_subagents = true;
        let n = state.rest.pending_subagent_calls.len();
        state.rest.status = if n == 1 {
            "delegating… (1 sub-agent)".into()
        } else {
            format!("delegating… ({n} sub-agents)")
        };
        return;
    }
    finish_tool_round(state, client, handle);
}

/// Resume a tool round that was PARKED on deferred `task`-tool delegations.
///
/// Called from the event-loop sub-agent drain once every id in
/// `pending_subagent_calls` has had its result delivered into `tool_results`. By
/// then `tool_results` holds one entry per call in the round (non-task calls were
/// answered inline in [`process_tools`]; task calls were filled by the drain), so
/// finishing the round flushes them all into the conversation and re-streams —
/// the main agent now sees every delegated report as a tool result and reacts.
pub(crate) fn resume_after_subagents(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) {
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

    // Live reload: if the `remember` tool ran this round, re-inject the updated
    // MEMORY.md into messages[0] so the model sees the new fact immediately.
    let remember_ran = state
        .rest
        .pending_tool_calls
        .iter()
        .any(|c| c.function.name == "remember");
    if remember_ran {
        if let Some(sess) = state.rest.session.as_mut() {
            sess.rebuild_system();
        }
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
    // The tool round is done; this re-stream is a model wait, so label it the same
    // "thinking" phase the comet sweeps (not a tool run).
    state.rest.status = "thinking".into();
    state.rest.begin_stream();
    super::run::start_stream_task(history, state, client, handle);
}

/// Run a single tool call against the session workspace and return its result
/// string (an `error: …` line on failure / unknown tool). Reads the session for
/// the workspace path and clones the shared dir cache up front, then dispatches
/// to the matching [`crate::tool::Tool`].
///
/// `pub(crate)` so the approve/deny action handlers can run a single tool when
/// resuming the approval machine.
pub(crate) fn run_tool(state: &mut AppState, call: &ToolCall) -> String {
    let ctx = super::spawn::build_tool_ctx(state);
    crate::tool::execute_tool(&ctx, call)
}

/// Halt the current turn by answering every still-pending tool call with
/// `reason` (and flushing any results already collected this round), so the
/// stored conversation keeps every `tool_call` id answered — then reset the
/// agentic-loop machine and end the turn WITHOUT re-streaming.
///
/// Shares the shape of [`super::actions`]'s `DenyTool` handler; used by the
/// harness workspace check (WC) to refuse a turn whose workspace isn't allowed.
/// Pending calls from `tool_idx` onward are the unanswered ones.
pub(crate) fn deny_all_pending(state: &mut AppState, reason: &str) {
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
