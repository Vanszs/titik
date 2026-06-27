//! State for the `/quit` confirm overlay (`Mode::QuitConfirm`).
//!
//! Shown ONLY when the user asks to quit (the `/quit` command or the quit
//! keybind) while at least one live session still has work in flight
//! (`SessionRuntime::is_working()`). When nothing is working, quit happens
//! immediately and this mode is never entered.
//!
//! It is a fixed snapshot built when the confirm opens: `working` is the count
//! of busy sessions at that moment (purely for the warning text — a session
//! finishing while the overlay is up just means a slightly stale count, which is
//! harmless). The overlay offers three choices, handled in
//! [`crate::controller::input::handle_quit_confirm`]:
//!   [k] kill all & quit — abort every session's in-flight stream, then exit;
//!   [d] detach & quit    — leave conversations persisted on disk, then exit
//!                          (Phase 1: they still die with the process — true
//!                          headless detach arrives with the daemon);
//!   [esc] cancel         — back to Chat, no quit.

/// State for the quit-confirm overlay.
///
/// `working` is the number of sessions that were busy when the overlay opened,
/// used only to render the "N session(s) still working" warning. No selection
/// cursor: the three choices are bound to distinct keys (k / d / Esc), so there
/// is no list to navigate.
pub struct QuitConfirmState {
    /// Count of live sessions with work in flight at open time. Display only.
    pub working: usize,
}

impl QuitConfirmState {
    /// Build the overlay state from the number of busy sessions.
    pub fn new(working: usize) -> Self {
        Self { working }
    }
}
