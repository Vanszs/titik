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

use std::io::stdout;
use std::sync::Arc;

use anyhow::Result;
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::app::mode::{KeyInputForm, Mode, PickerState};
use crate::app::state::AppState;
use crate::config::{DEFAULT_BASE_URL, DEFAULT_MODEL};
use crate::model::{app_config::AppConfig, session::Session, settings::Settings, store};
use crate::service::openrouter::OpenRouterClient;

use terminal::TerminalGuard;
use event_loop::run_loop;

pub(super) type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Build a client from a session's settings.
pub(super) fn build_client(s: &Session) -> Arc<OpenRouterClient> {
    Arc::new(OpenRouterClient::new(
        s.settings.api_key.clone(),
        DEFAULT_BASE_URL.to_string(),
        s.settings.model.clone(),
        s.settings.provider.clone(),
    ))
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
                        lp.clone().unwrap_or_default(),
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
                lp.clone().unwrap_or_default(),
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

    // If a session is already active (returning-user / startup-create path),
    // kick off a background index of its workspace so the file cache is warm.
    // Picker / first-run paths have no session yet; they trigger this later.
    if let Some(sess) = state.rest.session.as_ref() {
        crate::tool::dircache::reindex(sess.workdir(), state.rest.dir_cache.clone());
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
    // client now; otherwise it's built when the user confirms credentials.
    let mut client: Option<Arc<OpenRouterClient>> = state
        .rest
        .session
        .as_ref()
        .filter(|s| !s.settings.api_key.is_empty())
        .map(build_client);

    // Project self-awareness (Phase 2): if a session + client are already live
    // at startup, summarise the project's docs via a cheap secondary model and
    // stash it for injection into the system prompt. Best-effort and gated by
    // `awareness_enabled` inside `summarize`; a quick small-model `block_on`
    // here is acceptable (it never hard-blocks once the call returns/errors).
    // Picker / first-run paths have no session yet; they get a summary lazily
    // on the next trigger (post-`/compact`) instead.
    if let (Some(c), Some(sess)) = (client.as_ref(), state.rest.session.as_ref()) {
        if sess.settings.awareness_enabled {
            let summary = handle.block_on(crate::app::awareness::summarize(
                c,
                &sess.settings,
                &sess.workdir(),
            ));
            state.rest.awareness_summary = summary;
        }
    }

    let result = run_loop(&mut terminal, &mut state, &handle, &mut client);

    // Terminal teardown is handled by `_guard`'s Drop at function scope.

    // drop(rt) LAST: runtime shutdown cancels spawned tasks. Each task owns the
    // sender of its own per-request channel; once dropped here (or earlier when
    // its receiver in state was dropped), every send is a no-op. The `let _ =`
    // on each send makes this safe — no panic, no deadlock.
    drop(rt);

    result
}
