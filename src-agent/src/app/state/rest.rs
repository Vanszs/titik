//! [`AppStateRest`] struct definition and its constructor/default impl.
//!
//! The mode-independent "rest of the world" state: session, input buffer,
//! status line, scroll, streaming machinery, tool-approval state, and more.
//! Methods are split into sibling submodules (input, scroll, stream, misc).

use std::cell::RefCell;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::AbortHandle;
use crate::model::app_config::AppConfig;
use crate::model::session::Session;
use crate::service::{StreamEvent, WarmEvent};
use super::types::{AgentMode, CataloguePending, ToastKind, TranscriptCache};

pub struct AppStateRest {
    pub session: Option<Session>,
    /// Saved (session) before a /new or reconfigure prompt; restored on cancel.
    pub prev_session: Option<Session>,
    pub input: String,
    /// Caret position within `input`, as a CHAR index (0..=char_count). Edits
    /// (insert / backspace) and the Left/Right/Home/End keys move it; the view
    /// paints the block cursor here instead of always at the end. Kept in char
    /// units so multibyte input never splits a code point; converted to a byte
    /// offset only at the `String::insert`/`remove` call site. Reset to the end
    /// on any bulk replace (submit/clear, history recall, completion).
    pub cursor: usize,
    /// Image attachments staged by the composer (path-paste / `@`-picker) that
    /// have NOT yet been submitted. Each was produced by the ingest core (its
    /// bytes are already on disk under `<session>/images/`) and matches an
    /// `[Image #N]` marker inserted into `input`. On submit, these are MOVED onto
    /// the user `ChatMessage` and this is cleared; a `/clear` or take_input that
    /// drops the text also clears them so a stray marker can't outlive its image.
    pub pending_attachments: Vec<crate::dto::chat::Attachment>,
    /// Bash-style input history: index into the sent-user-message list while
    /// recalling (None = editing live input).
    pub hist_idx: Option<usize>,
    /// Live input stashed when history recall starts; restored on recall past
    /// the newest entry.
    pub input_stash: String,
    /// Selected row in the `/` command palette (index into the filtered list).
    pub palette_sel: usize,
    pub status: String,
    /// Transient toast: (message, expiry instant, kind). Shown at the top of the
    /// transcript and auto-dismissed once the instant passes. `kind` selects the
    /// box style (red "error" vs neutral "info").
    pub toast: Option<(String, std::time::Instant, ToastKind)>,
    /// True while the `/help` overlay is shown. Any key closes it.
    pub help_open: bool,
    pub waiting: bool,
    pub streaming: Option<String>,
    /// Parallel to `streaming`: the in-progress assistant's reasoning/thinking
    /// text, accumulated from `StreamEvent::Reasoning` deltas during a turn. Set
    /// up alongside the content buffer in `begin_stream`, drained at commit, and
    /// folded onto the committed `ChatMessage` as a display-only block (never
    /// serialised). Empty when the model emits no reasoning.
    pub stream_reasoning: String,
    pub should_quit: bool,
    pub scroll: u16,
    /// When true, the transcript stays pinned to the bottom (auto-follows new
    /// content). Cleared when the user scrolls up; re-set on reaching bottom.
    pub follow: bool,
    /// Max scroll offset (content_lines - viewport) from the LAST render. The
    /// renderer writes it (via interior mutability through a shared ref); the
    /// key/mouse scroll handlers read it to clamp + detect "at bottom". Single-
    /// threaded UI state, never sent across threads, so `Cell` is fine.
    pub last_max_scroll: std::cell::Cell<u16>,
    pub last_key: Option<String>,
    pub last_model: Option<String>,
    /// Most-recently used OpenRouter provider slug (empty string = default routing).
    pub last_provider: Option<String>,
    pub current_task: Option<AbortHandle>,
    /// Receiver for the in-flight request's events, or `None` when idle. Each
    /// request owns a fresh channel; dropping this receiver silently discards
    /// any further events from a task that was aborted or superseded.
    pub active_rx: Option<UnboundedReceiver<StreamEvent>>,
    /// Global application config (theme, accent). Loaded once at startup after
    /// `ensure_dirs`; defaults to `AppConfig::default()` until then.
    pub config: AppConfig,
    /// Set by `/select`; the event loop performs the terminal hand-off next tick.
    pub select_pending: bool,
    /// True while the conversation is dumped to the normal terminal for copying.
    pub select_active: bool,
    /// Cumulative session token/cost totals (summed from messages.sqlite on
    /// open, incremented per response). Survive /compact.
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost: f64,
    /// Prompt tokens served from the prompt cache on the LATEST response (a cache
    /// hit at the discounted rate). Like `tokens_in`, this tracks the current
    /// prompt, not a cumulative sum; set from `StreamEvent::Usage` each response,
    /// 0 on a cold prefix or a provider that doesn't report cache stats.
    pub tokens_cached: u64,
    /// Usage for the in-flight response, captured from the StreamEvent::Usage
    /// chunk and consumed when the assistant message is committed.
    pub pending_usage: Option<(u64, u64, f64)>,
    /// Cache of each committed message's rendered visual lines, reused across
    /// frames so markdown/syntect highlighting doesn't re-run every redraw.
    /// Borrowed mutably by the chat renderer through a shared `&rest` (the UI is
    /// single-threaded, so `RefCell` is fine).
    pub transcript_cache: RefCell<TranscriptCache>,
    /// Background-refreshed index of the active session's workspace files
    /// (gitignore-respecting). Re-indexed off-thread; shared with the tool layer.
    pub dir_cache: std::sync::Arc<std::sync::RwLock<crate::tool::DirCache>>,
    /// Tool calls emitted by the in-flight stream, stashed on
    /// `StreamEvent::ToolCalls` and consumed by `advance_turn` once the stream
    /// finalises. Empty when the model returned a plain (final) answer.
    pub pending_tool_calls: Vec<crate::dto::chat::ToolCall>,
    /// Number of tool-call rounds taken in the current turn. Reset to 0 when a
    /// new user turn starts / the turn ends; bounded so a runaway model can't
    /// loop forever.
    pub agent_steps: usize,
    /// Tool-approval policy. `Auto` runs every tool immediately; `Normal` pauses
    /// for `y/n` on risky (write/delete) tools. Toggled with Shift+Tab / `/mode`.
    pub agent_mode: AgentMode,
    // --- tool-approval state machine (within a single agentic turn) ---
    /// Index of the next call in `pending_tool_calls` to process this round.
    pub tool_idx: usize,
    /// `(tool_call_id, result)` pairs collected so far this round, flushed into
    /// the conversation once every call in the round resolves.
    pub tool_results: Vec<(String, String)>,
    /// True while a risky call is paused waiting for the user's `y/n`. The event
    /// loop routes keys to the approval modal while this is set.
    pub awaiting_approval: bool,
    /// Project-awareness summary (Phase 2): a few-sentence digest of the
    /// project's depth-1 docs, produced by a secondary model at startup and
    /// after `/compact`. Appended to the first System message on every request
    /// (see `runtime::stream::start_stream_task`) so it survives compaction.
    /// `None` when awareness is disabled, no docs exist, or the call failed —
    /// it is recomputed per session, never persisted.
    pub awareness_summary: Option<String>,
    /// Process working directory captured at startup. The deterministic
    /// workspace check (WC) always allows this directory regardless of the
    /// allow-list, so running the agent in the folder you want to work in just
    /// works. Set once in `runtime::run`; never mutated afterwards.
    pub launch_dir: std::path::PathBuf,
    /// Receiver for advisory prompt-classifier (PC) verdicts. Each new turn
    /// (when the classifier is enabled) opens a fresh channel here and spawns a
    /// background task that sends one [`StreamEvent::HarnessVerdict`]. Drained in
    /// `run_loop` independently of the streaming channel, so PC never blocks or
    /// interferes with streaming. `None` when no PC task is in flight.
    pub harness_rx: Option<UnboundedReceiver<StreamEvent>>,
    /// Receiver for a model's provider-endpoint fetch. Opened (replacing any
    /// previous, which drops an in-flight older fetch's receiver — the desired
    /// stale-cancel) when the model modal selects/opens an OpenRouter model;
    /// the spawned task sends one [`StreamEvent::EndpointsLoaded`] or
    /// [`StreamEvent::EndpointsError`]. Drained in `run_loop` independently of
    /// streaming. `None` when no endpoints fetch is in flight.
    pub endpoints_rx: Option<UnboundedReceiver<StreamEvent>>,
    /// Receiver for warming background tasks. Carries TWO kinds of [`WarmEvent`]:
    /// the startup project-awareness summary (opened by `runtime::warm_session` for
    /// a returning-into-Chat session, folded into `awareness_summary` and advancing
    /// the `LoadingState` splash), and the ON-DEMAND, per-endpoint model catalogue
    /// (opened by the debounced omnisearch fetch in the event-loop tick, folded into
    /// `models_cache` + `models_cache_endpoint`). Drained in `run_loop` independently
    /// of streaming, mirroring `endpoints_rx`. `None` when nothing is in flight.
    pub warm_rx: Option<UnboundedReceiver<WarmEvent>>,
    /// Reason the tool-call classifier (TAC) flagged the currently-paused call,
    /// shown in the approval overlay so the user sees WHY approval is asked.
    /// `None` for an approval that wasn't classifier-driven. Cleared when the
    /// approval resolves.
    pub approval_reason: Option<String>,
    /// Cached model catalogue (`GET {endpoint}/models`) for ONE endpoint at a
    /// time — the endpoint recorded in `models_cache_endpoint`. Fetched ON DEMAND
    /// (debounced) by the model omnisearch for whichever provider is being edited,
    /// not at boot. `Some(vec![])` is a TERMINAL "no models / fetch failed" state
    /// for that endpoint (degrade to manual model-id entry), distinct from `None`
    /// = "never fetched". Re-fetched when the active omnisearch endpoint differs
    /// from `models_cache_endpoint`.
    pub models_cache: Option<Vec<crate::dto::openrouter::ModelInfo>>,
    /// Which endpoint `models_cache` currently holds models for (`None` when the
    /// cache has never been populated). The omnisearch only filters against
    /// `models_cache` while this equals the active provider's endpoint; otherwise
    /// it shows `searching models…` and (re)requests a fetch.
    pub models_cache_endpoint: Option<String>,
    /// A debounced catalogue fetch waiting to fire (see [`CataloguePending`]).
    /// Set/refreshed by [`AppStateRest::request_catalogue`]; consumed by the
    /// event-loop tick once `due` passes. `None` when no fetch is pending.
    pub catalogue_pending: Option<CataloguePending>,
    /// The endpoint of a catalogue fetch currently IN FLIGHT (in-flight guard so
    /// the same endpoint isn't fetched twice concurrently). Set when the tick
    /// spawns the fetch; cleared by the `warm_rx` drain when the result lands.
    /// `None` when nothing is being fetched.
    pub catalogue_fetching: Option<String>,
    /// Start instant of the `/compact` animation. `Some` only while a compaction
    /// is in flight (set in `Command::Compact`, cleared once the result is
    /// applied). The renderer uses it to draw the spinner + elapsed + indeterminate
    /// bar, and the event loop uses it both to keep redrawing each tick (so the
    /// animation actually animates) and to enforce the cosmetic minimum duration.
    pub compact_anim_start: Option<std::time::Instant>,
    /// Earliest instant the stashed compaction result may be applied. Set when a
    /// fast `StreamEvent::Compacted` arrives before the minimum animation duration
    /// has elapsed; the event loop applies `compact_pending` once `now >= this`.
    pub compact_apply_at: Option<std::time::Instant>,
    /// Stashed `(summary, kept_tail)` awaiting the minimum-duration gate. Held
    /// only when a compaction finished faster than the minimum so the apply is
    /// deferred (non-blocking) rather than slept on. Applied by the event loop.
    pub compact_pending: Option<(String, Vec<crate::dto::chat::ChatMessage>)>,
    /// Path of the session whose on-disk `session.lock` THIS instance currently
    /// holds (its active session's directory). `reconcile_session_lock` keeps it
    /// in lock-step with the active session: it releases this lock when switching
    /// away and acquires the new one. The clean-exit teardown in `runtime::run`
    /// removes it; a crash leaves a stale lock that PID-liveness later sweeps.
    pub held_lock: Option<std::path::PathBuf>,
    /// Wall-clock instant of the most-recent send (user turn start). Stamped by
    /// the submit handler in a later wave; used to estimate prompt-cache warmth.
    #[allow(dead_code)]
    pub last_send_at: Option<std::time::Instant>,
    /// Latched true the first time a response reports `cached_tokens > 0`, meaning
    /// the active provider supports and is using a prompt cache. Never reset.
    pub provider_caches: bool,
    /// Sticky engage-state for the cache-warmth-adaptive summarization hysteresis.
    /// Set true when the summarizer engages; a later wave reads and writes it.
    #[allow(dead_code)]
    pub summarizing: bool,
    /// Start instant of the current WORKING wait — the moment the app entered a
    /// model/tool/fold wait that should shimmer (i.e. `waiting && !awaiting_approval`).
    /// Drives the status-line "comet" animation's elapsed counter and its travelling
    /// head. Reconciled on the rising/falling edge in the event-loop tick: set to
    /// `Some(now)` when shimmer becomes active and it's `None`; cleared to `None`
    /// the moment work ends or an approval prompt takes over. `None` when idle.
    pub work_since: Option<std::time::Instant>,
    /// The missing-root set we last warned about, so the toast fires only when
    /// the set changes (not on every reindex).
    pub warned_missing_roots: Vec<String>,
    /// All sub-agents spawned this session (running + finished). Drained each tick
    /// by the event loop; finished ones stay in the list for the UI to show their
    /// final state.
    pub subagents: Vec<crate::app::subagent::SubAgent>,
    /// True while the sub-agent panel is open (toggled by the sub-agent UI).
    #[allow(dead_code)]
    pub subagents_open: bool,
    /// Selected row in the sub-agent list (index into `subagents`).
    #[allow(dead_code)]
    pub subagent_sel: usize,
    /// When `Some(i)`, the full-screen sub-agent VIEWER is open showing
    /// `subagents[i]`'s structured conversation (rendered exactly like the main
    /// chat, view-only). `None` = not viewing. Opened with Enter on a spawned row
    /// in the `$` panel; Esc closes it back to the panel. Short-circuits the
    /// normal chat draw while set (mirrors the full-screen prompt editor).
    pub agent_viewer: Option<usize>,
    /// Scroll offset (top visual line) for the sub-agent viewer. Used only when
    /// `agent_viewer_follow` is false (not pinned). Reset to 0 when the viewer opens.
    pub agent_viewer_scroll: u16,
    /// true = pinned to the newest line; cleared when the user scrolls up,
    /// re-set when they scroll back to the bottom.
    pub agent_viewer_follow: bool,
    /// Monotonic counter: the id assigned to the NEXT spawned sub-agent.
    #[allow(dead_code)]
    pub next_subagent_id: usize,
    /// FIFO queue of delegations accepted while all [`crate::app::subagent::MAX_SUBAGENTS`]
    /// slots were busy. Unlimited length: over-cap delegations ENQUEUE here instead
    /// of being refused. `try_start_pending` (in the event-loop sub-agent drain)
    /// pops the FRONT and spawns it whenever a running sub-agent terminates and a
    /// slot frees, so at most `MAX_SUBAGENTS` ever run at once. Each entry's id is
    /// pre-allocated from `next_subagent_id` at enqueue time (stable `$`-panel row);
    /// a `task`-tool entry's call id is also held in `pending_subagent_calls` so the
    /// parked main turn waits for the queued delegation too.
    pub pending_subagents: std::collections::VecDeque<crate::app::subagent::PendingSubagent>,
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
    // --- async tool-task lane (parallel to the sub-agent lane above) ---
    /// Tool-call ids of ASYNC tools (see [`crate::tool::ASYNC_TOOLS`], e.g.
    /// `web_fetch` / `web_search`) currently running OFF the UI thread. These tools
    /// do blocking HTTP, so running them inline on the event-loop thread would
    /// freeze the TUI for the whole network round-trip. Instead `process_tools`
    /// spawns the work on a plain `std::thread` and records the call id here; the
    /// round PARKS (mirroring `pending_subagent_calls`) until the background thread
    /// sends its result back over `tool_task_rx`, which the event-loop drain folds
    /// into `tool_results` (removing the id). Empty when no async tool is in flight.
    pub pending_tool_tasks: Vec<String>,
    /// True while a tool round is PARKED waiting on one or more async tool tasks
    /// (see `pending_tool_tasks`). Set alongside (or instead of) `awaiting_subagents`
    /// when `process_tools` returns without `finish_tool_round`; cleared by the
    /// event-loop drain once every async tool has delivered its result, which then
    /// resumes the round. Keeps the busy/shimmer indicator on while parked.
    pub awaiting_tool_tasks: bool,
    /// Receiver for async tool-task results: `(tool_call_id, result_string)`. Lazily
    /// created (with `tool_task_tx`) the first time an async tool is dispatched in a
    /// session, then reused. Drained each event-loop tick into `tool_results`. `None`
    /// until the first async tool runs.
    pub tool_task_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, String)>>,
    /// Sender half of the async tool-task channel. Cloned into each spawned tool
    /// thread (the sender is `Send`, so it can fire from a non-tokio thread). Kept
    /// here so later async tools in the same session reuse the one channel. `None`
    /// until the first async tool runs.
    pub tool_task_tx: Option<tokio::sync::mpsc::UnboundedSender<(String, String)>>,
    /// Receiver for a background clipboard-image fetch (Ctrl+V). The fetch thread
    /// shells out to `wl-paste` (Wayland) or `xclip` (X11), reads raw PNG bytes, and
    /// sends `Ok(bytes)` on success or `Err(reason)` on failure (tool absent, empty
    /// clipboard, non-image data). Drained each tick in `run_loop`; on `Ok` the bytes
    /// are ingested as an attachment; on `Err` a toast is shown. `None` when no fetch
    /// is in flight.
    pub clipboard_rx: Option<std::sync::mpsc::Receiver<Result<Vec<u8>, String>>>,
}

impl Default for AppStateRest {
    fn default() -> Self {
        Self::new()
    }
}

impl AppStateRest {
    pub fn new() -> Self {
        Self {
            session: None,
            prev_session: None,
            input: String::new(),
            cursor: 0,
            pending_attachments: Vec::new(),
            hist_idx: None,
            input_stash: String::new(),
            palette_sel: 0,
            status: "ready".into(),
            toast: None,
            help_open: false,
            waiting: false,
            streaming: None,
            stream_reasoning: String::new(),
            should_quit: false,
            scroll: 0,
            follow: true,
            last_max_scroll: std::cell::Cell::new(0),
            last_key: None,
            last_model: None,
            last_provider: None,
            current_task: None,
            active_rx: None,
            config: AppConfig::default(),
            select_pending: false,
            select_active: false,
            tokens_in: 0,
            tokens_out: 0,
            cost: 0.0,
            tokens_cached: 0,
            pending_usage: None,
            transcript_cache: RefCell::new(TranscriptCache::default()),
            dir_cache: std::sync::Arc::new(std::sync::RwLock::new(
                crate::tool::DirCache::default(),
            )),
            pending_tool_calls: Vec::new(),
            agent_steps: 0,
            agent_mode: AgentMode::default(),
            tool_idx: 0,
            tool_results: Vec::new(),
            awaiting_approval: false,
            awareness_summary: None,
            launch_dir: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            harness_rx: None,
            endpoints_rx: None,
            warm_rx: None,
            approval_reason: None,
            models_cache: None,
            models_cache_endpoint: None,
            catalogue_pending: None,
            catalogue_fetching: None,
            compact_anim_start: None,
            compact_apply_at: None,
            compact_pending: None,
            held_lock: None,
            last_send_at: None,
            provider_caches: false,
            summarizing: false,
            work_since: None,
            warned_missing_roots: Vec::new(),
            subagents: Vec::new(),
            subagents_open: false,
            subagent_sel: 0,
            agent_viewer: None,
            agent_viewer_scroll: 0,
            agent_viewer_follow: true,
            next_subagent_id: 0,
            pending_subagents: std::collections::VecDeque::new(),
            pending_subagent_calls: Vec::new(),
            awaiting_subagents: false,
            pending_tool_tasks: Vec::new(),
            awaiting_tool_tasks: false,
            tool_task_rx: None,
            tool_task_tx: None,
            clipboard_rx: None,
        }
    }

    /// Load cumulative token/cost totals for `session_dir` from its sqlite log
    /// (0 if absent). Called when a session becomes active.
    pub fn load_token_totals(&mut self, session_dir: &std::path::Path) {
        let (i, o, c) = crate::model::msglog::totals(session_dir).unwrap_or((0, 0, 0.0));
        self.tokens_in = i;
        self.tokens_out = o;
        self.cost = c;
    }
}
