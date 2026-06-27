//! Key handler for the message-rewind picker (`Mode::MessageRewind`).
//!
//! Opened by a double-Esc while idle in Chat. Up/Down navigate the list of
//! prior user messages (newest-first); Esc cancels back to Chat unchanged;
//! Enter selects the highlighted message to rewind to.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use crate::app::mode::RewindState;
use crate::app::state::AppStateRest;
use super::{is_ctrl, Action};

/// Handle a key press inside the message-rewind picker.
///
/// `_rest` is accepted for handler-signature consistency with the other mode
/// handlers but is unused here (the picker carries its own state). Ctrl+C and
/// Esc both cancel back to Chat without changing the conversation.
pub fn handle_rewind(rw: &mut RewindState, _rest: &mut AppStateRest, key: KeyEvent) -> Action {
    if is_ctrl(&key, 'c') {
        return Action::RewindCancel;
    }

    match key.code {
        KeyCode::Esc => Action::RewindCancel,
        KeyCode::Up => {
            rw.move_up();
            Action::None
        }
        KeyCode::Down => {
            rw.move_down();
            Action::None
        }
        KeyCode::Enter => Action::RewindSelect,
        _ => Action::None,
    }
}
