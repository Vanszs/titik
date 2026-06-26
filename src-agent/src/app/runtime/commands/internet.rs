//! Internet command: `/internet [simple|full]` — toggle or set internet mode.

use anyhow::Result;

use crate::app::state::AppState;
use crate::model::settings::InternetMode;

/// Brief status label for a just-applied internet `mode`.
///
/// Always returns a string; the caller writes it to `rest.status` so it
/// flashes in the status bar until the next action resets it to `"ready"`.
pub(crate) fn internet_status(mode: InternetMode) -> String {
    match mode {
        InternetMode::Full if !crate::internet::is_installed() => {
            "internet: full needs `koma --internet-fullmode-install`".to_string()
        }
        InternetMode::Full => "internet: full".to_string(),
        InternetMode::Simple => "internet: simple".to_string(),
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

    state.rest.status = internet_status(new_mode);
    // Toast the install instructions when Full is selected without the
    // browser backend — status bar resets on next keypress, so the toast
    // gives the user time to read the command.
    if new_mode == InternetMode::Full && !crate::internet::is_installed() {
        state.rest.set_toast_info(internet_status(new_mode));
    }

    Ok(())
}
