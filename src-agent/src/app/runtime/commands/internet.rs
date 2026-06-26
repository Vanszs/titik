//! Internet command: `/internet [simple|full]` — toggle or set internet mode.

use anyhow::Result;

use crate::app::state::AppState;
use crate::model::settings::InternetMode;

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

    if let Err(e) = sess.save() {
        state.rest.status = format!("error saving settings: {e}");
        return Ok(());
    }

    state.rest.status = if new_mode == InternetMode::Full {
        "internet: full \u{2014} higher token usage".to_string()
    } else {
        "internet: simple".to_string()
    };

    Ok(())
}
