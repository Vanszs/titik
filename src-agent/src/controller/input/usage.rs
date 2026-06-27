//! Controller – key handler for the `/usage` cost dashboard (Usage mode).
//!
//! The usage dashboard is read-only: the only meaningful key is `Esc`, which
//! returns to Chat. Ctrl+C quits, consistent with every other full-screen mode.
//! All other keys are silently ignored.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::{is_ctrl, Action};

/// Handle a key press while the usage dashboard is open.
///
/// Returns [`Action::CloseUsage`] on Esc, [`Action::Quit`] on Ctrl+C,
/// and [`Action::None`] for everything else.
pub(super) fn handle_usage(key: KeyEvent) -> Action {
    if is_ctrl(&key, 'c') {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::CloseUsage,
        _ => Action::None,
    }
}
