//! Action handlers for chat-turn lifecycle: Submit, Interrupt, Resend,
//! ApproveTool, DenyTool.

use std::sync::Arc;

use anyhow::Result;

use crate::app::state::AppState;
use crate::dto::chat::Role;
use crate::model::msglog;
use crate::service::openrouter::OpenRouterClient;

use crate::app::runtime::stream::{abort_current, dispatch_deferred, process_tools, run_tool, start_stream_task};

/// Handle `Action::Submit`: push the user message, spawn the stream task, and
/// optionally kick off the advisory prompt-classifier on a background task.
pub(super) fn handle_submit(
    text: String,
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    if client.is_none() || state.rest.session.is_none() {
        state.rest.status = "no active session".into();
        return Ok(());
    }
    if state.rest.waiting {
        state.rest.status = "busy — wait for response".into();
        return Ok(());
    }
    // Prompt-classifier (PC): keep a copy of the user's prompt to
    // classify in the background once the turn is kicked off.
    let pc_prompt = text.clone();
    // Send-time @-scan backstop (Slice 3): before finalising the user message,
    // scan the typed text for hand-typed `@path/to/image.png` tokens that were
    // NOT converted interactively (path-paste / @-picker).  Each such token is
    // ingested and rewritten to `[Image #N]` in `text`.  This runs BEFORE
    // `take_attachments()` so the interactive attachments are still on the
    // composer; the scan's attachments are appended to them afterward.
    let (text, scan_attachments) =
        if let Some(sess) = state.rest.session.as_ref() {
            let images_dir = sess.images_dir();
            let workdir = sess.workdir();
            crate::model::attachment::scan_at_image_tokens(&text, &images_dir, &workdir)
        } else {
            (text, Vec::new())
        };
    // Move the staged composer attachments (from path-paste / @-picker) AND the
    // scan-backstop attachments onto THIS user message. Ingested bytes are already
    // on disk under `<session>/images/`; the wire builder re-reads at send time.
    let mut attachments = state.rest.take_attachments();
    attachments.extend(scan_attachments);
    let had_image = !attachments.is_empty();
    let history = {
        let sess = state.rest.session.as_mut().unwrap();
        let _ = msglog::append(&sess.path, Role::User, &text, None);
        sess.conversation.push_user_with_attachments(text, attachments);
        if let Err(e) = sess.save() {
            state.rest.status = format!("error: {e}");
            return Ok(());
        }
        sess.conversation.history()
    };
    // Image-capability guard: if this message carries images and the resolved Main
    // model can't read them, DON'T spend an API call — post a friendly notice in the
    // chat and keep the image un-sent (the orange attachment tree still shows it).
    // Capability mirrors run.rs: cold/None catalogue => assume capable (never wrongly block).
    if had_image {
        let main = state.rest.session.as_ref().and_then(|sess| {
            crate::app::resolve::resolve_role(
                &state.rest.config,
                &sess.settings,
                crate::model::app_config::ModelRole::Main,
            )
        });
        let capable = match (state.rest.models_cache.as_deref(), main.as_ref()) {
            (Some(models), Some(m)) => crate::service::openrouter::model_takes_images(models, &m.model_id),
            _ => true,
        };
        if !capable {
            crate::app::runtime::stream::push_image_unsupported_notice(&mut state.rest);
            state.rest.reset_scroll();
            state.rest.status = "ready".into();
            return Ok(());
        }
    }
    state.rest.reset_scroll();
    state.rest.begin_stream();
    state.rest.waiting = true;
    // A new user turn starts fresh: no carried-over tool-call rounds or
    // a half-finished approval machine.
    state.rest.agent_steps = 0;
    state.rest.pending_tool_calls.clear();
    state.rest.awaiting_approval = false;
    state.rest.tool_idx = 0;
    state.rest.tool_results.clear();
    // Hard-reset the deferred-task lane so a new submit can never inherit
    // awaiting_tool_tasks=true or stale pending ids from a prior halt path.
    state.rest.pending_tool_tasks.clear();
    state.rest.awaiting_tool_tasks = false;
    // Phase label for the comet: a single word the shimmer sweeps across
    // (the elapsed counter is appended by the renderer). No trailing dots —
    // the comet supplies the motion, `· Ns` supplies the elapsed.
    state.rest.status = "thinking".into();
    start_stream_task(history, state, client, handle);

    // Prompt-classifier (PC), advisory + non-blocking: once per turn, if
    // the harness is enabled, classify the user prompt on a background
    // task. It sends one HarnessVerdict on a dedicated channel (drained
    // in run_loop) — it NEVER gates the stream that just started. Drop
    // any stale receiver from a prior turn first.
    state.rest.harness_rx = None;
    let pc_inputs = match (client.as_ref(), state.rest.session.as_ref()) {
        (Some(c), Some(sess)) if sess.settings.classifier_enabled => Some((
            Arc::clone(c),
            state.rest.config.clone(),
            sess.settings.clone(),
        )),
        _ => None,
    };
    if let Some((c, config, settings)) = pc_inputs {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        state.rest.harness_rx = Some(rx);
        handle.spawn(async move {
            let v =
                crate::app::harness::classify_prompt(&c, &config, &settings, &pc_prompt)
                    .await;
            // A dropped receiver (turn superseded / app closing) makes
            // this a no-op — same contract as the streaming channel.
            let _ = tx.send(crate::service::StreamEvent::HarnessVerdict {
                allow: v.allow,
                reason: v.reason,
            });
        });
    }
    Ok(())
}

/// Handle `Action::Interrupt`: abort the in-flight stream, finalize the partial
/// buffer with an "[interrupted]" marker, and reset the agentic-loop state.
pub(super) fn handle_interrupt(state: &mut AppState) -> Result<()> {
    // Custom finalization (not finish_stream): the partial buffer is
    // committed with an "  [interrupted]" marker. abort_current drops
    // active_rx, so the aborted task's late events are ignored.
    if state.rest.waiting {
        abort_current(&mut state.rest);
        // Halt the agentic loop: drop any stashed tool calls, reset the
        // step counter, and clear the approval machine so a halt mid-
        // approval doesn't leave the turn wedged.
        state.rest.pending_tool_calls.clear();
        state.rest.agent_steps = 0;
        state.rest.awaiting_approval = false;
        state.rest.approval_reason = None;
        state.rest.tool_idx = 0;
        state.rest.tool_results.clear();
        // Abandon any round parked on deferred task-tool delegations so a
        // sub-agent that finishes AFTER this interrupt can't resume a turn
        // the user killed. The orphaned sub-agents keep running in the
        // background; their terminal delivery finds no matching pending id
        // and is dropped (no chat fold, no re-stream).
        state.rest.pending_subagent_calls.clear();
        state.rest.awaiting_subagents = false;
        // Also drop any QUEUED `task`-tool delegations that belonged to this
        // killed turn (their call ids were just cleared above, so they'd never
        // resume anything). User-initiated `/task` queue entries (tool_call_id
        // == None) are turn-independent and stay queued.
        state
            .rest
            .pending_subagents
            .retain(|p| p.tool_call_id.is_none());
        // Abandon any round parked on a deferred tool task (the heavy/blocking
        // tools — read/write/edit/delete/bash/grep/glob/remember/web_fetch/
        // web_search) the same way. The off-thread worker keeps running but its
        // result lands with no matching pending id, so the next-turn machine reset
        // discards it; it can't resume a turn the user killed. The channel itself
        // is left intact for reuse by later deferred tools. We deliberately do NOT
        // join the worker here — joining could block the UI thread for the full
        // duration of the tool (e.g. a long bash or HTTP timeout), the exact freeze
        // this fix removes.
        state.rest.pending_tool_tasks.clear();
        state.rest.awaiting_tool_tasks = false;
        // Take any captured usage unconditionally so a partial turn's
        // usage can't leak into the next response.
        let usage = state.rest.pending_usage.take();
        // Likewise drain the reasoning buffer unconditionally so a
        // half-streamed thinking block can't bleed into the next turn;
        // it's folded onto the interrupted message (display-only).
        let reasoning = state.rest.take_reasoning();
        let buf = state.rest.take_stream();
        if let Some(b) = buf {
            if !b.is_empty() {
                if let Some(sess) = state.rest.session.as_mut() {
                    let content = format!("{b}  [interrupted]");
                    let _ = msglog::append(&sess.path, Role::Assistant, &content, usage);
                    sess.conversation.push_assistant(content, reasoning);
                    let _ = sess.save();
                    if let Some((pt, ct, cost)) = usage {
                        state.rest.tokens_in = pt;        // current context size, not a sum
                        state.rest.tokens_out += ct;
                        state.rest.cost += cost;
                    }
                }
            }
        }
    }
    state.rest.status = "interrupted".into();
    Ok(())
}

/// Handle `Action::Resend`: pop trailing assistant messages and re-stream the
/// last user turn.
pub(super) fn handle_resend(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    if state.rest.waiting {
        state.rest.status = "busy — wait for response".into();
        return Ok(());
    }
    if client.is_none() || state.rest.session.is_none() {
        state.rest.status = "no active session".into();
        return Ok(());
    }
    let history = {
        let sess = state.rest.session.as_mut().unwrap();
        if sess.conversation.last_user_content().is_none() {
            state.rest.status = "nothing to resend".into();
            return Ok(());
        }
        sess.conversation.pop_trailing_assistants();
        let _ = sess.save();
        sess.conversation.history()
    };
    state.rest.reset_scroll();
    state.rest.begin_stream();
    state.rest.waiting = true;
    state.rest.status = "thinking".into();
    start_stream_task(history, state, client, handle);
    Ok(())
}

/// Handle `Action::ApproveTool`: run the paused risky call, record its result,
/// advance past it, then resume the tool-processing machine.
pub(super) fn handle_approve_tool(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    // Run the paused risky call, record its result, advance past it, then
    // resume the machine (which may pause again on the next risky call or
    // finish the round). Clone the call out first so `run_tool`'s mutable
    // borrow of `state` doesn't overlap the `pending_tool_calls` read.
    state.rest.awaiting_approval = false;
    state.rest.approval_reason = None;
    if let Some(call) = state.rest.pending_tool_calls.get(state.rest.tool_idx).cloned() {
        // The approved call is a risky tool (write/edit/delete/bash) — all of which
        // are heavy/blocking and live in `DEFERRED_TOOLS`. Run it OFF the UI thread
        // and PARK rather than running it inline here: an approved large write would
        // otherwise re-freeze the comet for the whole write. `dispatch_deferred`
        // advances `tool_idx` and registers the pending id; we return without
        // re-entering `process_tools` (the resume gate does that once the result
        // lands). The defensive `else` keeps the old inline path for the unexpected
        // case of a non-deferred approved tool, so no call is ever left dangling.
        if crate::tool::DEFERRED_TOOLS.contains(&call.function.name.as_str()) {
            dispatch_deferred(state, &call);
            return Ok(());
        }
        let result = run_tool(state, &call);
        state.rest.tool_results.push((call.id.clone(), result));
        state.rest.tool_idx += 1;
    }
    process_tools(state, client, handle);
    Ok(())
}

/// Handle `Action::DenyTool`: answer every pending call with "denied by user",
/// commit, and stop — do not re-stream.
pub(super) fn handle_deny_tool(state: &mut AppState) -> Result<()> {
    state.rest.awaiting_approval = false;
    state.rest.approval_reason = None;
    // Denial halts the turn. Answer the denied call AND every remaining
    // pending call with "denied by user" (so the conversation stays
    // API-valid: every tool_call gets a result), commit any results
    // already collected this round, then STOP — do not re-stream.
    let results = state.rest.tool_results.clone();
    let denied_ids: Vec<String> = state
        .rest
        .pending_tool_calls
        .iter()
        .skip(state.rest.tool_idx)
        .map(|c| c.id.clone())
        .collect();
    if let Some(sess) = state.rest.session.as_mut() {
        for (id, result) in &results {
            let _ = msglog::append(
                &sess.path,
                Role::Tool,
                result,
                None,
            );
            sess.conversation.push_tool(id.clone(), result.clone());
        }
        for id in &denied_ids {
            let _ = msglog::append(
                &sess.path,
                Role::Tool,
                "denied by user",
                None,
            );
            sess.conversation.push_tool(id.clone(), "denied by user".to_string());
        }
        let _ = sess.save();
    }
    // Reset the agentic-loop state and end the turn.
    state.rest.pending_tool_calls.clear();
    state.rest.tool_idx = 0;
    state.rest.tool_results.clear();
    state.rest.agent_steps = 0;
    state.rest.waiting = false;
    state.rest.current_task = None;
    // Clear deferred-task state so the resume gate can't ghost-restart
    // a killed turn via stale awaiting flags or leftover pending ids.
    state.rest.pending_subagent_calls.clear();
    state.rest.awaiting_subagents = false;
    state.rest.pending_tool_tasks.clear();
    state.rest.awaiting_tool_tasks = false;
    state.rest.status = "denied — stopped".into();
    Ok(())
}
