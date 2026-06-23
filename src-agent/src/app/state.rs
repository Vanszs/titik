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

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::AbortHandle;
use crate::app::mode::Mode;
use crate::model::app_config::AppConfig;
use crate::model::session::Session;
use crate::service::StreamEvent;

pub struct AppState {
    pub mode: Mode,
    pub rest: AppStateRest,
}

pub struct AppStateRest {
    pub session: Option<Session>,
    /// Saved (session) before a /new or reconfigure prompt; restored on cancel.
    pub prev_session: Option<Session>,
    pub input: String,
    /// Selected row in the `/` command palette (index into the filtered list).
    pub palette_sel: usize,
    pub status: String,
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
            palette_sel: 0,
            status: "ready".into(),
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
        }
    }

    // input editing
    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
        self.palette_sel = 0;
    }
    pub fn backspace(&mut self) {
        self.input.pop();
        self.palette_sel = 0;
    }
    pub fn take_input(&mut self) -> String {
        self.palette_sel = 0;
        std::mem::take(&mut self.input)
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
}
