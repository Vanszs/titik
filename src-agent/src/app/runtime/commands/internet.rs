//! Internet command: `/internet [simple|full]` — toggle or set internet mode.

use anyhow::Result;

use crate::app::state::AppState;
use crate::model::settings::InternetMode;

/// Status line for a just-applied internet `mode`.
///
/// Shared by the three places that flip `internet_mode` (`/internet`, Ctrl+E,
/// and the settings save) so the messaging never drifts:
/// - `Full` but the full-mode env is NOT installed → tell the user nothing
///   actually changed and how to provision it (the browser backend is inert
///   until installed; `web_fetch` silently stays on raw HTTP otherwise).
/// - `Full` and installed → the higher-token-usage note (exact em-dash kept).
/// - `Simple` → the plain note.
pub(crate) fn internet_status(mode: InternetMode) -> String {
    match mode {
        InternetMode::Full if !crate::internet::is_installed() => {
            "internet: full needs `koma --internet-fullmode-install`".to_string()
        }
        InternetMode::Full => "internet: full \u{2014} higher token usage".to_string(),
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
        state.rest.status = "no active session".into();
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
        state.rest.status = format!("error saving settings: {e}");
        return Ok(());
    }

    state.rest.status = internet_status(new_mode);

    Ok(())
}
