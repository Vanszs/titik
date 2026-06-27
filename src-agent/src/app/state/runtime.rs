//! [`SessionRuntime`]: the per-session EXECUTION state carved out of
//! [`super::AppStateRest`].
//!
//! This holds everything tied to ONE session's in-flight turn: its [`Session`],
//! the streaming buffers, the tool-approval / deferred-task / sub-agent state
//! machines, the shared dir cache, and the cache-warmth bookkeeping. Splitting
//! it out is the structural groundwork for running several concurrent sessions
//! later — for now there is always exactly ONE `SessionRuntime` (the foreground
//! one) and behaviour is identical to before the split.
//!
//! Streaming-lifecycle methods (`begin_stream`, `append_token`,
//! `append_reasoning`, `take_stream`, `take_reasoning`) live here because they
//! operate purely on the moved `streaming` / `stream_reasoning` buffers.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::AbortHandle;

use crate::app::subagent::{PendingSubagent, SubAgent};
use crate::dto::chat::ToolCall;
use crate::model::session::Session;
use crate::service::StreamEvent;
use crate::tool::DirCache;

/// Per-session execution state. Always non-empty in [`super::AppStateRest::sessions`];
/// the foreground one is reached through `fg()` / `fg_mut()`.
pub struct SessionRuntime {
    pub session: Option<Session>,
    pub waiting: bool,
    pub streaming: Option<String>,
    /// Parallel to `streaming`: the in-progress assistant's reasoning/thinking
    /// text, accumulated from `StreamEvent::Reasoning` deltas during a turn. Set
    /// up alongside the content buffer in `begin_stream`, drained at commit, and
    /// folded onto the committed `ChatMessage` as a display-only block (never
    /// serialised). Empty when the model emits no reasoning.
    pub stream_reasoning: String,
    pub current_task: Option<AbortHandle>,
    /// Receiver for the in-flight request's events, or `None` when idle. Each
    /// request owns a fresh channel; dropping this receiver silently discards
    /// any further events from a task that was aborted or superseded.
    pub active_rx: Option<UnboundedReceiver<StreamEvent>>,
    /// Receiver for the advisory prompt-classifier (PC) verdict. Each new turn
    /// (when the classifier is enabled) opens a fresh channel here and spawns a
    /// background task that sends one [`StreamEvent::HarnessVerdict`]. Drained in
    /// `run_loop` independently of the streaming channel, so PC never blocks or
    /// interferes with streaming. `None` when no PC task is in flight.
    pub harness_rx: Option<UnboundedReceiver<StreamEvent>>,
    /// Usage for the in-flight response, captured from the StreamEvent::Usage
    /// chunk and consumed when the assistant message is committed.
    pub pending_usage: Option<(u64, u64, f64)>,
    /// Tool calls emitted by the in-flight stream, stashed on
    /// `StreamEvent::ToolCalls` and consumed by `advance_turn` once the stream
    /// finalises. Empty when the model returned a plain (final) answer.
    pub pending_tool_calls: Vec<ToolCall>,
    /// Number of tool-call rounds taken in the current turn. Reset to 0 when a
    /// new user turn starts / the turn ends; bounded so a runaway model can't
    /// loop forever.
    pub agent_steps: usize,
    // --- tool-approval state machine (within a single agentic turn) ---
    /// Index of the next call in `pending_tool_calls` to process this round.
    pub tool_idx: usize,
    /// `(tool_call_id, result)` pairs collected so far this round, flushed into
    /// the conversation once every call in the round resolves.
    pub tool_results: Vec<(String, String)>,
    /// True while a risky call is paused waiting for the user's `y/n`. The event
    /// loop routes keys to the approval modal while this is set.
    pub awaiting_approval: bool,
    /// Reason the tool-call classifier (TAC) flagged the currently-paused call,
    /// shown in the approval overlay so the user sees WHY approval is asked.
    /// `None` for an approval that wasn't classifier-driven. Cleared when the
    /// approval resolves.
    pub approval_reason: Option<String>,
    // --- deferred tool-task lane (parallel to the sub-agent lane below) ---
    /// Tool-call ids of DEFERRED tools (see [`crate::tool::DEFERRED_TOOLS`] — the
    /// heavy/blocking ones: read / write / edit / delete / bash / grep / glob /
    /// remember / web_fetch / web_search) currently running OFF the UI thread.
    /// These tools do blocking I/O (fs reads/writes, a subprocess, a tree walk, or
    /// blocking HTTP), so running them inline on the event-loop thread would freeze
    /// the TUI for the whole call. Instead `process_tools` spawns the work on a
    /// plain `std::thread` and records the call id here; the round PARKS (mirroring
    /// `pending_subagent_calls`) until the background thread sends its result back
    /// over `tool_task_rx`, which the event-loop drain folds into `tool_results`
    /// (removing the id). The round's deferred tools run ONE AT A TIME, so this vec
    /// holds AT MOST ONE id at a time. Empty when no deferred tool is in flight.
    pub pending_tool_tasks: Vec<String>,
    /// True while a tool round is PARKED waiting on a deferred tool task (see
    /// `pending_tool_tasks`). Set by `dispatch_deferred` (or alongside
    /// `awaiting_subagents` for a task-tool park) when `process_tools` returns
    /// without `finish_tool_round`; cleared by the event-loop drain once the
    /// deferred tool has delivered its result, which then resumes the round.
    /// Keeps the busy/shimmer indicator on while parked.
    pub awaiting_tool_tasks: bool,
    /// Receiver for deferred tool-task results: `(tool_call_id, result_string)`.
    /// Lazily created (with `tool_task_tx`) the first time a deferred tool is
    /// dispatched in a session, then reused. Drained each event-loop tick into
    /// `tool_results`. `None` until the first deferred tool runs.
    pub tool_task_rx: Option<UnboundedReceiver<(String, String)>>,
    /// Sender half of the deferred tool-task channel. Cloned into each spawned
    /// tool thread (the sender is `Send`, so it can fire from a non-tokio thread).
    /// Kept here so later deferred tools in the same session reuse the one channel.
    /// `None` until the first deferred tool runs.
    pub tool_task_tx: Option<UnboundedSender<(String, String)>>,
    /// All sub-agents spawned this session (running + finished). Drained each tick
    /// by the event loop; finished ones stay in the list for the UI to show their
    /// final state.
    pub subagents: Vec<SubAgent>,
    /// FIFO queue of delegations accepted while all [`crate::app::subagent::MAX_SUBAGENTS`]
    /// slots were busy. Unlimited length: over-cap delegations ENQUEUE here instead
    /// of being refused. `try_start_pending` (in the event-loop sub-agent drain)
    /// pops the FRONT and spawns it whenever a running sub-agent terminates and a
    /// slot frees, so at most `MAX_SUBAGENTS` ever run at once. Each entry's id is
    /// pre-allocated from `next_subagent_id` at enqueue time (stable `$`-panel row);
    /// a `task`-tool entry's call id is also held in `pending_subagent_calls` so the
    /// parked main turn waits for the queued delegation too.
    pub pending_subagents: VecDeque<PendingSubagent>,
    /// Tool-call ids of in-flight `task`-tool delegations whose result the main
    /// agent is still waiting for. The model-callable `task` tool DEFERS its tool
    /// result (mirroring the `awaiting_approval` park): `process_tools` pushes the
    /// call id here instead of an immediate "started" result, the round parks, and
    /// the event-loop sub-agent drain delivers the FULL report into `tool_results`
    /// (removing the id) once that sub-agent reaches a terminal state. Empty when
    /// no task delegation is pending. The `/task` slash command path never touches
    /// this (its sub-agents carry `tool_call_id == None`).
    pub pending_subagent_calls: Vec<String>,
    /// True while a tool round is PARKED waiting on one or more deferred
    /// `task`-tool delegations (see `pending_subagent_calls`). Set when
    /// `process_tools` returns without calling `finish_tool_round`; cleared by the
    /// event-loop drain once every pending delegation has filled its result, which
    /// then resumes the round (`finish_tool_round`) so the main agent reacts to the
    /// delegated reports. Keeps the busy/shimmer indicator on while parked.
    pub awaiting_subagents: bool,
    /// Monotonic counter: the id assigned to the NEXT spawned sub-agent.
    #[allow(dead_code)]
    pub next_subagent_id: usize,
    /// Background-refreshed index of the active session's workspace files
    /// (gitignore-respecting). Re-indexed off-thread; shared with the tool layer.
    pub dir_cache: Arc<RwLock<DirCache>>,
    /// Project-awareness summary (Phase 2): a few-sentence digest of the
    /// project's depth-1 docs, produced by a secondary model at startup and
    /// after `/compact`. Appended to the first System message on every request
    /// (see `runtime::stream::start_stream_task`) so it survives compaction.
    /// `None` when awareness is disabled, no docs exist, or the call failed —
    /// it is recomputed per session, never persisted.
    pub awareness_summary: Option<String>,
    /// Path of the session whose on-disk `session.lock` THIS instance currently
    /// holds (its active session's directory). `reconcile_session_lock` keeps it
    /// in lock-step with the active session: it releases this lock when switching
    /// away and acquires the new one. The clean-exit teardown in `runtime::run`
    /// removes it; a crash leaves a stale lock that PID-liveness later sweeps.
    pub held_lock: Option<PathBuf>,
    /// Latched true the first time a response reports `cached_tokens > 0`, meaning
    /// the active provider supports and is using a prompt cache. Never reset.
    pub provider_caches: bool,
    /// Sticky engage-state for the cache-warmth-adaptive summarization hysteresis.
    /// Set true when the summarizer engages; a later wave reads and writes it.
    #[allow(dead_code)]
    pub summarizing: bool,
    /// Wall-clock instant of the most-recent send (user turn start). Stamped by
    /// the submit handler in a later wave; used to estimate prompt-cache warmth.
    #[allow(dead_code)]
    pub last_send_at: Option<Instant>,
}

impl Default for SessionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRuntime {
    pub fn new() -> Self {
        Self {
            session: None,
            waiting: false,
            streaming: None,
            stream_reasoning: String::new(),
            current_task: None,
            active_rx: None,
            harness_rx: None,
            pending_usage: None,
            pending_tool_calls: Vec::new(),
            agent_steps: 0,
            tool_idx: 0,
            tool_results: Vec::new(),
            awaiting_approval: false,
            approval_reason: None,
            pending_tool_tasks: Vec::new(),
            awaiting_tool_tasks: false,
            tool_task_rx: None,
            tool_task_tx: None,
            subagents: Vec::new(),
            pending_subagents: VecDeque::new(),
            pending_subagent_calls: Vec::new(),
            awaiting_subagents: false,
            next_subagent_id: 0,
            dir_cache: Arc::new(RwLock::new(DirCache::default())),
            awareness_summary: None,
            held_lock: None,
            provider_caches: false,
            summarizing: false,
            last_send_at: None,
        }
    }

    /// Streaming lifecycle methods.
    pub fn begin_stream(&mut self) {
        self.streaming = Some(String::new());
        // Arm the parallel reasoning buffer fresh so the previous round's
        // thinking can never bleed into this one.
        self.stream_reasoning.clear();
    }

    pub fn append_token(&mut self, t: &str) {
        if let Some(buf) = self.streaming.as_mut() {
            buf.push_str(t);
        }
    }

    /// Append a reasoning fragment to the parallel thinking buffer (driven by
    /// `StreamEvent::Reasoning`, mirroring `append_token` for content).
    pub fn append_reasoning(&mut self, t: &str) {
        self.stream_reasoning.push_str(t);
    }

    pub fn take_stream(&mut self) -> Option<String> {
        self.streaming.take()
    }

    /// Take the accumulated reasoning buffer, clearing it. Returns `Some` only
    /// when non-empty so an empty thinking block never attaches to a message.
    /// Always clears (alongside `take_stream`) so reasoning can't leak forward.
    pub fn take_reasoning(&mut self) -> Option<String> {
        if self.stream_reasoning.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.stream_reasoning))
        }
    }
}
