//! View layer — render dispatcher ("V" in MVC).
//!
//! The single entry-point [`draw`] is called once per event-loop tick by the
//! runtime after state has been updated.  It inspects the current [`Mode`] and
//! forwards to the appropriate module:
//!
//! - [`chat`]           – the main conversation view (messages + input bar)
//! - [`key_input`]      – the first-run / reconfigure credentials form
//! - [`session_picker`] – the `--resume` session list with search bar
//!
//! No logic lives here; all rendering decisions belong to the sub-modules.

pub mod chat;
pub mod key_input;
pub mod session_picker;

use ratatui::Frame;
use crate::app::mode::Mode;
use crate::app::state::AppState;

/// Render the entire terminal frame for the current application state.
///
/// Called by the runtime on every UI refresh tick.  Delegates to the
/// mode-specific draw function; only one mode is active at a time.
pub fn draw(frame: &mut Frame, state: &AppState) {
    match &state.mode {
        Mode::Chat => chat::draw(frame, &state.rest),
        Mode::KeyInput(form) => key_input::draw(frame, form),
        Mode::SessionPicker(p) => session_picker::draw(frame, p),
    }
}
