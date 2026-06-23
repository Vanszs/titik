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
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::app::mode::{KeyInputForm, Mode, PickerState};
use crate::app::state::{AppState, AppStateRest};
use crate::config::{DEFAULT_BASE_URL, DEFAULT_MODEL};
use crate::controller::{self, command::Command, input::Action};
use crate::dto::chat::{ChatMessage, Role};
use crate::model::{session::Session, settings::Settings, store};
use crate::service::{openrouter::OpenRouterClient, StreamEvent, TaggedEvent};
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

    let (tx, mut rx) = mpsc::unbounded_channel::<TaggedEvent>();

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

    let result = run_loop(&mut terminal, &mut state, &mut rx, &tx, &handle, &mut client);

    // Terminal teardown is handled by `_guard`'s Drop at function scope.

    // drop(rt) LAST: runtime shutdown cancels spawned tasks; tasks hold tx
    // clones which become Err on send after rx (owned by run) is dropped. The
    // `let _ =` on every task send makes this safe — no panic, no deadlock.
    drop(rt);

    result
}

fn run_loop(
    terminal: &mut Term,
    state: &mut AppState,
    rx: &mut UnboundedReceiver<TaggedEvent>,
    tx: &UnboundedSender<TaggedEvent>,
    handle: &tokio::runtime::Handle,
    client: &mut Option<Arc<OpenRouterClient>>,
) -> Result<()> {
    loop {
        terminal.draw(|f| view::draw(f, state))?;

        // 1. drain async events; discard stale generations.
        while let Ok(tev) = rx.try_recv() {
            if tev.generation != state.rest.generation {
                continue;
            }
            match tev.event {
                StreamEvent::Token(t) => {
                    state.rest.append_token(&t);
                    state.rest.status = "streaming".into();
                }
                StreamEvent::Done => {
                    let buf = state.rest.take_stream();
                    if let Some(b) = buf {
                        if !b.is_empty() {
                            if let Some(sess) = state.rest.session.as_mut() {
                                sess.conversation.push_assistant(b);
                                if let Err(e) = sess.save() {
                                    state.rest.status = format!("error: {e}");
                                }
                            }
                        }
                    }
                    state.rest.waiting = false;
                    state.rest.current_task = None;
                    if !state.rest.status.starts_with("error") {
                        state.rest.status = "ready".into();
                    }
                }
                StreamEvent::Error(e) => {
                    let buf = state.rest.take_stream();
                    if let Some(b) = buf {
                        if !b.is_empty() {
                            if let Some(sess) = state.rest.session.as_mut() {
                                sess.conversation.push_assistant(b);
                                let _ = sess.save();
                            }
                        }
                    }
                    state.rest.waiting = false;
                    state.rest.current_task = None;
                    state.rest.status = format!("error: {e}");
                }
                StreamEvent::Compacted { summary, kept_tail } => {
                    if let Some(sess) = state.rest.session.as_mut() {
                        sess.conversation.apply_compaction(summary, kept_tail);
                        let _ = sess.save();
                    }
                    state.rest.waiting = false;
                    state.rest.current_task = None;
                    state.rest.status = "ready".into();
                }
            }
        }

        // 2. input.
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                let action = controller::input::handle_key(state, key);
                apply_action(action, state, client, handle, tx)?;
            }
        }

        if state.rest.should_quit {
            break;
        }
    }
    Ok(())
}

/// Abort the in-flight task, invalidate any events it may still send, clear
/// the waiting flag.
fn abort_current(rest: &mut AppStateRest) {
    if let Some(h) = rest.current_task.take() {
        h.abort();
    }
    rest.bump_generation();
    rest.waiting = false;
}

/// Spawn a streaming task for `history`. Bumps the generation FIRST so any
/// in-flight task's later events are discarded.
fn start_stream_task(
    history: Vec<ChatMessage>,
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
    tx: &UnboundedSender<TaggedEvent>,
) {
    let gen = state.rest.bump_generation();
    let c = Arc::clone(client.as_ref().unwrap());
    let txc = tx.clone();
    let jh = handle.spawn(async move {
        let _ = c.stream_complete(history, gen, txc).await;
    });
    state.rest.current_task = Some(jh.abort_handle());
}

fn apply_action(
    action: Action,
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
    tx: &UnboundedSender<TaggedEvent>,
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
            start_stream_task(history, state, client, handle, tx);
        }

        Action::Slash(cmd) => {
            apply_slash(cmd, state, client, handle, tx)?;
        }

        Action::Interrupt => {
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
            start_stream_task(history, state, client, handle, tx);
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

fn apply_slash(
    cmd: Command,
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
    tx: &UnboundedSender<TaggedEvent>,
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
            let gen = state.rest.bump_generation();
            let c = Arc::clone(client.as_ref().unwrap());
            let txc = tx.clone();
            let jh = handle.spawn(async move {
                match c.complete(req).await {
                    Ok(s) => {
                        let _ = txc.send(TaggedEvent {
                            generation: gen,
                            event: StreamEvent::Compacted {
                                summary: s,
                                kept_tail,
                            },
                        });
                    }
                    Err(e) => {
                        let _ = txc.send(TaggedEvent {
                            generation: gen,
                            event: StreamEvent::Error(e.to_string()),
                        });
                    }
                }
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
