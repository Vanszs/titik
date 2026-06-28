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

/// Handle `Action::PickerSelect`: NON-DESTRUCTIVE `/resume` selection.
///
/// The disk session picker (`/resume`) used to ABORT the current foreground and
/// replace it in-place with the picked session — fatal under multi-session, since
/// it nuked a live tab. This is now APPEND-OR-SWAP and the current foreground is
/// NEVER aborted, never loses its lock, and keeps cooking in its own slot:
///
/// 1. **Already open in THIS process** (a `sessions` slot's `session.path`
///    matches the picked path) → just SWAP foreground to it (`/swap`'s flat-UI
///    reset), no reload, no lock churn.
/// 2. **Locked by ANOTHER live process** → refuse (a lock held by US is always
///    covered by case 1, so a still-live lock here is necessarily another PID).
/// 3. **Free to load** → load the [`Session`] from disk, hydrate a FRESH
///    [`SessionRuntime`], acquire ITS lock, APPEND to `sessions`, make it the
///    foreground (`/new`'s flat-UI reset + warm). The previous foreground stays
///    live in its slot, lock held.
///
/// INVARIANT (this stage): `sessions` is only ever APPENDED to + the foreground
/// changes — never reordered/removed — and no other session's lock is released.
pub(super) fn handle_picker_select(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    // Guard: /resume can't append onto an unstable session tail while a /new
    // KeyInput is pending confirmation. The modal state should prevent this,
    // but make the invariant explicit.
    if state.rest.spawn_pending {
        return Ok(());
    }

    // Extract selected path first (borrow of mode released before
    // mutating rest/mode below). The picked path is the canonical identity:
    // a session dir is `sessions/<pwd_hash>/<uuid>`, so equal paths ⇒ equal id,
    // and every lock/load API here is already path-keyed.
    let path = match &state.mode {
        Mode::SessionPicker(p) => p.selected_meta().map(|m| m.path.clone()),
        _ => None,
    };
    let Some(path) = path else {
        state.rest.status = "no session selected".into();
        return Ok(());
    };

    // --- Case 1: already open as a live tab in THIS process → SWAP, don't reload.
    // Match on the on-disk session path (stable identity). If found, behave exactly
    // like `/swap` (handle_live_switch): set foreground, reset the flat foreground-UI,
    // rebuild the keyless client for the target's route, status from is_working().
    if let Some(idx) = state
        .rest
        .sessions
        .iter()
        .position(|rt| rt.session.as_ref().map(|s| &s.path) == Some(&path))
    {
        state.rest.foreground = idx;
        // Flat foreground-UI reset for the now-shown tab (mirror handle_live_switch):
        // empty composer + caret, pinned-to-bottom scroll, no staged attachments, and
        // a fresh transcript cache so the target's conversation renders instead of the
        // previous tab's cached blocks. No token reseed — each slot owns its counters.
        state.rest.input.clear();
        state.rest.cursor = 0;
        state.rest.reset_scroll();
        state.rest.pending_attachments.clear();
        state.rest.transcript_cache.borrow_mut().blocks.clear();
        // KEYLESS client → rebuild for a fresh plan_word at this session boundary,
        // gated on the target having a usable Main route (no-client-no-send).
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
        state.rest.status = if state.rest.fg().is_working() {
            "working".into()
        } else {
            "ready".into()
        };
        state.mode = Mode::Chat;
        return Ok(());
    }

    // --- Case 2: not in our process but locked by a LIVE process → refuse.
    // Re-check the lock live (don't trust the cached row flag) so a race — the
    // session getting opened elsewhere after the list was built — can't slip
    // through. Since case 1 already ruled out OUR own tabs, any live lock here is
    // necessarily another process's (`is_locked` does the PID-liveness check and
    // sweeps stale locks). Stay in the picker; the row already shows the marker.
    if store::is_locked(&path) {
        state.rest.status = "session in use by another process".into();
        return Ok(());
    }

    // --- Case 3: free to load → APPEND a fresh tab; previous foreground stays live.
    let sess = match Session::load(&path) {
        Ok(s) => s,
        Err(e) => {
            state.rest.status = format!("error: {e}");
            return Ok(());
        }
    };

    // Acquire THIS session's lock immediately — every live session holds its own
    // lock for its lifetime. Build a fresh runtime that owns the loaded session +
    // lock, then APPEND it and make it the foreground. The OLD foreground keeps its
    // own slot, lock, and in-flight turn (we never abort or take it).
    store::write_lock(&sess.path);
    let mut runtime = crate::app::state::SessionRuntime::new();
    runtime.held_lock = Some(sess.path.clone());
    let no_creds = sess.settings.api_key.is_empty();
    let sess_path = sess.path.clone();
    runtime.session = Some(sess);

    // Remember where to return if the (creds-less) KeyInput below is cancelled,
    // then APPEND + make foreground.
    state.rest.spawn_prev_fg = state.rest.foreground;
    state.rest.sessions.push(runtime);
    state.rest.foreground = state.rest.sessions.len() - 1;

    // Reset the flat foreground-UI for a clean slate on the new tab (mirror /new):
    // empty composer + caret, pinned-to-bottom scroll, no staged attachments (so the
    // previous session's images don't leak in), fresh transcript cache.
    state.rest.input.clear();
    state.rest.cursor = 0;
    state.rest.reset_scroll();
    state.rest.pending_attachments.clear();
    state.rest.transcript_cache.borrow_mut().blocks.clear();
    state.rest.status = "ready".into();

    // Existing session: seed THIS (new foreground) slot's OWN counters from its full
    // sqlite log so the readout reflects prior usage. Never touches another slot.
    let new_fg = state.rest.foreground;
    state.rest.load_token_totals(new_fg, &sess_path);

    if no_creds {
        // Loaded session has no creds — prompt FOR THE NEW (appended) SESSION. Mark
        // it spawn-pending and open KeyInput with from_picker = false so Esc routes
        // to CancelKeyInput, whose spawn_pending branch POPS this just-appended tab,
        // releases its lock, and restores the previous foreground (reusing /new's
        // proven cancel machinery — leaving a valid foreground either way).
        let lk = state.rest.last_key.clone().unwrap_or_default();
        let lm = state
            .rest
            .last_model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        *client = None;
        state.rest.spawn_pending = true;
        state.mode = Mode::KeyInput(KeyInputForm::prefilled(lk, lm, false, false));
    } else {
        state.rest.spawn_pending = false;
        let (key, model, provider) = state
            .rest
            .fg()
            .session
            .as_ref()
            .map(|s| {
                (
                    s.settings.api_key.clone(),
                    s.settings.model.clone(),
                    s.settings.provider.clone(),
                )
            })
            .unwrap_or_default();
        state.rest.remember_creds(&key, &model, &provider);
        // KEYLESS client → fresh plan_word at this session boundary. This branch
        // already gated on a non-empty key above, so build directly.
        *client = Some(build_client());
        // Land in Chat first, THEN warm: `warm_session` is non-blocking and may
        // upgrade the mode to `Mode::Loading` (animated splash) when it has warm
        // work to spawn, so it must run LAST. With no warm work it leaves the mode
        // as the Chat we just set. warm_session -> reconcile_session_lock only ever
        // touches the (new) foreground's lock, which already matches the on-disk
        // lock we just wrote — a no-op for locks; no other session's lock is freed.
        state.mode = Mode::Chat;
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
