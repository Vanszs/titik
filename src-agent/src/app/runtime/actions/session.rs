//! Action handlers for session lifecycle: CancelKeyInput, CancelKeyInputToPicker,
//! CancelPickerToChat, PickerSelect, SkipLoading.

use std::sync::Arc;

use anyhow::Result;

use crate::app::mode::{KeyInputForm, Mode, PickerState};
use crate::app::state::AppState;
use crate::config::DEFAULT_MODEL;
use crate::model::{session::Session, store};
use crate::service::openrouter::OpenRouterClient;

use crate::app::runtime::build_client;

/// Handle `Action::CancelKeyInput`: restore the previous session (or rebuild
/// the client from the current one) and return to Chat.
pub(super) fn handle_cancel_key_input(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
) -> Result<()> {
    // KEYLESS client → build for a fresh plan_word at this session boundary;
    // gate on whether the restored session's MAIN role resolves to a usable
    // route (non-empty key), preserving the no-client-no-send invariant.
    let usable = |state: &AppState, settings: &crate::model::settings::Settings| {
        crate::app::resolve::resolve_role(
            &state.rest.config,
            settings,
            crate::model::app_config::ModelRole::Main,
        )
        .is_some_and(|r| !r.api_key.is_empty())
    };
    if let Some(prev) = state.rest.prev_session.take() {
        *client = if usable(state, &prev.settings) {
            Some(build_client())
        } else {
            None
        };
        state.rest.fg_mut().session = Some(prev);
    } else if let Some(settings) = state.rest.fg().session.as_ref().map(|s| s.settings.clone()) {
        // Defensive: no stashed prev; rebuild from current session.
        *client = if usable(state, &settings) {
            Some(build_client())
        } else {
            None
        };
    }
    // Restoring prev_session here bypasses warm_session, so reconcile the
    // lock directly: release the lock for the session we were configuring
    // and re-acquire the restored one's.
    super::super::reconcile_session_lock(state);
    state.rest.reset_scroll();
    state.mode = Mode::Chat;
    if client.is_none() {
        state.rest.status = "no active session".into();
    } else {
        state.rest.status = "ready".into();
    }
    Ok(())
}

/// Handle `Action::CancelKeyInputToPicker`: drop the partially-set session,
/// clear the client, and return to the session picker.
pub(super) fn handle_cancel_key_input_to_picker(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
) -> Result<()> {
    // Esc out of a picker-launched KeyInput: drop the partially-set
    // session, clear any client, and return to the session picker
    // instead of pinning a no-client Chat.
    state.rest.fg_mut().session = None;
    state.rest.prev_session = None;
    *client = None;
    state.rest.reset_scroll();
    state.mode = Mode::SessionPicker(PickerState::new(store::list_sessions()?));
    state.rest.status = "ready".into();
    Ok(())
}

/// Handle `Action::CancelPickerToChat`: return to Chat without touching any
/// session state.
pub(super) fn handle_cancel_picker_to_chat(state: &mut AppState) -> Result<()> {
    // Esc/Ctrl+C in the /resume-opened session picker: the active
    // session is still in state.rest.fg().session (untouched), so just
    // swap the mode back to Chat without disturbing anything else.
    state.mode = Mode::Chat;
    Ok(())
}

/// Handle `Action::PickerSelect`: load the selected session (prompting for
/// creds if missing) and transition to Chat or KeyInput.
pub(super) fn handle_picker_select(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    // Extract selected path first (borrow of mode released before
    // mutating rest/mode below).
    let path = match &state.mode {
        Mode::SessionPicker(p) => p.selected_meta().map(|m| m.path.clone()),
        _ => None,
    };
    let Some(path) = path else {
        state.rest.status = "no session selected".into();
        return Ok(());
    };
    // Re-check the lock live (don't trust the cached row flag) so a race
    // — the session getting opened elsewhere after the list was built —
    // can't slip through. If it's locked by a live process, refuse to
    // enter and stay in the picker; the row already shows the marker.
    if store::is_locked(&path) {
        state.rest.status = "session is open — can't enter".into();
        return Ok(());
    }
    let sess = match Session::load(&path) {
        Ok(s) => s,
        Err(e) => {
            state.rest.status = format!("error: {e}");
            return Ok(());
        }
    };
    if sess.settings.api_key.is_empty() {
        // Prefill from remembered creds; do NOT overwrite them.
        let lk = state.rest.last_key.clone().unwrap_or_default();
        let lm = state
            .rest
            .last_model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        state.rest.fg_mut().session = Some(sess);
        state.rest.reset_scroll();
        state.mode = Mode::KeyInput(KeyInputForm::prefilled(lk, lm, false, true));
    } else {
        state
            .rest
            .remember_creds(&sess.settings.api_key, &sess.settings.model, &sess.settings.provider);
        // KEYLESS client → fresh plan_word at this session boundary. This
        // branch already gated on a non-empty key above, so build directly.
        *client = Some(build_client());
        // Drop any staged image attachments from the previous session so they
        // don't leak into the newly-selected one.
        state.rest.pending_attachments.clear();
        let sess_path = sess.path.clone();
        state.rest.fg_mut().session = Some(sess);
        // Existing session: seed the running totals from its full sqlite
        // log so the readout reflects prior usage.
        state.rest.load_token_totals(&sess_path);
        state.rest.reset_scroll();
        // Land in Chat first, THEN warm: `warm_session` is non-blocking and
        // may upgrade the mode to `Mode::Loading` (animated splash) when it
        // has warm work to spawn, so it must run LAST to get the final word.
        // With no warm work it leaves the mode as the Chat we just set.
        state.mode = Mode::Chat;
        state.rest.status = "ready".into();
        // Warm the selected session: reindex its workspace + (async) fetch
        // the catalogue and awareness summary so picker-resume is primed
        // like a cold boot.
        super::super::warm_session(state, client, handle);
    }
    Ok(())
}

/// Handle `Action::SkipLoading`: dismiss the loading splash and drop straight
/// into Chat, leaving the background warm tasks running.
pub(super) fn handle_skip_loading(state: &mut AppState) -> Result<()> {
    // Esc on the loading splash: drop straight into Chat. The warm tasks
    // keep running in the background and their results still populate
    // `state.rest.*` via the `warm_rx` drain (the receiver is untouched
    // here). The session/chat state was already set up by the activation
    // path that opened the splash, so we only swap the mode.
    state.mode = Mode::Chat;
    state.rest.status = "ready".into();
    Ok(())
}
