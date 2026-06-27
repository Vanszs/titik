//! New-session and resume/rename commands: `/new`, `/resume`, `/rename`.

use std::sync::Arc;

use anyhow::Result;

use crate::app::mode::{KeyInputForm, Mode, PickerState};
use crate::app::state::{AppState, SessionRuntime};
use crate::config::DEFAULT_MODEL;
use crate::model::store;
use crate::service::openrouter::OpenRouterClient;

/// Handle the `/new` command: SPAWN a fresh PARALLEL session.
///
/// The current foreground session is left UNTOUCHED — it keeps its lock, its
/// in-flight turn, and all of its execution state, still cooking in the
/// background in its own `sessions` slot. A brand-new [`Session`] is created
/// (inheriting last-used creds), given its OWN lock, wrapped in a fresh
/// [`SessionRuntime`], APPENDED to `state.rest.sessions`, and made the new
/// foreground. The flat foreground-UI fields (composer, scroll, attachments,
/// transcript cache, status) are reset for a clean slate on the new tab.
///
/// If no creds are known yet, this opens the KeyInput prompt for the new
/// session; cancelling it pops the just-appended session back off (the
/// `handle_cancel_key_input` action keys off the `spawn_pending` flag set here).
///
/// INVARIANT (this stage): `sessions` is only ever APPENDED to here and never
/// reordered/removed (in-flight async routes by Vec index), and the previous
/// foreground's lock is NEVER released — every live session holds its own lock
/// for its whole lifetime.
pub(super) fn handle_new(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
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

    // Acquire THIS session's lock immediately — every live session holds its own
    // lock for its lifetime. Build a fresh runtime that owns the session + lock.
    store::write_lock(&sess.path);
    let mut runtime = SessionRuntime::new();
    runtime.held_lock = Some(sess.path.clone());
    let no_creds = sess.settings.api_key.is_empty();
    runtime.session = Some(sess);

    // Remember where to return if the (creds-less) KeyInput below is cancelled,
    // then APPEND the new runtime and make it the foreground. The old foreground
    // stays live in its own slot, lock held, still cooking.
    state.rest.spawn_prev_fg = state.rest.foreground;
    state.rest.sessions.push(runtime);
    state.rest.foreground = state.rest.sessions.len() - 1;

    // Reset the FLAT foreground-UI fields (shared on AppStateRest) for a clean
    // slate on the new tab: empty composer + caret, pinned-to-bottom scroll, no
    // staged attachments, and a fresh (empty) transcript so the new conversation
    // renders instead of the previous tab's cached blocks.
    state.rest.input.clear();
    state.rest.cursor = 0;
    state.rest.reset_scroll();
    state.rest.pending_attachments.clear();
    state.rest.transcript_cache.borrow_mut().blocks.clear();
    state.rest.status = "ready".into();
    // Fresh session → seed ITS OWN counters from its (empty) ledger, i.e. 0. No
    // global counter to reset: the new session carries its own totals, and the
    // previous foreground keeps its counters intact in its own slot.
    let new_fg = state.rest.foreground;
    let sess_path = state
        .rest
        .fg()
        .session
        .as_ref()
        .map(|s| s.path.clone());
    if let Some(p) = sess_path.as_ref() {
        state.rest.load_token_totals(new_fg, p);
    }

    if no_creds {
        // No creds known yet — fall back to the credential prompt FOR THE NEW
        // SESSION. Mark it spawn-pending so an Esc out of KeyInput pops this
        // freshly-appended session and restores the previous foreground.
        *client = None;
        state.rest.spawn_pending = true;
        state.mode = Mode::KeyInput(KeyInputForm::prefilled(
            String::new(),
            DEFAULT_MODEL.to_string(),
            false, // Esc -> CancelKeyInput (which pops the spawned session)
            false, // not from picker
        ));
    } else {
        state.rest.spawn_pending = false;
        *client = Some(super::super::build_client());
        // Land in Chat first, THEN warm: `warm_session` is non-blocking and
        // may upgrade the mode to `Mode::Loading` (animated splash) when it
        // has warm work to spawn, so it must run LAST to get the final word.
        // With no warm work it leaves the mode as the Chat we just set.
        state.mode = Mode::Chat;
        // Warm the new foreground session: reindex its workspace + (async) fetch
        // the catalogue and awareness summary so /new is primed like a cold boot.
        // `warm_session` -> `reconcile_session_lock` only ever touches the
        // foreground (new) session's lock, which already matches its on-disk lock
        // we just wrote — so it is a no-op for locks and never releases the
        // previous foreground's lock.
        super::super::warm_session(state, client, handle);
    }
    Ok(())
}

/// Handle the `/resume` command: open the session picker.
///
/// Unlike CancelKeyInputToPicker we do NOT clear the current session/client —
/// if the user Escapes the picker they return to the active chat unchanged.
pub(super) fn handle_resume(state: &mut AppState) -> Result<()> {
    if state.rest.fg().waiting {
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
    if let Some(sess) = state.rest.fg_mut().session.as_mut() {
        match store::rename_session(sess, &name) {
            Ok(()) => state.rest.status = format!("renamed to {}", sess.name),
            Err(e) => state.rest.status = format!("error: {e}"),
        }
    }
    Ok(())
}
