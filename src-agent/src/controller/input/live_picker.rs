//! Key handler for the `/swap` live-session picker (`Mode::LiveSessionPicker`).
//!
//! Opened by `/swap`. Up/Down navigate the list of currently-running sessions;
//! Esc/Ctrl+C cancel back to Chat unchanged; Enter switches the foreground to the
//! highlighted session. The picker carries its own state (a snapshot of the live
//! sessions), so `_rest` is unused — mirroring [`super::handle_rewind`].

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use crate::app::mode::LiveSessionPicker;
use crate::app::state::AppStateRest;
use super::{is_ctrl, Action};

/// Handle a key press inside the `/swap` live-session picker.
///
/// Ctrl+C and Esc both cancel back to Chat without changing the foreground.
/// Enter carries the highlighted session's Vec index out so the runtime can set
/// it as the new foreground.
pub fn handle_live_picker(
    p: &mut LiveSessionPicker,
    _rest: &mut AppStateRest,
    key: KeyEvent,
) -> Action {
    if is_ctrl(&key, 'c') {
        return Action::LiveSwitchCancel;
    }

    match key.code {
        KeyCode::Esc => Action::LiveSwitchCancel,
        KeyCode::Up => {
            p.move_up();
            Action::None
        }
        KeyCode::Down => {
            p.move_down();
            Action::None
        }
        KeyCode::Enter => match p.selected_entry() {
            // Carry the chosen session's Vec index out to the runtime.
            Some(entry) => Action::LiveSwitch(entry.idx),
            None => Action::LiveSwitchCancel,
        },
        _ => Action::None,
    }
}
