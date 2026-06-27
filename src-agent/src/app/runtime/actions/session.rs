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
    // Edge case (/new parallel spawn): this KeyInput was for a brand-new
    // PARALLEL session that was already appended to `sessions` and made the
    // foreground. The user bailed before entering creds — pop that empty
    // session back off, release its lock, and restore the previous foreground.
    // We do NOT touch the previous foreground's session/lock here (it never
    // moved). Returns straight to the (restored) foreground's Chat.
    if state.rest.spawn_pending {
        state.rest.spawn_pending = false;
        // Pop the just-appended session (it is always the LAST entry: KeyInput is
        // modal, so nothing could have appended after it).
        if state.rest.sessions.len() > 1 {
            let mut popped = state.rest.sessions.pop().expect("len > 1 checked");
            if let Some(lock) = popped.held_lock.take() {
                store::remove_lock(&lock);
            }
        }
        // Restore the foreground to where /new left from, clamped defensively.
        state.rest.foreground = state
            .rest
            .spawn_prev_fg
            .min(state.rest.sessions.len().saturating_sub(1));
        // Rebuild the client for the restored foreground (fresh plan_word at this
        // boundary); keep it only when that session has a usable Main route.
        let restored_usable = state
            .rest
            .fg()
            .session
            .as_ref()
            .map(|s| s.settings.clone())
            .is_some_and(|settings| {
                crate::app::resolve::resolve_role(
                    &state.rest.config,
                    &settings,
                    crate::model::app_config::ModelRole::Main,
                )
                .is_some_and(|r| !r.api_key.is_empty())
            });
        *client = if restored_usable {
            Some(build_client())
        } else {
            None
        };
        // Reset the flat foreground-UI for the restored tab + invalidate the
        // transcript cache so its conversation (not the popped empty one) renders.
        state.rest.input.clear();
        state.rest.cursor = 0;
        state.rest.reset_scroll();
        state.rest.pending_attachments.clear();
        state.rest.transcript_cache.borrow_mut().blocks.clear();
        // No token reseed on this switch-back: the restored foreground carries its
        // OWN per-session counters in its slot (untouched while it sat in the
        // background), so we just render fg()'s. Reseeding from sqlite here would
        // clobber a mid-turn `tokens_in` (current context) with the cumulative sum.
        // The restored foreground already holds its own lock (untouched); no
        // reconcile needed (and reconcile must not release any other session's lock).
        state.mode = Mode::Chat;
        state.rest.status = if client.is_some() {
            "ready".into()
        } else {
            "no active session".into()
        };
        return Ok(());
    }
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
    // A picker-launched KeyInput is never a /new spawn, but clear the flag
    // defensively so it can't leak into a later cancel.
    state.rest.spawn_pending = false;
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

/// Handle `Action::LiveSwitch`: switch the foreground to the live session at
/// `idx` (`/swap`'s Enter). Sets `foreground = idx` and resets the FLAT
/// foreground-UI for the newly-shown session, WITHOUT aborting anything and
/// WITHOUT touching any lock — every live session keeps its own lock, and the
/// target's in-flight stream (if any) keeps appearing live once it's on screen.
pub(super) fn handle_live_switch(
    idx: usize,
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
) -> Result<()> {
    // Defensive: ignore an out-of-range index (the snapshot is built from the
    // live `sessions`, and `sessions` is only ever appended to this stage, so a
    // stale index can't normally occur — but never panic).
    if idx >= state.rest.sessions.len() {
        state.mode = Mode::Chat;
        state.rest.status = "session unavailable".into();
        return Ok(());
    }
    state.rest.foreground = idx;
    // Reset the flat foreground-UI for the newly-shown session: empty composer +
    // caret, pinned-to-bottom scroll, no staged attachments, and a fresh (empty)
    // transcript cache so the target's conversation renders instead of the
    // previous tab's cached blocks.
    state.rest.input.clear();
    state.rest.cursor = 0;
    state.rest.reset_scroll();
    state.rest.pending_attachments.clear();
    state.rest.transcript_cache.borrow_mut().blocks.clear();
    // NO token reseed on /swap: each session now carries its OWN token/cost
    // counters in its slot, so switching foreground just renders fg()'s — never
    // the previous tab's, never a sum. (Reseeding from sqlite here would also
    // clobber the target's mid-turn `tokens_in` with its cumulative ledger sum.)
    // KEYLESS client → rebuild for a fresh plan_word at this session boundary,
    // gated on the target having a usable Main route (preserve no-client-no-send).
    let usable = state
        .rest
        .fg()
        .session
        .as_ref()
        .map(|s| s.settings.clone())
        .is_some_and(|settings| {
            crate::app::resolve::resolve_role(
                &state.rest.config,
                &settings,
                crate::model::app_config::ModelRole::Main,
            )
            .is_some_and(|r| !r.api_key.is_empty())
        });
    *client = if usable { Some(build_client()) } else { None };
    // Reflect the now-foreground session's live state in the status line.
    state.rest.status = if state.rest.fg().is_working() {
        "working".into()
    } else {
        "ready".into()
    };
    state.mode = Mode::Chat;
    Ok(())
}

/// Handle `Action::LiveSwitchCancel`: discard the `/swap` picker and return to
/// the unchanged Chat view. No session state, foreground, or lock is touched.
pub(super) fn handle_live_switch_cancel(state: &mut AppState) -> Result<()> {
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
        // Existing session: seed THIS (foreground) session's own counters from its
        // full sqlite log so the readout reflects prior usage.
        let fg = state.rest.foreground;
        state.rest.load_token_totals(fg, &sess_path);
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
