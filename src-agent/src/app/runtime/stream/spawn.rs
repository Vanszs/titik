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

/// Spawn a background sub-agent for `agent_name` running `task_text` under a
/// CALLER-SUPPLIED `id`, wiring it into app state. The core spawn step shared by
/// the live `spawn_task` path (which allocates a fresh id) and `try_start_pending`
/// (which reuses the queued entry's pre-allocated id). Builds the EXACT same
/// `ToolCtx` / registry / awareness / memory inputs in every case.
///
/// On success: pushes the [`crate::app::subagent::SubAgent`] (carrying `id`) into
/// `state.rest.subagents` and returns `Some(id)`. Returns `None` when there is no
/// client/session or the named agent doesn't resolve. Does NOT touch
/// `next_subagent_id` (the caller owns id allocation). Does NOT await the
/// sub-agent; the `$` panel is NOT auto-opened.
fn spawn_task_with_id(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
    id: usize,
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
    state.rest.subagents.push(sub);
    Some(id)
}

/// Spawn a background sub-agent for `agent_name` running `task_text`, allocating
/// it a FRESH id from `next_subagent_id`. Shared by the `/task` slash command and
/// the model-callable `task` tool (via [`spawn_or_queue`]) so both build the EXACT
/// same inputs and advance the same bookkeeping.
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
    let id = state.rest.next_subagent_id;
    let spawned = spawn_task_with_id(state, client, handle, id, agent_name, task_text, tool_call_id)?;
    // Only consume the id on a successful spawn (a failed spawn leaves it free).
    state.rest.next_subagent_id += 1;
    Some(spawned)
}

/// Outcome of [`spawn_or_queue`]: the delegation was started immediately, parked
/// in the pending queue, or rejected outright.
pub(crate) enum SpawnOutcome {
    /// A slot was free — the sub-agent started now under this id.
    Spawned(usize),
    /// All slots were busy — the delegation was queued under this id and will
    /// start when a slot frees.
    Queued(usize),
    /// No client/session, or (for the immediate-spawn branch) the named agent
    /// doesn't exist. Nothing was started or queued.
    Failed,
}

/// Accept a delegation: spawn it NOW if a slot is free, else ENQUEUE it.
///
/// The single decision point shared by both spawn sites (the `task`-tool
/// interception and the `/task` command). When [`running_subagents`] is below
/// [`crate::app::subagent::MAX_SUBAGENTS`] it spawns immediately (returning
/// [`SpawnOutcome::Spawned`] / [`SpawnOutcome::Failed`] exactly as [`spawn_task`]
/// would); otherwise it enqueues a [`crate::app::subagent::PendingSubagent`] with
/// a freshly-allocated id and returns [`SpawnOutcome::Queued`]. Enqueue still
/// requires a client + session (so a parked task-tool turn can actually resume);
/// without them it returns [`SpawnOutcome::Failed`] and the caller answers the
/// call with an error instead of parking forever.
///
/// This does NOT touch `pending_subagent_calls`: the blocking `task`-tool caller
/// records the call id there itself (for BOTH spawned and queued outcomes) so the
/// parked main turn waits for the delegation whether it ran now or later.
pub(crate) fn spawn_or_queue(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
    agent_name: &str,
    task_text: &str,
    tool_call_id: Option<String>,
) -> SpawnOutcome {
    if running_subagents(state) < crate::app::subagent::MAX_SUBAGENTS {
        match spawn_task(state, client, handle, agent_name, task_text, tool_call_id) {
            Some(id) => SpawnOutcome::Spawned(id),
            None => SpawnOutcome::Failed,
        }
    } else {
        // Over cap: enqueue (unlimited). Needs a client+session so the queued
        // delegation can eventually run and (for a task-tool call) unpark the turn.
        if client.is_none() || state.rest.session.is_none() {
            return SpawnOutcome::Failed;
        }
        let id = state.rest.next_subagent_id;
        state.rest.next_subagent_id += 1;
        state
            .rest
            .pending_subagents
            .push_back(crate::app::subagent::PendingSubagent {
                id,
                agent_name: agent_name.to_string(),
                prompt: task_text.to_string(),
                tool_call_id,
            });
        SpawnOutcome::Queued(id)
    }
}

/// Start queued delegations while slots are free.
///
/// Called from the event-loop sub-agent drain after a handle reaches a terminal
/// state (a slot just freed). While [`running_subagents`] is below
/// [`crate::app::subagent::MAX_SUBAGENTS`] and the queue is non-empty, it pops the
/// FRONT [`crate::app::subagent::PendingSubagent`] and spawns it via the SAME
/// spawn path used live ([`spawn_task_with_id`], reusing the queued entry's
/// pre-allocated id).
///
/// A queued `task`-tool delegation's call id is ALREADY in `pending_subagent_calls`
/// (recorded at enqueue time), so a successful start needs no bookkeeping there —
/// the id simply stays until the now-running agent finishes and the drain delivers
/// its result. If the named agent no longer resolves (or client/session vanished),
/// the entry is DROPPED; for a `task`-tool entry we also deliver an error result
/// for its call id (and remove it from `pending_subagent_calls`) so the parked
/// round can't hang on a delegation that will never run.
///
/// Early-returns (leaving the queue intact) when there is no client/session, so a
/// transient gap doesn't drain + fail the whole queue.
pub(crate) fn try_start_pending(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) {
    if client.is_none() || state.rest.session.is_none() {
        return;
    }
    while running_subagents(state) < crate::app::subagent::MAX_SUBAGENTS {
        let Some(pending) = state.rest.pending_subagents.pop_front() else {
            break;
        };
        let started = spawn_task_with_id(
            state,
            client,
            handle,
            pending.id,
            &pending.agent_name,
            &pending.prompt,
            pending.tool_call_id.clone(),
        );
        if started.is_none() {
            // The agent no longer resolves. Drop the entry; for a task-tool
            // delegation, free its parked call so the round can't hang.
            if let Some(call_id) = pending.tool_call_id {
                if state.rest.pending_subagent_calls.contains(&call_id) {
                    state.rest.pending_subagent_calls.retain(|c| c != &call_id);
                    state.rest.tool_results.push((
                        call_id,
                        format!("error: unknown agent '{}'", pending.agent_name),
                    ));
                }
            }
            // Try the next queued entry within the same free slot.
            continue;
        }
    }
}
