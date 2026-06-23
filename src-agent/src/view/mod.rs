pub mod chat;
pub mod key_input;
pub mod session_picker;

use ratatui::Frame;
use crate::app::mode::Mode;
use crate::app::state::AppState;

pub fn draw(frame: &mut Frame, state: &AppState) {
    match &state.mode {
        Mode::Chat => chat::draw(frame, &state.rest),
        Mode::KeyInput(form) => key_input::draw(frame, form),
        Mode::SessionPicker(p) => session_picker::draw(frame, p),
    }
}
