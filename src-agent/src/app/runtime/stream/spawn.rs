//! Sub-agent spawning and tool context construction.

use std::sync::Arc;

use crate::app::state::AppState;
use crate::service::openrouter::OpenRouterClient;

/// Build a [`crate::tool::ToolCtx`] from the current session + shared dir cache.
///
/// Centralises the workspace/workspaces/memory_dir construction so that both
/// `run_tool` (inline tool calls) and the `/task` spawner (sub-agent launch)
/// use the EXACT same paths and dir-cache reference.
pub(crate) fn build_tool_ctx(state: &AppState) -> crate::tool::ToolCtx {
    let session_ref = state.rest.session.as_ref();
    let workspace = session_ref
        .as_ref()
        .map(|s| s.workdir())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let workspaces = session_ref
        .as_ref()
        .map(|s| s.workdirs())
        .unwrap_or_else(|| vec![workspace.clone()]);
    let memory_dir = session_ref
        .as_ref()
        .map(|s| s.path.join("memory"));
    crate::tool::ToolCtx {
        workspace,
        workspaces,
        dir_cache: state.rest.dir_cache.clone(),
        memory_dir,
    }
}

/// Count the sub-agents currently in [`crate::app::subagent::SubAgentStatus::Running`].
///
/// This is the live concurrency figure both spawn paths check against
/// [`crate::app::subagent::MAX_SUBAGENTS`] before launching: terminated
/// sub-agents are pruned each tick, so a `Running` count is exactly the number
/// of occupied slots. `pub(crate)` so the `/task` command handler can share it.
pub(crate) fn running_subagents(state: &AppState) -> usize {
    state
        .rest
        .subagents
        .iter()
        .filter(|s| matches!(s.status, crate::app::subagent::SubAgentStatus::Running))
        .count()
}

/// Spawn a background sub-agent for `agent_name` running `task_text`, wiring it
/// into app state. Shared by the `/task` slash command and the model-callable
/// `task` tool so both build the EXACT same `ToolCtx` / registry / awareness /
/// memory inputs and advance the same bookkeeping.
///
/// On success: increments `next_subagent_id`, pushes the [`crate::app::subagent::SubAgent`]
/// into `state.rest.subagents`, and returns `Some(id)` (the id assigned to the
/// spawned sub-agent). Returns `None` when there is no client/session or the
/// named agent doesn't exist — the caller surfaces that as it sees fit. Does NOT
/// await the sub-agent. The `$` panel is NOT auto-opened; the user opens it manually.
pub(crate) fn spawn_task(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
    agent_name: &str,
    task_text: &str,
    tool_call_id: Option<String>,
) -> Option<usize> {
    if client.is_none() || state.rest.session.is_none() {
        return None;
    }
    // Snapshot inputs before borrowing state mutably below — identical to the
    // `/task` command's construction so the two paths can never diverge.
    let ctx = build_tool_ctx(state);
    let (session_dir, config, settings, awareness, memory_md) = {
        let sess = state.rest.session.as_ref().unwrap();
        let session_dir = sess.path.clone();
        let config = state.rest.config.clone();
        let settings = sess.settings.clone();
        let awareness = state.rest.awareness_summary.clone().unwrap_or_default();
        let memory_md =
            std::fs::read_to_string(session_dir.join("memory").join("MEMORY.md"))
                .unwrap_or_default();
        (session_dir, config, settings, awareness, memory_md)
    };

    let registry = crate::model::agent_def::AgentRegistry::load(Some(&session_dir));
    let id = state.rest.next_subagent_id;
    let client_arc = Arc::clone(client.as_ref().unwrap());

    let sub = crate::app::subagent::spawn_subagent(
        &client_arc,
        handle,
        &registry,
        &config,
        &settings,
        ctx,
        &awareness,
        &memory_md,
        id,
        agent_name,
        task_text,
        tool_call_id,
    )?;
    state.rest.next_subagent_id += 1;
    state.rest.subagents.push(sub);
    Some(id)
}
