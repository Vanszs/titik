//! Quit-flow action handlers: the working-aware quit chokepoint and the
//! kill-all / detach / cancel choices from the [`Mode::QuitConfirm`] overlay.
//!
//! The single entry point for ANY quit request (the `/quit` command and the quit
//! keybind both route here) is [`request_quit`]:
//!   - no session is working  → quit immediately (set `should_quit`);
//!   - any session is working → open the confirm overlay so the user picks
//!     kill-all vs detach vs cancel.
//!
//! All on-disk lock teardown happens on the NATURAL exit path (after `run_loop`
//! returns, in [`crate::app::runtime::run`]), which now releases EVERY session's
//! lock — so neither handler here touches locks directly.

use crate::app::mode::{Mode, QuitConfirmState};
use crate::app::state::AppState;

/// Quit chokepoint shared by the `/quit` command and the quit keybind.
///
/// Counts the sessions with work in flight ([`SessionRuntime::is_working`]).
/// When none are busy, quit immediately (the old behaviour). When at least one
/// is busy, open the [`Mode::QuitConfirm`] overlay instead so the user must
/// choose kill-all, detach, or cancel — nothing is aborted yet.
///
/// [`SessionRuntime::is_working`]: crate::app::state::SessionRuntime::is_working
pub(in crate::app::runtime) fn request_quit(state: &mut AppState) {
    let working = state
        .rest
        .sessions
        .iter()
        .filter(|s| s.is_working())
        .count();
    if working == 0 {
        // Nothing in flight: exit straight away. The natural exit path releases
        // every session's lock on the way out.
        state.rest.should_quit = true;
    } else {
        // Work in flight: ask. The overlay snapshots the busy count for its
        // warning text.
        state.mode = Mode::QuitConfirm(Box::new(QuitConfirmState::new(working)));
    }
}

/// Handle `Action::QuitKillAll`: abort EVERY session's in-flight stream, then
/// quit. Mirrors [`crate::app::runtime::stream::abort_current`] but across ALL
/// sessions (that helper only touches the foreground): for each session it
/// aborts the task handle, drops the active receiver (so late events vanish),
/// and clears the `waiting` flag. Also tears down any in-flight compaction
/// animation (those fields are global, not per-session). Locks are released by
/// the natural exit path.
pub(super) fn handle_quit_kill_all(state: &mut AppState) {
    for s in &mut state.rest.sessions {
        if let Some(h) = s.current_task.take() {
            h.abort();
        }
        s.active_rx = None;
        s.waiting = false;
    }
    // Tear down any in-flight compaction animation / deferred apply so a kill
    // mid-compact doesn't leave bookkeeping dangling (global, set once).
    state.rest.compact_anim_start = None;
    state.rest.compact_apply_at = None;
    state.rest.compact_pending = None;
    state.rest.should_quit = true;
}

/// Handle `Action::QuitDetach`: detach & quit. Set `should_quit` WITHOUT
/// aborting anything, so each session's conversation stays persisted on disk and
/// is resumable later. Locks are released by the natural exit path.
///
/// Phase 1 caveat: there is no daemon yet, so the in-flight work still dies when
/// the process exits — "detach" here means "leave it resumable", not "keep it
/// cooking headless". The overlay copy says so explicitly.
pub(super) fn handle_quit_detach(state: &mut AppState) {
    state.rest.should_quit = true;
}

/// Handle `Action::QuitCancel`: dismiss the overlay and return to Chat
/// unchanged. Nothing is aborted; the app keeps running.
pub(super) fn handle_quit_cancel(state: &mut AppState) {
    state.mode = Mode::Chat;
}
