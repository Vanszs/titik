//! Task command: `/task <agent> <task>` — spawn a sub-agent.

use std::sync::Arc;

use anyhow::Result;

use crate::app::state::AppState;
use crate::service::openrouter::OpenRouterClient;

/// Handle the `/task <agent> <task>` command: spawn a named sub-agent.
///
/// Guards against missing session/client, empty args, and the concurrency cap.
/// Uses the shared `stream::spawn_task` path so bookkeeping never diverges from
/// the `task` tool.
pub(super) fn handle_task(
    args: String,
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    // Guard: needs an active client + session.
    if client.is_none() || state.rest.session.is_none() {
        state.rest.status = "no active session".into();
        return Ok(());
    }
    // Split the first whitespace token as the agent name; the rest is
    // the task text (original casing preserved).
    let mut tokens = args.splitn(2, char::is_whitespace);
    let agent_name = tokens.next().unwrap_or("").trim().to_string();
    let task_text = tokens.next().unwrap_or("").trim().to_string();
    if agent_name.is_empty() || task_text.is_empty() {
        state.rest.status = "usage: /task <agent> <task>".into();
        return Ok(());
    }
    // Concurrency cap: refuse a new spawn while the max are already running
    // (terminated sub-agents are pruned each tick, freeing slots). Surface a
    // toast + status and bail without spawning, mirroring the `task` tool.
    if super::super::stream::running_subagents(state) >= crate::app::subagent::MAX_SUBAGENTS {
        state.rest.set_toast("sub-agent limit reached (5 running)".into());
        state.rest.status = "sub-agent limit reached (5 running)".into();
        return Ok(());
    }

    // Spawn via the shared helper (same path the `task` tool uses) so the
    // ctx/registry/awareness/memory inputs + bookkeeping never diverge.
    match super::super::stream::spawn_task(state, client, handle, &agent_name, &task_text, None) {
        Some(id) => {
            state
                .rest
                .set_toast_info(format!("started sub-agent #{id} ({agent_name})"));
            state.rest.status = format!("started sub-agent #{id} ({agent_name})");
        }
        None => {
            state.rest.status = format!("unknown agent: {agent_name}");
        }
    }
    Ok(())
}
