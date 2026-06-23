//! Async streaming bridge: spawn / abort / finalize a request task.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::app::state::{AgentMode, AppState, AppStateRest};
use crate::dto::chat::{ChatMessage, Role, ToolCall};
use crate::service::openrouter::OpenRouterClient;

/// Hard cap on tool-call rounds within a single user turn. Once exceeded the
/// turn is stopped so a misbehaving model can't loop indefinitely.
const MAX_AGENT_STEPS: usize = 40;


/// Finalize a finished stream: commit any buffered assistant text, clear the
/// waiting flag + task handle, set the status line. `error` is Some on stream
/// failure; a save error is surfaced only if the stream itself succeeded.
pub(super) fn finish_stream(rest: &mut AppStateRest, error: Option<String>) {
    // Take the in-flight usage unconditionally so it can never leak into the
    // next turn, even when the buffer is empty or there's no session to commit.
    let usage = rest.pending_usage.take();
    let mut save_err = None;
    if let Some(buf) = rest.take_stream() {
        if !buf.is_empty() {
            if let Some(sess) = rest.session.as_mut() {
                let _ = crate::model::msglog::append(
                    &sess.path,
                    crate::dto::chat::Role::Assistant,
                    &buf,
                    usage,
                );
                sess.conversation.push_assistant(buf);
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
                    .push_assistant_with_tools(content, pending.clone());
                if let Err(e) = sess.save() {
                    save_err = Some(e.to_string());
                }
            } else if let Some(b) = buf.as_ref() {
                if !b.is_empty() {
                    let _ = crate::model::msglog::append(&sess.path, Role::Assistant, b, usage);
                    sess.conversation.push_assistant(b.clone());
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

    // 5a. Plan gate: on the FIRST tool round of a new user turn, make sure the
    //     model stated a plan before tools run. The System message already steers
    //     it to lead with a whimsical word (see `stream_complete`), so it usually
    //     plans on its own — in which case we just run the tools (no duplicate
    //     plan). Only when it jumped straight to tools with NO text do we answer
    //     each call with a silent nudge and re-stream; that nudge carries the
    //     hide-marker so it's fed to the model but never shown in the transcript.
    //     Every pending call gets a result, so there are no dangling tool_call IDs.
    if state.rest.needs_plan {
        state.rest.needs_plan = false;
        let planned = buf.as_deref().is_some_and(|b| !b.trim().is_empty());
        if !planned {
            state.rest.tool_idx = 0;
            state.rest.tool_results.clear();
            let nudge = format!(
                "{}First state a brief plan: restate what the user wants in one line, then list your steps. Then make your tool calls (batch where possible — dir_list accepts multiple paths).",
                crate::dto::chat::PLAN_NUDGE_MARK
            );
            let ids: Vec<String> = state.rest.pending_tool_calls.iter().map(|c| c.id.clone()).collect();
            for id in ids {
                state.rest.tool_results.push((id, nudge.clone()));
            }
            finish_tool_round(state, client, handle);
            return;
        }
        // else: model already planned → fall through to the normal tool run below
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

/// Drive the tool-approval state machine for the current round.
///
/// Walks `pending_tool_calls` from `tool_idx`, running each call and collecting
/// its `(id, result)` into `tool_results`. In `Normal` mode it STOPS at the
/// first risky (write/delete) call, sets `awaiting_approval`, and returns — the
/// turn is resumed later by [`Action::ApproveTool`] / [`Action::DenyTool`]
/// (which run/deny that one call, advance `tool_idx`, and call back in here).
/// Once every call in the round has resolved it calls [`finish_tool_round`].
///
/// Each call/string is cloned out of `state.rest` before `run_tool` (which
/// borrows `state` mutably) so there's no overlapping borrow of the vec.
pub(super) fn process_tools(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) {
    let mode = state.rest.agent_mode;
    while state.rest.tool_idx < state.rest.pending_tool_calls.len() {
        let call = state.rest.pending_tool_calls[state.rest.tool_idx].clone();
        if mode == AgentMode::Normal && tool_is_risky(&call.function.name) {
            // Pause the turn for the user's decision; resumed by Approve/Deny.
            state.rest.awaiting_approval = true;
            state.rest.status = format!("approve {}? [y/n]", call.function.name);
            return;
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

/// Abort the in-flight task and stop listening to it: aborts the task handle,
/// drops the active receiver (so any late events from the task vanish), and
/// clears the waiting flag.
pub(super) fn abort_current(rest: &mut AppStateRest) {
    if let Some(h) = rest.current_task.take() {
        h.abort();
    }
    rest.active_rx = None;
    rest.waiting = false;
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
    // Self-awareness: tell the model the project's top-level layout every request
    // (so it's present after compaction too). Pulled from the live dir cache.
    if let Some(first) = history.first_mut() {
        if first.role == Role::System {
            if let Ok(cache) = state.rest.dir_cache.read() {
                let listing = cache.children(".");
                if !listing.is_empty() {
                    first.content.push_str("\n\n# Project files (top level)\n");
                    first.content.push_str(&listing.join("\n"));
                }
            }
        }
    }
    let (tx, rx) = mpsc::unbounded_channel();
    state.rest.active_rx = Some(rx);
    let c = Arc::clone(client.as_ref().unwrap());
    let jh = handle.spawn(async move {
        let _ = c.stream_complete(history, tx).await;
    });
    state.rest.current_task = Some(jh.abort_handle());
}
