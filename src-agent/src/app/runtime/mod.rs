//! Runtime: the synchronous event loop that ties the whole app together.
//!
//! Owns the terminal, the tokio runtime handle, and the `AppState`. Its job is
//! the central cycle: drain the active request's [`StreamEvent`]s -> read
//! terminal input -> turn keystrokes into `Action`s -> apply them by mutating
//! state -> redraw. This is the only place that spawns async tasks and the only
//! place that calls `view::draw`.
//!
//! Rendering is dirty-flagged (draw only after something changes) and input
//! polling is adaptive (8ms while a request streams so tokens flush at >=60fps,
//! 100ms when idle) so a quiet UI burns no CPU.
//!
//! Async bridge: one channel per request. [`start_stream_task`] opens a fresh
//! channel, stashes the receiver in `state.rest.active_rx`, and spawns a task
//! holding the sender. Cancelling (interrupt / `/new` / quit) just drops the
//! receiver, so a superseded task's late events vanish with no generation
//! bookkeeping.

mod terminal;
mod event_loop;
mod stream;
mod actions;
mod commands;
mod shortsend;

use std::io::stdout;
use std::sync::Arc;

use anyhow::Result;
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::app::mode::{KeyInputForm, LoadingState, Mode, PickerState, WarmStatus};
use crate::app::resolve::resolve_role;
use crate::app::state::AppState;
use crate::config::DEFAULT_MODEL;
use crate::model::app_config::ModelRole;
use crate::model::{app_config::AppConfig, settings::Settings, store};
use crate::service::openrouter::OpenRouterClient;
use crate::service::WarmEvent;

use terminal::TerminalGuard;
use event_loop::run_loop;

pub(super) type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Make the on-disk lock match the active session.
///
/// Releases the previously-held lock if the active session changed, then writes
/// a fresh `session.lock` for the current one. A no-op when the active session
/// is unchanged (so calling it on every activation is cheap and idempotent).
/// All lock IO is best-effort, so this never fails or blocks.
pub(super) fn reconcile_session_lock(state: &mut AppState) {
    let cur = state.rest.session.as_ref().map(|s| s.path.clone());
    if state.rest.held_lock == cur {
        return; // active session unchanged → on-disk lock already correct
    }
    // Drop the stale lock first so switching away from a session unlocks it.
    if let Some(old) = state.rest.held_lock.take() {
        crate::model::store::remove_lock(&old);
    }
    // Acquire the new session's lock (if there is an active session now).
    if let Some(new) = cur {
        crate::model::store::write_lock(&new);
        state.rest.held_lock = Some(new);
    }
}

/// Warm a newly-activated session to match a cold terminal launch: kick off a
/// background reindex of its workspace and (best-effort) compute the project
/// awareness summary. Safe to call whenever a session becomes the active one
/// (startup, /new, picker-select, creds-confirm). No-op if no session.
///
/// NON-BLOCKING: the awareness network call used to run via `handle.block_on` on
/// the UI thread BEFORE the event loop started, so a slow network froze the app on
/// a black screen. It is now SPAWNED as a background task (mirroring the endpoints
/// fetch), and — when there is awareness work to do — this switches the app into
/// [`Mode::Loading`], an animated splash the event loop renders while the task
/// runs. The task sends a [`WarmEvent::WarmAwareness`] on `warm_rx`, drained in
/// `run_loop` to populate the summary and advance the splash; once the awareness
/// step is terminal the loop enters Chat. This function returns immediately (it
/// only spawns), so startup never blocks.
///
/// The model catalogue is NO LONGER fetched here: it loads ON DEMAND, per
/// endpoint, the first time a model omnisearch needs it (see
/// `AppStateRest::request_catalogue` + the debounced tick in `event_loop`). So
/// when awareness is disabled or unroutable, there is no warm work and the mode is
/// left as-is (Chat) — no splash flash.
pub(super) fn warm_session(
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) {
    // Claim the lock for the now-active session (and release any prior one).
    // Cheap no-op when the active session is unchanged. Placed first so every
    // activation path that routes through warm_session — startup, /new,
    // picker-select, creds-confirm — acquires the lock.
    reconcile_session_lock(state);
    // Snapshot what we need, dropping the session borrow before mutating
    // `state.mode` / `state.rest`. `config` is cloned so the role resolution
    // below doesn't borrow `state` across the spawn.
    let (workdir, settings, workdirs) = match state.rest.session.as_ref() {
        Some(s) => (s.workdir(), s.settings.clone(), s.workdirs()),
        None => return,
    };
    let config = state.rest.config.clone();
    // Workspace reindex is already async (background thread); fire it always,
    // independent of whether we show the loading splash.
    crate::tool::dircache::reindex(workdirs, state.rest.dir_cache.clone());

    // Decide the warm work. Awareness runs only when the setting is on. It needs a
    // client AND a routable resolved route (an Anthropic-typed provider can't be
    // dispatched by the OpenAI-compatible client — native Anthropic is deferred).
    // "wanted but not routable" becomes a Skipped step (no task spawned).
    let want_awareness = settings.awareness_enabled;
    let aware_route = client.as_ref().and_then(|_| {
        if want_awareness {
            resolve_role(&config, &settings, ModelRole::Awareness).filter(|r| r.is_routable())
        } else {
            None
        }
    });

    // No awareness task to spawn → no splash; leave the mode as-is (Chat) so the
    // no-work case behaves exactly as before (no splash flash).
    if aware_route.is_none() {
        return;
    }

    state.mode = Mode::Loading(LoadingState {
        started: std::time::Instant::now(),
        frame: 0,
        workspace: WarmStatus::Running,
        awareness: WarmStatus::Running,
    });

    // One channel for the warm task; the receiver lives in state and is drained in
    // run_loop. Dropping it (e.g. app close) makes the sends no-ops, same contract
    // as the streaming / endpoints channels.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    state.rest.warm_rx = Some(rx);

    // Awareness task: read the depth-1 docs + summarize on the resolved Awareness
    // route. Move the owned route + the cloned settings/workdir in; `summarize`
    // returns `None` on no docs / failure, which the drain renders as the
    // appropriate terminal step.
    if let (Some(c), Some(route)) = (client.as_ref(), aware_route) {
        let c = Arc::clone(c);
        handle.spawn(async move {
            let summary = crate::app::awareness::summarize(
                &c,
                &settings,
                route.conn(),
                &route.model_id,
                route.provider(),
                &workdir,
            )
            .await;
            let _ = tx.send(WarmEvent::WarmAwareness(summary));
        });
    }
}

/// Build a fresh per-session client.
///
/// The client is now KEYLESS — it carries no creds/model/provider/effort, only
/// `http` + a fresh `plan_word`. So this is needed ONLY at session boundaries
/// (startup, `/new`, picker-select, creds-confirm, cancel paths) to re-roll the
/// cache-stable `plan_word`; it must NOT be called on a mid-session cred/effort
/// change, since those are read per-call via `resolve_role`. The `&Session`
/// param is gone — building doesn't depend on session state anymore.
pub(super) fn build_client() -> Arc<OpenRouterClient> {
    Arc::new(OpenRouterClient::new())
}

/// Best-effort prefill of (api_key, model, provider) from the most-recently-modified
/// session that has a non-empty key. Ignores all errors.
fn prefill_creds() -> (Option<String>, Option<String>, Option<String>) {
    let metas = match store::list_sessions() {
        Ok(m) => m,
        Err(_) => return (None, None, None),
    };
    let Some(meta) = metas.into_iter().next() else {
        return (None, None, None);
    };
    let settings = match Settings::load(&meta.path.join("settings.json")) {
        Ok(s) => s,
        Err(_) => return (None, None, None),
    };
    if settings.api_key.is_empty() {
        (None, None, None)
    } else {
        (Some(settings.api_key), Some(settings.model), Some(settings.provider))
    }
}

pub fn run(opts: crate::cli::Opts) -> Result<()> {
    store::ensure_dirs()?;

    let rt = tokio::runtime::Runtime::new()?;
    let handle = rt.handle().clone();

    // Decide initial state.
    let mut state = if opts.resume {
        let metas = store::list_sessions()?;
        let (lk, lm, lp) = prefill_creds();
        let mut state = AppState::new(Mode::SessionPicker(PickerState::new(metas)));
        state.rest.last_key = lk;
        state.rest.last_model = lm;
        state.rest.last_provider = lp;
        state
    } else {
        let (lk, lm, lp) = prefill_creds();
        let key_known = lk.as_deref().is_some_and(|k| !k.is_empty());
        let mut state = if key_known {
            // Returning user: spawn a fresh session pre-loaded with the last
            // creds and drop straight into chat. The credential prompt only
            // appears on the very first run. Per-session changes via /settings.
            let mut st = AppState::new(Mode::Chat);
            match store::create_session() {
                Ok(mut sess) => {
                    sess.settings.api_key = lk.clone().unwrap_or_default();
                    sess.settings.model =
                        lm.clone().unwrap_or_else(|| DEFAULT_MODEL.to_string());
                    sess.settings.provider = lp.clone().unwrap_or_default();
                    let _ = sess.save();
                    let sess_path = sess.path.clone();
                    st.rest.session = Some(sess);
                    // Fresh startup session → totals 0; harmless and explicit.
                    st.rest.load_token_totals(&sess_path);
                }
                Err(e) => {
                    // Couldn't create the session dir — fall back to the prompt.
                    st.rest.status = format!("error: {e}");
                    st.mode = Mode::KeyInput(KeyInputForm::prefilled(
                        lk.clone().unwrap_or_default(),
                        lm.clone().unwrap_or_else(|| DEFAULT_MODEL.to_string()),
                        true,
                        false,
                    ));
                }
            }
            st
        } else {
            // First ever run on this machine: prompt for credentials (lazy — no
            // session dir is created until the user confirms).
            AppState::new(Mode::KeyInput(KeyInputForm::prefilled(
                lk.clone().unwrap_or_default(),
                lm.clone().unwrap_or_else(|| DEFAULT_MODEL.to_string()),
                true,  // first_run
                false, // from_picker
            )))
        };
        state.rest.last_key = lk;
        state.rest.last_model = lm;
        state.rest.last_provider = lp;
        state
    };

    // Load global config now that ensure_dirs has run (so the dir exists if we
    // later write config.json). Falls back to AppConfig::default() on any error.
    state.rest.config = AppConfig::load();

    // Capture the process launch directory for the harness workspace check (WC).
    // This folder is always an allowed workspace regardless of the allow-list.
    if let Ok(cwd) = std::env::current_dir() {
        state.rest.launch_dir = cwd;
    }

    // Terminal setup. Guard created BEFORE the Terminal so its Drop covers a
    // failing Terminal::new, any later `?`-error, and panic-unwind.
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    // Clear the alternate screen so no shell scrollback bleeds through the
    // cells the UI never paints (e.g. the empty part of the transcript).
    terminal.clear()?;

    // If startup opened a session straight into chat (returning user), build its
    // client now; otherwise it's built when the user confirms credentials. The
    // None-gate is the "is there a usable session/key?" signal the whole runtime
    // relies on. Because the key now lives in config/settings (read per-call), the
    // condition is whether the MAIN role resolves to a usable route (a route with a
    // non-empty api_key) — NOT the old `!settings.api_key.is_empty()`. The client
    // itself is keyless; the gate just preserves the no-client-no-send invariant.
    let mut client: Option<Arc<OpenRouterClient>> = state
        .rest
        .session
        .as_ref()
        .filter(|s| {
            resolve_role(&state.rest.config, &s.settings, ModelRole::Main)
                .is_some_and(|r| !r.api_key.is_empty())
        })
        .map(|_| build_client());

    // Warm the session (reindex workspace + compute awareness summary) so a
    // cold launch is fully primed before the first keystroke. Picker / first-run
    // paths have no session yet; warm_session is a no-op for them and fires
    // later when a session becomes active (picker-select / creds-confirm / /new).
    warm_session(&mut state, &client, &handle);

    let result = run_loop(&mut terminal, &mut state, &handle, &mut client);

    // Terminal teardown is handled by `_guard`'s Drop at function scope.

    // Clean-exit unlock: release the lock we hold for the active session so the
    // session is immediately re-enterable. Runs on both the Ok and Err paths
    // (this is after run_loop returns either way). A crash that skips this is
    // covered by PID-liveness staleness in `store::is_locked`.
    if let Some(p) = state.rest.held_lock.take() {
        crate::model::store::remove_lock(&p);
    }

    // drop(rt) LAST: runtime shutdown cancels spawned tasks. Each task owns the
    // sender of its own per-request channel; once dropped here (or earlier when
    // its receiver in state was dropped), every send is a no-op. The `let _ =`
    // on each send makes this safe — no panic, no deadlock.
    drop(rt);

    result
}
