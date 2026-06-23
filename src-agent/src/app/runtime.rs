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

use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;

use crate::app::mode::{KeyInputForm, Mode, PickerState};
use crate::app::state::{AppState, AppStateRest};
use crate::config::{DEFAULT_BASE_URL, DEFAULT_MODEL};
use crate::controller::{self, command::Command, input::Action};
use crate::dto::chat::{ChatMessage, Role};
use crate::model::{session::Session, settings::Settings, store};
use crate::service::{openrouter::OpenRouterClient, StreamEvent};
use crate::view;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// RAII guard for terminal state. Entering enables raw mode + the alternate
/// screen; dropping (normal return, `?`-error after creation, or panic-unwind)
/// always restores the terminal.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        if let Err(e) = execute!(stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), crossterm::cursor::Show);
    }
}

/// Build a client from a session's settings.
fn build_client(s: &Session) -> Arc<OpenRouterClient> {
    Arc::new(OpenRouterClient::new(
        s.settings.api_key.clone(),
        DEFAULT_BASE_URL.to_string(),
        s.settings.model.clone(),
    ))
}

/// Best-effort prefill of (api_key, model) from the most-recently-modified
/// session that has a non-empty key. Ignores all errors.
fn prefill_creds() -> (Option<String>, Option<String>) {
    let metas = match store::list_sessions() {
        Ok(m) => m,
        Err(_) => return (None, None),
    };
    let Some(meta) = metas.into_iter().next() else {
        return (None, None);
    };
    let settings = match Settings::load(&meta.path.join("settings.json")) {
        Ok(s) => s,
        Err(_) => return (None, None),
    };
    if settings.api_key.is_empty() {
        (None, None)
    } else {
        (Some(settings.api_key), Some(settings.model))
    }
}

pub fn run(opts: crate::cli::Opts) -> Result<()> {
    store::ensure_dirs()?;

    let rt = tokio::runtime::Runtime::new()?;
    let handle = rt.handle().clone();

    // Decide initial state.
    let mut state = if opts.resume {
        let metas = store::list_sessions()?;
        let (lk, lm) = prefill_creds();
        let mut state = AppState::new(Mode::SessionPicker(PickerState::new(metas)));
        state.rest.last_key = lk;
        state.rest.last_model = lm;
        state
    } else {
        let (lk, lm) = prefill_creds();
        // Lazy creation: do NOT create a session dir yet. It is created in the
        // SaveCreds handler once the user actually confirms credentials, so an
        // aborted first run (Esc/Quit) or a terminal-setup failure leaves no
        // orphaned empty session directory on disk.
        let mut state = AppState::new(Mode::KeyInput(KeyInputForm::prefilled(
            lk.clone().unwrap_or_default(),
            lm.clone().unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            true,  // first_run
            false, // from_picker
        )));
        state.rest.last_key = lk;
        state.rest.last_model = lm;
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

/// The central event loop. Each tick: redraw if dirty, drain the active
/// request's events, then drain all buffered terminal input. Rendering is
/// dirty-flagged and polling is adaptive (8ms streaming / 100ms idle) so an
/// idle UI is effectively free while streaming stays at >=60fps.
fn run_loop(
    terminal: &mut Term,
    state: &mut AppState,
    handle: &tokio::runtime::Handle,
    client: &mut Option<Arc<OpenRouterClient>>,
) -> Result<()> {
    let mut dirty = true; // paint once on entry
    loop {
        if dirty {
            terminal.draw(|f| view::draw(f, state))?;
            dirty = false;
        }

        // 1. Drain the active stream's events. take() the receiver so the match
        //    arms can mutate other fields of state.rest without a borrow
        //    conflict; put it back if the stream is still open.
        if let Some(mut rx) = state.rest.active_rx.take() {
            let mut still_streaming = true;
            while let Ok(event) = rx.try_recv() {
                dirty = true;
                match event {
                    StreamEvent::Token(t) => {
                        state.rest.append_token(&t);
                        state.rest.status = "streaming".into();
                    }
                    StreamEvent::Done => {
                        finish_stream(&mut state.rest, None);
                        still_streaming = false;
                        break;
                    }
                    StreamEvent::Error(e) => {
                        finish_stream(&mut state.rest, Some(e));
                        still_streaming = false;
                        break;
                    }
                    StreamEvent::Compacted { summary, kept_tail } => {
                        if let Some(sess) = state.rest.session.as_mut() {
                            sess.conversation.apply_compaction(summary, kept_tail);
                            let _ = sess.save();
                        }
                        state.rest.waiting = false;
                        state.rest.current_task = None;
                        state.rest.status = "ready".into();
                        still_streaming = false;
                        break;
                    }
                }
            }
            if still_streaming {
                state.rest.active_rx = Some(rx);
            }
        }

        // 2. Input. 8ms poll while streaming so tokens flush at >=60fps; 100ms
        //    idle (poll still wakes instantly on a keypress, so typing latency
        //    is 0). Drain EVERY buffered event each tick so paste / fast typing
        //    don't lag.
        let timeout = if state.rest.waiting {
            Duration::from_millis(8)
        } else {
            Duration::from_millis(100)
        };
        if event::poll(timeout)? {
            while event::poll(Duration::ZERO)? {
                match event::read()? {
                    Event::Key(key) => {
                        let action = controller::input::handle_key(state, key);
                        apply_action(action, state, client, handle)?;
                        dirty = true;
                    }
                    Event::Resize(_, _) => dirty = true,
                    _ => {}
                }
            }
        }

        if state.rest.should_quit {
            break;
        }
    }
    Ok(())
}

/// Finalize a finished stream: commit any buffered assistant text, clear the
/// waiting flag + task handle, set the status line. `error` is Some on stream
/// failure; a save error is surfaced only if the stream itself succeeded.
fn finish_stream(rest: &mut AppStateRest, error: Option<String>) {
    let mut save_err = None;
    if let Some(buf) = rest.take_stream() {
        if !buf.is_empty() {
            if let Some(sess) = rest.session.as_mut() {
                sess.conversation.push_assistant(buf);
                if let Err(e) = sess.save() {
                    save_err = Some(e.to_string());
                }
            }
        }
    }
    rest.waiting = false;
    rest.current_task = None;
    rest.status = match error.or(save_err) {
        Some(e) => format!("error: {e}"),
        None => "ready".into(),
    };
}

/// Abort the in-flight task and stop listening to it: aborts the task handle,
/// drops the active receiver (so any late events from the task vanish), and
/// clears the waiting flag.
fn abort_current(rest: &mut AppStateRest) {
    if let Some(h) = rest.current_task.take() {
        h.abort();
    }
    rest.active_rx = None;
    rest.waiting = false;
}

/// Spawn a streaming task for `history`. Opens a fresh channel, stashes the
/// receiver in state, and hands the sender to the task — so this request's
/// events are isolated from any previous one (no generation tagging needed).
fn start_stream_task(
    history: Vec<ChatMessage>,
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    state.rest.active_rx = Some(rx);
    let c = Arc::clone(client.as_ref().unwrap());
    let jh = handle.spawn(async move {
        let _ = c.stream_complete(history, tx).await;
    });
    state.rest.current_task = Some(jh.abort_handle());
}

/// Apply one `Action` (the decoded result of a keystroke) by mutating state and,
/// where needed, spawning/aborting the request task.
fn apply_action(
    action: Action,
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    match action {
        Action::None => {}

        Action::Quit => {
            if state.rest.waiting {
                abort_current(&mut state.rest);
            }
            state.rest.should_quit = true;
        }

        Action::Submit(text) => {
            if client.is_none() || state.rest.session.is_none() {
                state.rest.status = "no active session".into();
                return Ok(());
            }
            if state.rest.waiting {
                state.rest.status = "busy — wait for response".into();
                return Ok(());
            }
            let history = {
                let sess = state.rest.session.as_mut().unwrap();
                sess.conversation.push_user(text);
                if let Err(e) = sess.save() {
                    state.rest.status = format!("error: {e}");
                    return Ok(());
                }
                sess.conversation.history()
            };
            state.rest.begin_stream();
            state.rest.waiting = true;
            state.rest.status = "thinking...".into();
            start_stream_task(history, state, client, handle);
        }

        Action::Slash(cmd) => {
            apply_slash(cmd, state, client, handle)?;
        }

        Action::Interrupt => {
            // Custom finalization (not finish_stream): the partial buffer is
            // committed with an "  [interrupted]" marker. abort_current drops
            // active_rx, so the aborted task's late events are ignored.
            if state.rest.waiting {
                abort_current(&mut state.rest);
                let buf = state.rest.take_stream();
                if let Some(b) = buf {
                    if !b.is_empty() {
                        if let Some(sess) = state.rest.session.as_mut() {
                            sess.conversation.push_assistant(format!("{b}  [interrupted]"));
                            let _ = sess.save();
                        }
                    }
                }
            }
            state.rest.status = "interrupted".into();
        }

        Action::Resend => {
            if state.rest.waiting {
                state.rest.status = "busy — wait for response".into();
                return Ok(());
            }
            if client.is_none() || state.rest.session.is_none() {
                state.rest.status = "no active session".into();
                return Ok(());
            }
            let history = {
                let sess = state.rest.session.as_mut().unwrap();
                if sess.conversation.last_user_content().is_none() {
                    state.rest.status = "nothing to resend".into();
                    return Ok(());
                }
                sess.conversation.pop_trailing_assistants();
                let _ = sess.save();
                sess.conversation.history()
            };
            state.rest.begin_stream();
            state.rest.waiting = true;
            state.rest.status = "thinking...".into();
            start_stream_task(history, state, client, handle);
        }

        Action::SaveCreds { api_key, model } => {
            let model = if model.is_empty() {
                DEFAULT_MODEL.to_string()
            } else {
                model
            };
            // Lazy creation: first-run path has no session yet. Create it now,
            // then apply the entered credentials.
            if state.rest.session.is_none() {
                match store::create_session() {
                    Ok(s) => state.rest.session = Some(s),
                    Err(e) => {
                        state.rest.status = format!("error: {e}");
                        return Ok(());
                    }
                }
            }
            if let Some(sess) = state.rest.session.as_mut() {
                sess.settings.api_key = api_key.clone();
                sess.settings.model = model.clone();
                let _ = sess.save();
            }
            state.rest.remember_creds(&api_key, &model);
            *client = state.rest.session.as_ref().map(build_client);
            state.rest.prev_session = None; // committed; discard fallback
            state.rest.reset_scroll();
            state.mode = Mode::Chat;
            state.rest.status = "ready".into();
        }

        Action::CancelKeyInput => {
            if let Some(prev) = state.rest.prev_session.take() {
                *client = if prev.settings.api_key.is_empty() {
                    None
                } else {
                    Some(build_client(&prev))
                };
                state.rest.session = Some(prev);
            } else if let Some(sess) = state.rest.session.as_ref() {
                // Defensive: no stashed prev; rebuild from current session.
                *client = if sess.settings.api_key.is_empty() {
                    None
                } else {
                    Some(build_client(sess))
                };
            }
            state.rest.reset_scroll();
            state.mode = Mode::Chat;
            if client.is_none() {
                state.rest.status = "no active session".into();
            } else {
                state.rest.status = "ready".into();
            }
        }

        Action::CancelKeyInputToPicker => {
            // Esc out of a picker-launched KeyInput: drop the partially-set
            // session, clear any client, and return to the session picker
            // instead of pinning a no-client Chat.
            state.rest.session = None;
            state.rest.prev_session = None;
            *client = None;
            state.rest.reset_scroll();
            state.mode = Mode::SessionPicker(PickerState::new(store::list_sessions()?));
            state.rest.status = "ready".into();
        }

        Action::PickerSelect => {
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
                state.rest.session = Some(sess);
                state.rest.reset_scroll();
                state.mode = Mode::KeyInput(KeyInputForm::prefilled(lk, lm, false, true));
            } else {
                state
                    .rest
                    .remember_creds(&sess.settings.api_key, &sess.settings.model);
                *client = Some(build_client(&sess));
                state.rest.session = Some(sess);
                state.rest.reset_scroll();
                state.mode = Mode::Chat;
                state.rest.status = "ready".into();
            }
        }
    }
    Ok(())
}

/// Apply a parsed slash command. Like [`apply_action`], it mutates state and
/// may spawn/abort the request task.
fn apply_slash(
    cmd: Command,
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    match cmd {
        Command::Compact => {
            if state.rest.waiting {
                state.rest.status = "busy — wait for response".into();
                return Ok(());
            }
            if client.is_none() || state.rest.session.is_none() {
                state.rest.status = "no active session".into();
                return Ok(());
            }
            let (to_sum, kept_tail) = {
                let sess = state.rest.session.as_ref().unwrap();
                let pn = sess.settings.compaction.preserve_n;
                sess.conversation.split_for_compaction(pn)
            };
            if to_sum.is_empty() {
                state.rest.status = "nothing to compact".into();
                return Ok(());
            }
            let mut req = vec![ChatMessage::new(
                Role::System,
                "Summarise the following conversation concisely, preserving key facts, decisions, and context.",
            )];
            req.extend(to_sum);
            state.rest.waiting = true;
            state.rest.status = "compacting...".into();
            // Fresh channel for this request; the receiver lives in state so an
            // interrupt/new just drops it and the task's result is ignored.
            let (tx, rx) = mpsc::unbounded_channel();
            state.rest.active_rx = Some(rx);
            let c = Arc::clone(client.as_ref().unwrap());
            let jh = handle.spawn(async move {
                let event = match c.complete(req).await {
                    Ok(s) => StreamEvent::Compacted {
                        summary: s,
                        kept_tail,
                    },
                    Err(e) => StreamEvent::Error(e.to_string()),
                };
                let _ = tx.send(event);
            });
            state.rest.current_task = Some(jh.abort_handle());
        }

        Command::New => {
            abort_current(&mut state.rest);
            let _ = state.rest.take_stream(); // discard partial; belongs to old session
            let sess = match store::create_session() {
                Ok(s) => s,
                Err(e) => {
                    state.rest.status = format!("error: {e}");
                    return Ok(());
                }
            };
            state.rest.prev_session = state.rest.session.take();
            state.rest.session = Some(sess);
            *client = None; // forces SaveCreds rebuild
            state.rest.reset_scroll();
            state.mode = Mode::KeyInput(KeyInputForm::prefilled(
                state.rest.last_key.clone().unwrap_or_default(),
                state
                    .rest
                    .last_model
                    .clone()
                    .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
                false, // Esc -> CancelKeyInput restores prev_session
                false, // not from picker
            ));
        }

        Command::Rename(name) => {
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
        }

        Command::Help => {
            state.rest.status =
                "/compact /new /rename <name> /help /quit · Ctrl+R resend · Esc interrupt".into();
        }

        Command::Quit => {
            if state.rest.waiting {
                abort_current(&mut state.rest);
            }
            state.rest.should_quit = true;
        }

        Command::Unknown(s) => {
            state.rest.status = format!("unknown command: /{s}");
        }
    }
    Ok(())
}
