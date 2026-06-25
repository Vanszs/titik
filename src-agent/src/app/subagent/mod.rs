//! Self-contained sub-agent runtime: run a defined agent as an autonomous
//! LLM-tool loop in a background tokio task.
//!
//! A sub-agent is an [`AgentDef`](crate::model::agent_def::AgentDef) (persona +
//! model + tool allow-list + step budget) driven to completion WITHOUT a human
//! in the loop. It runs against its own isolated [`Conversation`](crate::model::conversation::Conversation),
//! reports progress as a stream of [`AgentEvent`](event::AgentEvent)s, and is
//! killable via its [`AbortHandle`](tokio::task::AbortHandle).
//!
//! Module map:
//! - [`event`] — [`AgentEvent`](event::AgentEvent), the task -> orchestrator
//!   progress stream.
//! - [`context`] — [`build_seed`](context::build_seed): the isolated seed
//!   conversation (persona + memory + awareness + task).
//! - [`engine`] — [`run_agent_loop`](engine::run_agent_loop): the autonomous,
//!   non-interactive stream/tool loop.
//! - [`spawn`] — [`spawn_subagent`](spawn::spawn_subagent): resolve + seed +
//!   launch, returning a [`SubAgent`] handle.
//!
//! This module is ADDITIVE and currently UNUSED — nothing in the main chat loop
//! references it yet; it is wired into the UI / `task` tool in a later stage.

// Inert in Stage 1: the whole sub-agent surface is defined but not yet wired into
// the binary, so its items are legitimately unreferenced until a later stage.
#![allow(dead_code)]

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::AbortHandle;

pub mod context;
pub mod engine;
pub mod event;
pub mod spawn;

// Re-exports form the intended public surface for the later wiring stage. The
// loop + spawn entry points are not referenced yet, so silence the unused-import
// lint on them specifically (they become live when the orchestrator calls them).
#[allow(unused_imports)]
pub use engine::run_agent_loop;
pub use event::AgentEvent;
#[allow(unused_imports)]
pub use spawn::spawn_subagent;

/// Lifecycle state of a [`SubAgent`], folded from its [`AgentEvent`] stream by
/// the orchestrator (wired up later).
///
/// - `Running`: the loop is in flight (the initial state).
/// - `Done`: the loop finished cleanly; `String` is the final answer.
/// - `Killed`: the loop was aborted via its [`AbortHandle`].
/// - `Error`: the loop hit a fatal stream error; `String` is the cause.
#[derive(Debug, Clone)]
pub enum SubAgentStatus {
    Running,
    Done(String),
    Killed,
    Error(String),
}

/// A handle to one running sub-agent: its identity, lifecycle state, abort
/// handle, event receiver, and the accumulated transcript.
///
/// The orchestrator owns the [`SubAgent`] (in a list, wired up later), drains
/// `rx` each tick to advance `status` / append to `transcript`, and calls
/// `abort.abort()` to kill it.
pub struct SubAgent {
    /// Stable per-session id, assigned by the orchestrator at spawn.
    pub id: usize,
    /// The agent definition's name this sub-agent runs (lowercased).
    pub agent_name: String,
    /// Compact one-line label (the truncated task) for display in a list.
    pub label: String,
    /// Lifecycle state, advanced as [`AgentEvent`]s are drained from `rx`.
    pub status: SubAgentStatus,
    /// Abort handle for the spawned loop task; `abort()` kills the sub-agent.
    pub abort: AbortHandle,
    /// Receiver end of the sub-agent's [`AgentEvent`] channel. Drained by the
    /// orchestrator; dropping it makes the task's emits no-ops.
    pub rx: UnboundedReceiver<AgentEvent>,
    /// Human-readable transcript lines accumulated from the event stream.
    pub transcript: Vec<String>,
}
