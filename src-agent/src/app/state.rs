use tokio::task::AbortHandle;
use crate::app::mode::Mode;
use crate::model::session::Session;
use crate::service::Generation;

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
    /// Monotonic generation of the in-flight task; events with a different
    /// generation are discarded.
    pub generation: Generation,
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
            generation: 0,
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

    /// Increment and return the new generation. Called when starting/aborting
    /// any task so stale events are filtered out.
    pub fn bump_generation(&mut self) -> Generation {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    pub fn remember_creds(&mut self, key: &str, model: &str) {
        self.last_key = Some(key.to_string());
        self.last_model = Some(model.to_string());
    }
}
