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
    pub status: String,
    pub waiting: bool,
    pub streaming: Option<String>,
    pub should_quit: bool,
    pub scroll: u16,
    pub last_key: Option<String>,
    pub last_model: Option<String>,
    pub current_task: Option<AbortHandle>,
    /// Receiver for the in-flight request's events, or `None` when idle. Each
    /// request owns a fresh channel; dropping this receiver silently discards
    /// any further events from a task that was aborted or superseded.
    pub active_rx: Option<UnboundedReceiver<StreamEvent>>,
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
            status: "ready".into(),
            waiting: false,
            streaming: None,
            should_quit: false,
            scroll: 0,
            last_key: None,
            last_model: None,
            current_task: None,
            active_rx: None,
        }
    }

    // input editing
    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
    }
    pub fn backspace(&mut self) {
        self.input.pop();
    }
    pub fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input)
    }

    // scroll
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }
    pub fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(1);
    }
    pub fn reset_scroll(&mut self) {
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

    pub fn remember_creds(&mut self, key: &str, model: &str) {
        self.last_key = Some(key.to_string());
        self.last_model = Some(model.to_string());
    }
}
