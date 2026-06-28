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
//! channel, stashes the receiver in `state.rest.fg().active_rx`, and spawns a task
//! holding the sender. Cancelling (interrupt / `/new` / quit) just drops the
//! receiver, so a superseded task's late events vanish with no generation
//! bookkeeping.

mod terminal;
mod event_loop;
mod stream;
mod actions;
mod client;
// `pub(crate)` so the shared `commands::internet::internet_feedback` helper is
// reachable from the controller's Ctrl+E handler (outside this module tree).
pub(crate) mod commands;
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
use event_loop::daemon::{daemon_loop, DaemonHub};

// Re-export the sync-loop <-> per-client-task bridge message so the per-client
// connection task in `crate::ipc::conn` (outside this module tree) can name it.
pub(crate) use event_loop::daemon::HubInbound;

// Re-export the thin-attach-client entry so `app::client_run` reaches the
// `koma --attach` path (defined in the `client` submodule).
pub use client::client_run;

pub(super) type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Make the on-disk lock match the active session.
///
/// Releases the previously-held lock if the active session changed, then writes
/// a fresh `session.lock` for the current one. A no-op when the active session
/// is unchanged (so calling it on every activation is cheap and idempotent).
/// All lock IO is best-effort, so this never fails or blocks.
pub(super) fn reconcile_session_lock(state: &mut AppState) {
    let cur = state.rest.fg().session.as_ref().map(|s| s.path.clone());
    if state.rest.fg().held_lock == cur {
        return; // active session unchanged → on-disk lock already correct
    }
    // Drop the stale lock first so switching away from a session unlocks it.
    if let Some(old) = state.rest.fg_mut().held_lock.take() {
        crate::model::store::remove_lock(&old);
    }
    // Acquire the new session's lock (if there is an active session now).
    if let Some(new) = cur {
        crate::model::store::write_lock(&new);
        state.rest.fg_mut().held_lock = Some(new);
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
    let (workdir, settings, workdirs) = match state.rest.fg().session.as_ref() {
        Some(s) => (s.workdir(), s.settings.clone(), s.workdirs()),
        None => return,
    };
    let config = state.rest.config.clone();
    // Workspace reindex is already async (background thread); fire it always,
    // independent of whether we show the loading splash.
    crate::tool::dircache::reindex(workdirs, state.rest.fg().dir_cache.clone());

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

/// The shared startup prefix for BOTH the interactive TUI ([`run`]) and the
/// headless daemon ([`daemon_run`]).
///
/// Does everything that is independent of the terminal: ensure the config dirs
/// exist, build the tokio runtime + clone its handle, decide the initial
/// [`AppState`] (resume picker / returning-user chat / first-run wizard), load the
/// global config, capture the launch cwd for the harness workspace check, build
/// the keyless per-session client when a usable Main route resolves, and warm the
/// active session (workspace reindex + awareness). Returns the owned runtime (kept
/// alive + dropped LAST by the caller), its handle, the constructed state, and the
/// optional client (the no-client-no-send gate).
///
/// SAFE FOR HEADLESS USE: nothing here touches stdout / the terminal. `warm_session`
/// only spawns background tasks + mutates state + does best-effort lock IO, so the
/// daemon path can call this identically to the TUI path.
fn build_startup(
    opts: &crate::cli::Opts,
) -> Result<(
    tokio::runtime::Runtime,
    tokio::runtime::Handle,
    AppState,
    Option<Arc<OpenRouterClient>>,
)> {
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
                    st.rest.fg_mut().session = Some(sess);
                    // Fresh startup session → seed ITS OWN counters (0 here, since a
                    // brand-new session has no ledger yet); harmless and explicit.
                    let fg = st.rest.foreground;
                    st.rest.load_token_totals(fg, &sess_path);
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

    // If startup opened a session straight into chat (returning user), build its
    // client now; otherwise it's built when the user confirms credentials. The
    // None-gate is the "is there a usable session/key?" signal the whole runtime
    // relies on. Because the key now lives in config/settings (read per-call), the
    // condition is whether the MAIN role resolves to a usable route (a route with a
    // non-empty api_key) — NOT the old `!settings.api_key.is_empty()`. The client
    // itself is keyless; the gate just preserves the no-client-no-send invariant.
    let client: Option<Arc<OpenRouterClient>> = state
        .rest
        .fg()
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

    Ok((rt, handle, state, client))
}

/// Release every live session's on-disk lock, then drop the tokio runtime LAST.
///
/// Shared clean-exit teardown for both the TUI and daemon paths. Multi-session
/// aware — a quit (kill-all OR detach) can leave several sessions holding locks,
/// so releasing only the foreground's would strand the rest until PID-liveness
/// staleness kicked in. Dropping `rt` last cancels every spawned task; each task
/// owns the sender of its own per-request channel, and `let _ =` on each send
/// makes a post-drop send a safe no-op (no panic, no deadlock). A crash that skips
/// this is covered by PID-liveness staleness in `store::is_locked`.
fn shutdown_runtime(state: &mut AppState, rt: tokio::runtime::Runtime) {
    for s in &mut state.rest.sessions {
        if let Some(p) = s.held_lock.take() {
            crate::model::store::remove_lock(&p);
        }
    }
    drop(rt);
}

pub fn run(opts: crate::cli::Opts) -> Result<()> {
    // Shared, terminal-independent startup (dirs, runtime, state, client, warm).
    let (rt, handle, mut state, mut client) = build_startup(&opts)?;

    // Terminal setup. Guard created BEFORE the Terminal so its Drop covers a
    // failing Terminal::new, any later `?`-error, and panic-unwind.
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    // Clear the alternate screen so no shell scrollback bleeds through the
    // cells the UI never paints (e.g. the empty part of the transcript).
    terminal.clear()?;

    let result = run_loop(&mut terminal, &mut state, &handle, &mut client);

    // Terminal teardown is handled by `_guard`'s Drop at function scope.
    // Release all session locks, then drop the runtime LAST (runs on Ok and Err).
    shutdown_runtime(&mut state, rt);

    result
}

/// Headless entry point: run the koma-daemon event loop with NO terminal.
///
/// Shares [`build_startup`] with the TUI [`run`] (same dirs / runtime / state /
/// client / warm), then — instead of the terminal + `run_loop` — ignores SIGPIPE,
/// binds the unix socket, spawns the per-client accept loop, and enters
/// [`daemon_loop`]. The accept loop runs on the tokio runtime (async socket I/O);
/// `daemon_loop` runs synchronously on this thread and drains the bridge each tick
/// (critique #6). It returns when a controller sends `QuitDaemon` (or the process is
/// signalled); the shared teardown then releases every session lock, drops the
/// runtime, and unlinks the socket.
pub fn run_daemon(opts: crate::cli::Opts) -> Result<()> {
    // Critique #10: writing to a dead client must never kill the daemon. Ignore
    // SIGPIPE process-wide BEFORE any socket IO so a broken-pipe write returns
    // EPIPE (handled per-write) instead of terminating the process. `libc` is a
    // direct dependency; this is the one tiny unsafe FFI call it is needed for.
    // SAFETY: `signal` with SIG_IGN on SIGPIPE is async-signal-safe and the
    // canonical way to opt out of SIGPIPE; it touches no Rust state.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    // Shared, terminal-independent startup — identical to the TUI path.
    let (rt, handle, mut state, mut client) = build_startup(&opts)?;

    // Sync-loop <-> per-client-task bridge (critique #1/#6). The runner holds the
    // paired `req_tx` (which the accept loop clones into each connection task) for
    // the daemon's lifetime so `req_rx` never observes a premature `Disconnected`
    // before any client connects.
    let (mut hub, req_tx) = DaemonHub::new();

    // Bind the unix listener (this process becomes the live daemon — bind is the
    // liveness oracle) and spawn the accept loop onto the tokio runtime. Each
    // accepted connection gets a per-client task bridging its socket to `req_tx`.
    // `UnixListener::bind` + `handle.spawn` need a tokio reactor in scope, so enter
    // the runtime context for them. The socket path is unlinked at teardown below.
    let sock_path = crate::model::store::daemon_sock_path()?;
    {
        let _enter = handle.enter();
        let listener = crate::ipc::server::bind(&sock_path)?;
        handle.spawn(crate::ipc::server::accept_loop(listener, req_tx));
    }

    // Enter the headless loop: service_all_sessions + service_global + the request-
    // bridge drain (apply mutations) + delta streaming on the adaptive cadence.
    // Returns when a controller's QuitDaemon latches the hub shutdown flag.
    daemon_loop(&mut state, &mut client, &handle, &mut hub);

    // Clean shutdown (QuitDaemon, or a future self-exit): release locks, drop the
    // runtime LAST, and remove the socket so the next spawn binds fresh.
    shutdown_runtime(&mut state, rt);
    let _ = std::fs::remove_file(&sock_path);

    Ok(())
}

/// End-to-end daemon self-test (`koma --daemon-selftest`): drive the FULL stage-5
/// stack — bind + accept loop + per-client tasks + the real [`daemon_loop`] hub —
/// over a real unix socket, with NO terminal and NO network/session.
///
/// It proves a client request reaches the daemon and DRIVES it: a client connects,
/// `Attach`es (and gets a full `Snapshot`), sends `SubmitInput` (which the daemon
/// applies through the SAME `Action::Submit` path the TUI uses — here, with no
/// active session, that lands as the `"no active session"` status line), and then
/// observes a `StatusChanged` `Delta` carrying exactly that new status — i.e. the
/// resulting state change folds back to the client. Finally `QuitDaemon` makes the
/// real loop return so the driver thread joins cleanly.
///
/// A dedicated socket path keeps it from colliding with a live daemon. The hub +
/// `daemon_loop` run on a std thread (the loop is synchronous); the client side runs
/// on a private tokio runtime here. Prints `OK` / `FAIL` and exits 0 / 1 — it never
/// returns normally (a short-circuit CLI mode, like the IPC self-test).
pub fn run_daemon_selftest() -> ! {
    let code = match daemon_selftest_inner() {
        Ok(()) => {
            println!("koma daemon-selftest: OK");
            0
        }
        Err(e) => {
            eprintln!("koma daemon-selftest: FAIL: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

/// The fallible body of [`run_daemon_selftest`].
fn daemon_selftest_inner() -> Result<()> {
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    use crate::ipc::frame::{read_frame, write_frame, FrameReader};
    use crate::ipc::proto::{ClientRequest, DaemonEvent, DaemonFrame, StateDelta};

    // Ignore SIGPIPE for parity with the real daemon (a dead client write must not
    // kill us). SAFETY: SIG_IGN on SIGPIPE is async-signal-safe and touches no Rust
    // state — the same call `run_daemon` makes.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();

    // Dedicated socket so the test never disturbs a live daemon. `UnixListener::bind`
    // needs a tokio reactor, so enter the runtime context for the bind + spawn.
    let sock_path = crate::model::store::base_dir()?.join("daemon-selftest.sock");
    let (mut hub, req_tx) = DaemonHub::new();
    {
        let _enter = handle.enter();
        let listener = crate::ipc::server::bind(&sock_path)?;
        handle.spawn(crate::ipc::server::accept_loop(listener, req_tx));
    }

    // Drive the REAL `daemon_loop` on a std thread (it is synchronous). A fresh
    // headless state with one foreground session and NO client (so `SubmitInput`
    // exercises the no-session branch, which still mutates the status line).
    let loop_handle = handle.clone();
    let driver = std::thread::spawn(move || {
        let mut state = AppState::new(Mode::Chat);
        let mut client: Option<Arc<OpenRouterClient>> = None;
        daemon_loop(&mut state, &mut client, &loop_handle, &mut hub);
    });

    // Client side: connect, attach, submit, observe, quit.
    let result: Result<()> = rt.block_on(async {
        let mut stream = crate::ipc::client::connect(&sock_path).await?;
        let mut reader = FrameReader::new();

        // Attach -> expect a full Snapshot.
        let attach = serde_json::to_vec(&ClientRequest::Attach { foreground_id: None })?;
        write_frame(&mut stream, &attach).await?;
        let snap_frame: DaemonFrame =
            serde_json::from_slice(&read_frame(&mut stream, &mut reader).await?)?;
        anyhow::ensure!(
            matches!(snap_frame.event, DaemonEvent::Snapshot(_)),
            "attach reply was not a Snapshot: {:?}",
            snap_frame.event
        );

        // SubmitInput -> the daemon applies Action::Submit; with no active session
        // it sets status = "no active session". Read frames until that status
        // change folds back as a Delta (skipping the request's own Ack, which may
        // interleave). Bounded so a missing delta fails the test instead of hanging.
        let submit = serde_json::to_vec(&ClientRequest::SubmitInput { text: "hi".into() })?;
        write_frame(&mut stream, &submit).await?;

        let mut saw_status = false;
        for _ in 0..50 {
            let buf = tokio::time::timeout(Duration::from_secs(5), async {
                read_frame(&mut stream, &mut reader).await
            })
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for the SubmitInput status delta"))??;
            let frame: DaemonFrame = serde_json::from_slice(&buf)?;

            match frame.event {
                DaemonEvent::Delta(StateDelta::StatusChanged { session_id, text }) => {
                    anyhow::ensure!(session_id.is_none(), "expected a GLOBAL status delta");
                    anyhow::ensure!(
                        text == "no active session",
                        "unexpected status text after SubmitInput: {text:?}"
                    );
                    saw_status = true;
                    break;
                }
                // A full resync is also a valid carrier of the change; accept it.
                DaemonEvent::Snapshot(s) => {
                    if s.global.status == "no active session" {
                        saw_status = true;
                        break;
                    }
                }
                // Ack for the request / unrelated deltas: keep reading.
                _ => {}
            }
        }
        anyhow::ensure!(saw_status, "never observed the SubmitInput status change");

        // QuitDaemon -> the real loop latches shutdown and returns; expect an Ack.
        let quit = serde_json::to_vec(&ClientRequest::QuitDaemon)?;
        write_frame(&mut stream, &quit).await?;
        // Drain a couple frames to find the Ack (deltas may interleave). Best-effort.
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_secs(5), async {
                read_frame(&mut stream, &mut reader).await
            })
            .await
            {
                Ok(Ok(buf)) => {
                    let f: DaemonFrame = serde_json::from_slice(&buf)?;
                    if matches!(f.event, DaemonEvent::Ack) {
                        break;
                    }
                }
                // Socket closed (daemon already tore down) is acceptable post-quit.
                _ => break,
            }
        }
        drop(stream);
        Ok(())
    });

    // The driver thread exits once `daemon_loop` observes the QuitDaemon shutdown
    // flag. Join it (bounded) so a wedged loop surfaces as a test failure. Use a
    // small channel to time-box the join.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = driver.join();
        let _ = done_tx.send(());
    });
    let joined = matches!(
        done_rx.recv_timeout(Duration::from_secs(10)),
        Ok(()) | Err(RecvTimeoutError::Disconnected)
    );

    // Clean up the socket regardless (best-effort).
    let _ = std::fs::remove_file(&sock_path);

    result?;
    anyhow::ensure!(joined, "daemon_loop did not return after QuitDaemon");
    Ok(())
}
