//! Internet command: `/internet [simple|full]` — toggle or set internet mode.

use anyhow::Result;

use crate::app::state::AppState;
use crate::model::settings::InternetMode;

/// Status line for a just-applied internet `mode`.
///
/// Returns `Some(msg)` **only** when `Full` is selected but the full-mode
/// browser backend is not installed yet — that is the one case where the
/// user needs to act (`koma --internet-fullmode-install`).  In all other
/// cases (`Simple`, or `Full` + already installed) the mode switch is
/// silent, so we return `None` and the caller skips the toast.
pub(crate) fn internet_status(mode: InternetMode) -> Option<String> {
    match mode {
        InternetMode::Full if !crate::internet::is_installed() => {
            Some("internet: full needs `koma --internet-fullmode-install`".to_string())
        }
        _ => None,
    }
}

/// Handle the `/internet [simple|full]` command.
///
/// `target` is `None` to toggle, or `Some(mode)` to set explicitly.
/// Mutates the active session's `internet_mode`, persists via `sess.save()`,
/// and sets a transient status line with a token-cost warning when Full.
pub(super) fn handle_internet(target: Option<InternetMode>, state: &mut AppState) -> Result<()> {
    let Some(sess) = state.rest.session.as_mut() else {
        state.rest.set_toast("no active session".to_string());
        return Ok(());
    };

    let new_mode = match target {
        Some(m) => m,
        None => match sess.settings.internet_mode {
            InternetMode::Simple => InternetMode::Full,
            InternetMode::Full => InternetMode::Simple,
        },
    };

    sess.settings.internet_mode = new_mode;
    // Refresh the system-prompt roster so any mode-gated agents stay in sync on
    // this mid-session flip (rebuild reads in-memory settings; harmless + cheap).
    sess.rebuild_system();

    if let Err(e) = sess.save() {
        state.rest.set_toast(format!("error saving settings: {e}"));
        return Ok(());
    }

    if let Some(msg) = internet_status(new_mode) {
        state.rest.set_toast_info(msg);
    }

    Ok(())
}
