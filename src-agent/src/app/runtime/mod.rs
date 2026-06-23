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
use crate::model::{session::Session, settings::Settings, store};
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
        // Lazy creation: do NOT create a session dir yet. It is created in the
        // SaveCreds handler once the user actually confirms credentials, so an
        // aborted first run (Esc/Quit) or a terminal-setup failure leaves no
        // orphaned empty session directory on disk.
        let mut state = AppState::new(Mode::KeyInput(KeyInputForm::prefilled(
            lk.clone().unwrap_or_default(),
            lm.clone().unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            lp.clone().unwrap_or_default(),
            true,  // first_run
            false, // from_picker
        )));
        state.rest.last_key = lk;
        state.rest.last_model = lm;
        state.rest.last_provider = lp;
        state
    };

    // Terminal setup. Guard created BEFORE the Terminal so its Drop covers a
    // failing Terminal::new, any later `?`-error, and panic-unwind.
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut client: Option<Arc<OpenRouterClient>> = None;

    let result = run_loop(&mut terminal, &mut state, &handle, &mut client);

    // Terminal teardown is handled by `_guard`'s Drop at function scope.

    // drop(rt) LAST: runtime shutdown cancels spawned tasks. Each task owns the
    // sender of its own per-request channel; once dropped here (or earlier when
    // its receiver in state was dropped), every send is a no-op. The `let _ =`
    // on each send makes this safe — no panic, no deadlock.
    drop(rt);

    result
}
