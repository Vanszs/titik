//! Application state: the single source of truth the UI renders from.
//!
//! [`AppState`] = the current [`Mode`] (which screen + its form/picker data)
//! plus [`AppStateRest`], the mode-independent rest of the world: the active
//! session, input buffer, status line, scroll, and the streaming machinery.
//!
//! Data flow: a keystroke becomes an `Action` (controller), the runtime applies
//! that `Action` by mutating this state, and `view::draw` reads it. Async
//! request output arrives via [`AppStateRest::active_rx`] — the receiver for the
//! one in-flight request. The runtime drains it each tick and folds the events
//! in here; dropping it cancels delivery from a superseded task.

use std::cell::RefCell;
use ratatui::text::Line;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::AbortHandle;
use crate::app::mode::Mode;
use crate::model::app_config::AppConfig;
use crate::model::session::Session;
use crate::service::StreamEvent;
use crate::view::theme::Palette;

pub struct AppState {
    pub mode: Mode,
    pub rest: AppStateRest,
}

/// Tool-approval policy for the agentic loop.
///
/// - `Auto`: every requested tool runs immediately (no prompt) — the original
///   behaviour.
/// - `Normal`: *risky* tools (write/delete) pause the turn for a `y/n` user
///   approval; *safe* tools (read/dir_list/dir_cache_update) still run inline.
///
/// Toggled with Shift+Tab or `/mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    #[default]
    Auto,
    Normal,
}

impl AgentMode {
    /// Short display label for the header / status line.
    pub fn label(self) -> &'static str {
        match self {
            AgentMode::Auto => "auto",
            AgentMode::Normal => "normal",
        }
    }
    /// The opposite mode (for the toggle key / command).
    pub fn toggled(self) -> Self {
        match self {
            AgentMode::Auto => AgentMode::Normal,
            AgentMode::Normal => AgentMode::Auto,
        }
    }
}

/// Per-frame cache of the transcript's rendered visual lines.
///
/// Markdown rendering (pulldown-cmark + syntect highlighting) and span-wrapping
/// are expensive and would otherwise re-run for every committed message on every
/// redraw (every streamed token, every scroll). This caches each NON-system
/// message's fully-rendered visual lines so they are computed once and reused
/// across frames; only NEW messages are rendered. The cache is keyed by the wrap
/// width + palette, so a resize or theme change forces a full rebuild; a shrink
/// of the message list (compaction / resend) also forces a rebuild.
#[derive(Default)]
pub struct TranscriptCache {
    pub width: usize,
    pub palette: Option<Palette>,
    /// One entry per NON-system message, in order; each is that message's
    /// rendered visual lines (bullet+indent applied, no separator).
    pub blocks: Vec<Vec<Line<'static>>>,
}

pub struct AppStateRest {
    pub session: Option<Session>,
    /// Saved (session) before a /new or reconfigure prompt; restored on cancel.
    pub prev_session: Option<Session>,
    pub input: String,
    /// Bash-style input history: index into the sent-user-message list while
    /// recalling (None = editing live input).
    pub hist_idx: Option<usize>,
    /// Live input stashed when history recall starts; restored on recall past
    /// the newest entry.
    pub input_stash: String,
    /// Selected row in the `/` command palette (index into the filtered list).
    pub palette_sel: usize,
    pub status: String,
    /// Transient error toast: (message, expiry instant). Shown at the top of the
    /// transcript and auto-dismissed once the instant passes.
    pub toast: Option<(String, std::time::Instant)>,
    /// True while the `/help` overlay is shown. Any key closes it.
    pub help_open: bool,
    pub waiting: bool,
    pub streaming: Option<String>,
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
    /// Set at the start of every new user turn; cleared once the plan nudge is
    /// delivered. While true, the first tool round is intercepted and each
    /// requested call is answered with `PLAN_NUDGE` instead of being executed,
    /// forcing the model to state its plan before actually running tools.
    pub needs_plan: bool,
    /// Project-awareness summary (Phase 2): a few-sentence digest of the
    /// project's depth-1 docs, produced by a secondary model at startup and
    /// after `/compact`. Appended to the first System message on every request
    /// (see `runtime::stream::start_stream_task`) so it survives compaction.
    /// `None` when awareness is disabled, no docs exist, or the call failed —
    /// it is recomputed per session, never persisted.
    pub awareness_summary: Option<String>,
}

impl AppState {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            rest: AppStateRest::new(),
        }
    }
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
            hist_idx: None,
            input_stash: String::new(),
            palette_sel: 0,
            status: "ready".into(),
            toast: None,
            help_open: false,
            waiting: false,
            streaming: None,
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
            needs_plan: false,
            awareness_summary: None,
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

    // input editing
    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
        self.palette_sel = 0;
        self.hist_idx = None;
    }
    pub fn backspace(&mut self) {
        self.input.pop();
        self.palette_sel = 0;
        self.hist_idx = None;
    }
    pub fn take_input(&mut self) -> String {
        self.palette_sel = 0;
        self.hist_idx = None;
        std::mem::take(&mut self.input)
    }

    /// Recall the previous (older) sent user message into the input. `users` is
    /// the session's user messages oldest-first.
    pub fn history_prev(&mut self, users: &[String]) {
        if users.is_empty() {
            return;
        }
        let next = match self.hist_idx {
            None => {
                self.input_stash = self.input.clone();
                users.len() - 1
            }
            Some(0) => return, // already at the oldest
            Some(i) => i - 1,
        };
        self.hist_idx = Some(next);
        self.input = users[next].clone();
    }
    /// Recall the next (newer) sent user message; past the newest, restore the
    /// stashed live input and leave recall mode.
    pub fn history_next(&mut self, users: &[String]) {
        match self.hist_idx {
            Some(i) if i + 1 < users.len() => {
                self.hist_idx = Some(i + 1);
                self.input = users[i + 1].clone();
            }
            Some(_) => {
                self.hist_idx = None;
                self.input = std::mem::take(&mut self.input_stash);
            }
            None => {}
        }
    }

    // scroll. `scroll` is an offset-from-top used only when NOT following;
    // `follow` pins the view to the bottom. `last_max_scroll` (set by the
    // renderer) lets these clamp without knowing the viewport here.
    pub fn scroll_up(&mut self) {
        if self.follow {
            // Leave follow starting from the current bottom offset.
            self.follow = false;
            self.scroll = self.last_max_scroll.get();
        }
        self.scroll = self.scroll.saturating_sub(1);
    }
    pub fn scroll_down(&mut self) {
        if self.follow {
            return; // already pinned to the bottom
        }
        self.scroll = self.scroll.saturating_add(1);
        if self.scroll >= self.last_max_scroll.get() {
            self.follow = true; // back at the bottom → resume following
        }
    }
    pub fn reset_scroll(&mut self) {
        self.follow = true;
        self.scroll = 0;
    }

    // streaming lifecycle
    pub fn begin_stream(&mut self) {
        self.streaming = Some(String::new());
    }
    pub fn append_token(&mut self, t: &str) {
        if let Some(buf) = self.streaming.as_mut() {
            buf.push_str(t);
        }
    }
    pub fn take_stream(&mut self) -> Option<String> {
        self.streaming.take()
    }

    pub fn remember_creds(&mut self, key: &str, model: &str, provider: &str) {
        self.last_key = Some(key.to_string());
        self.last_model = Some(model.to_string());
        self.last_provider = Some(provider.to_string());
    }

    /// Show an error toast for ~6 seconds.
    pub fn set_toast(&mut self, msg: String) {
        self.toast = Some((msg, std::time::Instant::now() + std::time::Duration::from_secs(6)));
    }
    /// Clear the toast if it has expired. Returns true if it was just cleared
    /// (so the caller can mark the frame dirty).
    pub fn tick_toast(&mut self) -> bool {
        if let Some((_, until)) = &self.toast {
            if std::time::Instant::now() >= *until {
                self.toast = None;
                return true;
            }
        }
        false
    }
}
