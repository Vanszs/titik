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
//!
//! # Module layout
//!
//! - `types`  – [`AgentMode`], [`ToastKind`], [`TranscriptCache`], [`CataloguePending`]
//! - `rest`   – [`AppStateRest`] struct + constructor
//! - `input`  – input-editing and history `impl` blocks
//! - `scroll` – scroll `impl` block
//! - `stream` – streaming-lifecycle `impl` block
//! - `misc`   – credentials, catalogue requests, toast `impl` block

mod types;
mod rest;
mod input;
mod scroll;
mod stream;
mod misc;

use crate::app::mode::Mode;

// Re-export everything that was public in the original state.rs so all
// external paths remain identical.
pub use types::{AgentMode, ToastKind};
pub use rest::AppStateRest;

pub struct AppState {
    pub mode: Mode,
    pub rest: AppStateRest,
}

impl AppState {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            rest: AppStateRest::new(),
        }
    }
}
