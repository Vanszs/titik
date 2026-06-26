//! The autonomous sub-agent loop.
//!
//! [`run_agent_loop`] is a NON-INTERACTIVE condensation of the interactive
//! engine in `app::runtime::stream` (`advance_turn` + `process_tools` +
//! `finish_tool_round`): it streams a model reply, runs the requested tools, and
//! feeds the results back — looping until the model produces a final answer or
//! the step budget is exhausted. Unlike the interactive engine it owns no
//! `AppState`, never prompts a human, and reports progress purely as
//! [`AgentEvent`]s.
//!
//! ## Differences from the interactive loop (deliberate)
//!
//! - **Allow-list enforcement.** `stream_complete` advertises ONLY this agent's
//!   `tools` allow-list to the model, so the model sees just the tools it is
//!   permitted to call. The loop ALSO rejects any call whose name is not in that
//!   allow-list with an `error: …` tool result — a backstop that keeps the
//!   conversation API-valid even if a model fabricates a name.
//! - **Fail CLOSED on classifier outage.** The interactive loop fails OPEN in
//!   Auto mode (an unavailable classifier auto-runs a risky call). A sub-agent
//!   has no human to fall back to, so an unavailable classifier BLOCKS the risky
//!   call instead — the safe default for an unattended actor.
//! - **No human approval.** There is no `y/n`: a risky call is gated solely by
//!   the tool-call classifier (TAC). When the harness is disabled (no Safeguard
//!   route), TAC is "unavailable" and the fail-closed rule blocks the call.

// Inert in Stage 1: the loop is fully implemented but not yet driven by the chat
// loop / `task` tool, so its items are unreferenced from the binary until a later
// stage wires it in.
#![allow(dead_code)]

use std::sync::Arc;

use tokio::sync::mpsc::{self, UnboundedSender};

use crate::app::resolve::Resolved;
use crate::dto::chat::ToolCall;
use crate::model::app_config::AppConfig;
use crate::model::conversation::Conversation;
use crate::model::settings::Settings;
use crate::service::openrouter::OpenRouterClient;
use crate::service::StreamEvent;
use crate::tool::ToolCtx;

use super::event::AgentEvent;

/// Send one event on the sub-agent channel, ignoring a closed receiver (the
/// orchestrator dropped it — e.g. the sub-agent was killed — so the event is
/// simply discarded, exactly like the interactive client's `emit`).
fn emit(tx: &UnboundedSender<AgentEvent>, event: AgentEvent) {
    let _ = tx.send(event);
}

/// True for tools that mutate the workspace (or run arbitrary shell commands),
/// matching `app::runtime::stream::tool_is_risky`. Deterministic, name-based.
/// A risky call must clear the tool-call classifier before it runs.
fn tool_is_risky(name: &str) -> bool {
    matches!(name, "write" | "delete" | "edit" | "bash")
}

/// Returns `true` when `text` looks like interstitial narration rather than a
/// finished report — e.g. "Let me read a few more files:" — so the engine can
/// nudge the model to keep going instead of accepting the half-thought as done.
///
/// Criteria (any one is enough):
/// - trimmed text is empty
/// - trimmed text ends with `:`  (classic "Let me read…:" cliffhanger)
/// - trimmed text is shorter than 40 chars  (too short to be a real report)
/// - trimmed text starts with a known procrastination phrase (case-insensitive)
fn is_stall(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.ends_with(':') || t.len() < 40 {
        return true;
    }
    let lower = t.to_lowercase();
    let stall_prefixes = [
        "let me", "i'll", "i will", "let's", "now i", "next,", "next i",
        "first,",
    ];
    stall_prefixes.iter().any(|p| lower.starts_with(p))
}

/// One drained stream result: the assistant text, any requested tool calls, and
/// a fatal error if the stream failed.
#[derive(Default)]
struct StreamOutcome {
    text: String,
    tool_calls: Vec<ToolCall>,
    error: Option<String>,
}

/// Run the autonomous sub-agent loop to completion.
///
/// Loops up to `max_steps` model calls. Each step:
/// 1. emits [`AgentEvent::Step`], then streams one reply via
///    [`OpenRouterClient::stream_complete`] on the resolved route, draining the
///    per-step channel (accumulating assistant text as [`AgentEvent::Token`]s,
///    collecting any tool calls, capturing a fatal error);
/// 2. pushes the assistant message into the isolated `convo`;
/// 3. if the model requested NO tools, emits [`AgentEvent::Done`] with the
///    answer and returns;
/// 4. otherwise runs each requested call — rejecting not-permitted names,
///    classifier-gating risky ones (fail CLOSED), running the rest via
///    [`crate::tool::execute_tool`] — pushing every result back into `convo` so
///    the next step sees them.
///
/// Exhausting the budget emits [`AgentEvent::Done`] with the last assistant text
/// (or a "(stopped: step budget reached)" note). A fatal stream error emits
/// [`AgentEvent::Error`] and returns. Never panics; a dropped receiver makes
/// every emit a no-op.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop(
    client: Arc<OpenRouterClient>,
    resolved: Resolved,
    config: AppConfig,
    settings: Settings,
    tools: Vec<String>,
    ctx: ToolCtx,
    mut convo: Conversation,
    task_intent: String,
    max_steps: usize,
    tx: UnboundedSender<AgentEvent>,
) {
    // The most-recent assistant text, surfaced as the final answer if the loop
    // runs out of steps before the model gives a no-tool reply.
    let mut last_text = String::new();
    // Count how many consecutive stall nudges have been issued so far.
    let mut nudges: usize = 0;

    for step in 0..max_steps {
        emit(&tx, AgentEvent::Step(step));

        // 1. Stream one model reply on a fresh per-step channel, then drain it.
        //    Advertise ONLY this agent's allow-list to the model (the execution
        //    gate below stays as a backstop).
        let outcome = stream_step(&client, &resolved, convo.history(), &tools, &tx).await;

        // A fatal stream error ends the run immediately.
        if let Some(err) = outcome.error {
            emit(&tx, AgentEvent::Error(err));
            return;
        }

        let assistant_text = outcome.text;
        let tool_calls = outcome.tool_calls;
        if !assistant_text.trim().is_empty() {
            last_text = assistant_text.clone();
        }

        // 2. Commit the assistant turn into the isolated history (with tool calls
        //    when present so the tool results can answer them). Reasoning is
        //    display-only and not tracked by the sub-agent, so `None`.
        if tool_calls.is_empty() {
            // 3. No tools → check whether this looks like an interstitial stall
            //    ("Let me read a few more files:" with no actual tool call) rather
            //    than a genuine final answer.  If so, nudge the model to continue
            //    instead of accepting the half-thought as a report.
            if nudges < 2 && is_stall(&assistant_text) {
                convo.push_assistant(assistant_text, None);
                convo.push_user(
                    "Continue now: call the tools you need to finish the task, \
                     then write your COMPLETE final report. \
                     Do not stop with a 'let me...' line."
                        .to_string(),
                );
                nudges += 1;
                // Turn committed (assistant + nudge): snapshot the history.
                emit(&tx, AgentEvent::Snapshot(convo.messages().to_vec()));
                // Do not emit Done; loop for another step.
                continue;
            }
            // Genuine final answer (or nudge budget exhausted).
            convo.push_assistant(assistant_text.clone(), None);
            // Final turn committed: snapshot the full history before finishing.
            emit(&tx, AgentEvent::Snapshot(convo.messages().to_vec()));
            emit(&tx, AgentEvent::Done(assistant_text));
            return;
        }
        convo.push_assistant_with_tools(assistant_text, tool_calls.clone(), None);

        // 4. Run each requested call, appending a result for EVERY call id so the
        //    conversation stays API-valid (no dangling tool_call ids).
        for call in &tool_calls {
            let name = call.function.name.clone();
            let args_json = call.function.arguments.clone();

            // 4a. Allow-list gate: a call the agent isn't permitted to make is
            //     refused with an error result (the model sees it and adapts).
            if !tools.iter().any(|t| t == &name) {
                let result = format!("error: tool {name} not permitted for this agent");
                convo.push_tool(call.id.clone(), result);
                continue;
            }

            // 4b. Risky calls (write/delete/edit/bash) must clear the tool-call
            //     classifier first. FAIL CLOSED: an unavailable classifier blocks
            //     the call (a sub-agent has no human to defer to).
            if tool_is_risky(&name) {
                let verdict = crate::app::harness::classify_toolcall(
                    &client,
                    &config,
                    &settings,
                    &task_intent,
                    &name,
                    &args_json,
                )
                .await;
                if !verdict.available {
                    let result = format!("blocked: classifier unavailable ({})", verdict.reason);
                    convo.push_tool(call.id.clone(), result);
                    continue;
                }
                if !verdict.allow {
                    let result = format!("blocked by harness: {}", verdict.reason);
                    convo.push_tool(call.id.clone(), result);
                    continue;
                }
                // available && allow → fall through and run it.
            }

            // 4c. Permitted (and, if risky, classifier-approved) → run it.
            emit(
                &tx,
                AgentEvent::ToolStarted {
                    name: name.clone(),
                    args: args_json,
                },
            );
            let result = crate::tool::execute_tool(&ctx, call);
            emit(
                &tx,
                AgentEvent::ToolDone {
                    name,
                    result: result.clone(),
                },
            );
            convo.push_tool(call.id.clone(), result);
        }
        // Turn committed (assistant + every tool result): snapshot the history
        // so the UI sees this step's tool round.
        emit(&tx, AgentEvent::Snapshot(convo.messages().to_vec()));
        // Loop: the next step re-streams with the tool results in `convo`.
    }

    // Budget exhausted without a no-tool finish. Surface the last assistant text
    // if we have one; otherwise a clear "stopped" note.
    let final_text = if last_text.trim().is_empty() {
        "(stopped: step budget reached)".to_string()
    } else {
        last_text
    };
    emit(&tx, AgentEvent::Done(final_text));
}

/// Stream a single model reply and drain its events into a [`StreamOutcome`].
///
/// Opens a fresh inner [`StreamEvent`] channel, dispatches
/// [`OpenRouterClient::stream_complete`] on the resolved route, and folds the
/// drained events: `Token` deltas append to the text (and are re-emitted as
/// [`AgentEvent::Token`]), `ToolCalls` are collected, `Error` is captured.
/// `Reasoning` / `Usage` are display-only accounting the sub-agent ignores.
async fn stream_step(
    client: &Arc<OpenRouterClient>,
    resolved: &Resolved,
    history: Vec<crate::dto::chat::ChatMessage>,
    tools: &[String],
    tx: &UnboundedSender<AgentEvent>,
) -> StreamOutcome {
    let (inner_tx, mut inner_rx) = mpsc::unbounded_channel();
    // Dispatch the stream as a task so we can drain its events concurrently. The
    // task owns its sender; the channel closes when it finishes, ending the drain.
    let c = Arc::clone(client);
    let model_id = resolved.model_id.clone();
    let provider = resolved.provider().to_string();
    let effort = resolved.effort.clone();
    let endpoint = resolved.endpoint.clone();
    let api_key = resolved.api_key.clone();
    // Advertise only this agent's allow-list (owned clone moved into the task).
    let advertise = tools.to_vec();
    let send = tokio::spawn(async move {
        let conn = crate::service::openrouter::Conn {
            endpoint: &endpoint,
            api_key: &api_key,
        };
        let _ = c
            .stream_complete(conn, &model_id, &provider, &effort, history, &advertise, inner_tx)
            .await;
    });

    let mut outcome = StreamOutcome::default();
    while let Some(event) = inner_rx.recv().await {
        match event {
            StreamEvent::Token(t) => {
                if !t.is_empty() {
                    outcome.text.push_str(&t);
                    emit(tx, AgentEvent::Token(t));
                }
            }
            StreamEvent::ToolCalls(calls) => {
                outcome.tool_calls = calls;
            }
            StreamEvent::Error(e) => {
                outcome.error = Some(e);
            }
            // Display-only / accounting events the sub-agent doesn't track.
            StreamEvent::Reasoning(_)
            | StreamEvent::Usage { .. }
            | StreamEvent::Done
            | StreamEvent::Compacted { .. }
            | StreamEvent::HarnessVerdict { .. }
            | StreamEvent::EndpointsLoaded { .. }
            | StreamEvent::EndpointsError { .. } => {}
        }
    }
    // The sender task has nothing left to emit; await it so it's fully joined
    // (it only ever returns `()` and never panics — every failure is an `Error`
    // event already folded above).
    let _ = send.await;
    outcome
}
