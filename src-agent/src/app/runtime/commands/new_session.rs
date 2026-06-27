//! New-session and resume/rename commands: `/new`, `/resume`, `/rename`.

use std::sync::Arc;

use anyhow::Result;

use crate::app::mode::{KeyInputForm, Mode, PickerState};
use crate::app::state::AppState;
use crate::config::DEFAULT_MODEL;
use crate::model::store;
use crate::service::openrouter::OpenRouterClient;

use super::super::stream::abort_current;

/// Handle the `/new` command: start a fresh session.
///
/// Aborts any in-flight request, clears agentic state, creates a new session,
/// inherits last-used credentials, and opens KeyInput if no creds are known.
pub(super) fn handle_new(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    abort_current(&mut state.rest);
    // Halt any in-flight agentic loop before swapping sessions, including
    // a half-finished approval machine.
    state.rest.pending_tool_calls.clear();
    state.rest.agent_steps = 0;
    state.rest.awaiting_approval = false;
    state.rest.tool_idx = 0;
    state.rest.tool_results.clear();
    // Drop any staged image attachments so they don't leak into the new session.
    state.rest.pending_attachments.clear();
    let _ = state.rest.take_stream(); // discard partial; belongs to old session
    let mut sess = match store::create_session() {
        Ok(s) => s,
        Err(e) => {
            state.rest.status = format!("error: {e}");
            return Ok(());
        }
    };
    // Inherit the last-used creds so a new session drops straight into
    // chat — no credential prompt. (Change them per-session via /settings.)
    sess.settings.api_key = state.rest.last_key.clone().unwrap_or_default();
    sess.settings.model = state
        .rest
        .last_model
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    sess.settings.provider = state.rest.last_provider.clone().unwrap_or_default();
    let _ = sess.save();
    state.rest.prev_session = state.rest.session.take();
    state.rest.reset_scroll();
    if sess.settings.api_key.is_empty() {
        // No creds known yet — fall back to the credential prompt.
        state.rest.session = Some(sess);
        *client = None;
        state.mode = Mode::KeyInput(KeyInputForm::prefilled(
            String::new(),
            DEFAULT_MODEL.to_string(),
            false, // Esc -> CancelKeyInput restores prev_session
            false, // not from picker
        ));
    } else {
        *client = Some(super::super::build_client());
        let sess_path = sess.path.clone();
        state.rest.session = Some(sess);
        // Fresh session → totals are 0; calling is harmless and keeps the
        // readout reset when switching sessions.
        state.rest.load_token_totals(&sess_path);
        // Land in Chat first, THEN warm: `warm_session` is non-blocking and
        // may upgrade the mode to `Mode::Loading` (animated splash) when it
        // has warm work to spawn, so it must run LAST to get the final word.
        // With no warm work it leaves the mode as the Chat we just set.
        state.mode = Mode::Chat;
        state.rest.status = "ready".into();
        // Warm the new session: reindex its workspace + (async) fetch the
        // catalogue and awareness summary so /new is primed like a cold boot.
        super::super::warm_session(state, client, handle);
    }
    Ok(())
}

/// Handle the `/resume` command: open the session picker.
///
/// Unlike CancelKeyInputToPicker we do NOT clear the current session/client —
/// if the user Escapes the picker they return to the active chat unchanged.
pub(super) fn handle_resume(state: &mut AppState) -> Result<()> {
    if state.rest.waiting {
        state.rest.status = "busy — wait for response".into();
        return Ok(());
    }
    match store::list_sessions() {
        Ok(sessions) => {
            state.mode = Mode::SessionPicker(PickerState::new(sessions));
        }
        Err(e) => {
            state.rest.status = format!("error listing sessions: {e}");
        }
    }
    Ok(())
}

/// Handle the `/rename <name>` command: rename the current session.
pub(super) fn handle_rename(state: &mut AppState, name: String) -> Result<()> {
    if name.trim().is_empty() {
        state.rest.status = "usage: /rename <name>".into();
        return Ok(());
    }
    if let Some(sess) = state.rest.session.as_mut() {
        match store::rename_session(sess, &name) {
            Ok(()) => state.rest.status = format!("renamed to {}", sess.name),
            Err(e) => state.rest.status = format!("error: {e}"),
        }
    }
    Ok(())
}
