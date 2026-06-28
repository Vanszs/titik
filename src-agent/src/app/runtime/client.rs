//! Thin attach client — the `koma --attach` core (daemon stage 6).
//!
//! [`client_run`] connects to a running daemon's unix socket, attaches, and then
//! renders the daemon's state + forwards input. It does NONE of the real work:
//! no `service_all_sessions`, no turn machinery, no agent runtime. It maintains a
//! SHADOW [`AppState`] populated PURELY from the daemon's
//! [`DaemonEvent::Snapshot`] / [`DaemonEvent::Delta`] frames and feeds that shadow
//! to the EXISTING [`crate::view::draw`] — so the attach client renders identically
//! to a local TUI, with zero second render path to drift.
//!
//! # Session-lock ownership (daemon stage 8)
//!
//! Session locks (`session.lock`, holding the owner's PID — see
//! [`crate::model::store`]) are owned EXCLUSIVELY by the process that runs the
//! session lifecycle: the local TUI or the headless daemon. The client does the
//! real work through neither path — its [`SessionRuntime`]s are SHADOW copies
//! rebuilt from frames, never warmed, never saved, never the foreground of a real
//! turn. So this module deliberately calls NO lock function
//! (`store::write_lock` / `store::remove_lock` / `store::is_locked`) and never
//! `warm_session` / `reconcile_session_lock`. When a forwarded key drives a `/new`
//! or a foreground switch, it is the DAEMON's `apply_action` that writes the lock
//! (with the daemon's PID), not the client. Writing a lock here would stamp a
//! shadow session with the CLIENT's PID and corrupt the daemon's ownership.
//!
//! # Why a real `AppState` as the shadow
//!
//! `view::draw` reads only `state.rest` (in Chat mode) + `state.mode`. Rebuilding
//! a real [`AppState`] from each snapshot — one [`crate::app::state::SessionRuntime`]
//! per [`crate::ipc::proto::SessionSnapshot`], each carrying a reconstructed
//! [`crate::model::session::Session`] (messages + name + model) — lets the
//! unmodified chat renderer consume it directly. Non-render fields (channels,
//! abort handles, tool state machines) stay at their `Default`; the client never
//! advances a turn, so they are never read.
//!
//! # Transport bridge (mirrors the daemon's [`crate::ipc::conn`], client-side)
//!
//! The render loop is SYNCHRONOUS (crossterm draw + input poll). Socket I/O lives
//! in two tokio tasks bridged over `std::sync::mpsc`:
//! - a READER task: `read_frame_from` -> decode [`DaemonFrame`] -> push onto the
//!   loop's incoming `std::sync::mpsc::Sender<DaemonFrame>`. On EOF/error it drops
//!   the sender, which the loop observes as the daemon going away.
//! - a WRITER task: owns the outbound `std::sync::mpsc::Receiver<ClientRequest>`,
//!   polls it on a short interval, and writes each as a frame. (Same `!Sync`
//!   reasoning as `conn::write_loop`: a `std::sync::mpsc::Receiver` held across an
//!   await makes the future non-`Send`; collect-then-write keeps it off the await.)
//!
//! # Seq-gap recovery (critique #1)
//!
//! Every [`DaemonFrame`] carries a per-connection monotonic `seq`. The loop tracks
//! the next expected seq; on a gap it sends [`ClientRequest::Resync`] and DROPS
//! every frame until the fresh full [`DaemonEvent::Snapshot`] arrives (whose seq
//! reseeds the expectation), so one dropped delta can't leave a permanently-wrong
//! shadow.
//!
//! # Input forwarding (raw keys)
//!
//! Each terminal key is forwarded VERBATIM as [`ClientRequest::SendKey`]; the
//! daemon runs the SAME `controller::input::handle_key` + `apply_action` pipeline
//! the local TUI uses, so every high-level gesture — submitting a typed message,
//! `/resume` (the session hub / foreground switch), `/new` — works through forwarded keys with
//! no client-side command parsing to drift from the daemon. The ONE key the client
//! interprets locally is the detach gesture (Ctrl-C): it sends
//! [`ClientRequest::Detach`] and exits the client, leaving the daemon (and every
//! session) running.

use std::io::stdout;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind,
};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::app::mode::{Mode, QuitConfirmState};
use crate::app::state::{AppState, SessionRuntime, ToastKind};
use crate::app::subagent::PendingSubagent;
use crate::ipc::frame::{self, FrameReader};
use crate::ipc::proto::{
    ClientRequest, DaemonEvent, DaemonFrame, KeyWire, ModeSnapshot, SessionSnapshot, StateDelta,
    StateSnapshot,
};
use crate::model::conversation::Conversation;
use crate::model::session::Session;
use crate::model::settings::Settings;
use crate::model::store;
use crate::view;

use super::terminal::TerminalGuard;

/// How often the writer task polls its (sync) request queue. 4ms matches the
/// daemon conn's `FRAME_POLL` so a typed key reaches the daemon within one tick.
const REQ_POLL: Duration = Duration::from_millis(4);

/// Upper bound on how long the client teardown waits for the writer task to flush
/// its final queued frame(s) (the shutdown `QuitDaemon`/`Detach`) before the tokio
/// runtime is dropped. The writer drains-and-returns the instant its channel closes
/// (well under one `REQ_POLL`), so this is only a safety ceiling against a wedged
/// socket — exit must never hang on a misbehaving daemon write half.
const WRITER_FLUSH_TIMEOUT: Duration = Duration::from_millis(200);

/// Local TTL for a toast reconstructed from a [`StateDelta::Toast`]. The daemon's
/// toast `Instant` is daemon-local and never crosses the wire (see `ipc::snapshot`);
/// the client re-derives its own dismissal timer here, matching the ~4s feel of the
/// local TUI's toasts.
const TOAST_TTL: Duration = Duration::from_secs(4);

/// Attach to a running daemon and run the thin render+forward client.
///
/// Connects to the daemon socket (an `Err` means no daemon is up — surfaced to the
/// caller, which prints it), spawns the reader/writer bridge tasks, sends
/// [`ClientRequest::Attach`], then enters the synchronous render loop. Returns when
/// the user detaches (Ctrl-C) or the daemon's socket closes; the terminal is
/// restored by [`TerminalGuard`]'s drop and the runtime is dropped last.
pub fn client_run(_opts: crate::cli::Opts) -> Result<()> {
    // The client needs the config dirs only to resolve the socket path; it owns no
    // sessions and writes no config. In particular it touches NO session lock here
    // or anywhere downstream (lock ownership belongs to the daemon — see the
    // module header): the only `store` calls are these two lock-free path helpers.
    store::ensure_dirs()?;
    let sock_path = store::daemon_sock_path()?;

    // A small multi-thread runtime drives the two socket tasks. The render loop runs
    // on THIS thread (synchronous), exactly like the local TUI's `run_loop`.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();

    // Connect first so a missing daemon fails BEFORE we touch the terminal (no
    // alt-screen flash on "no daemon"). The connected stream is split into the two
    // task halves below.
    let stream = handle
        .block_on(async { crate::ipc::client::connect(&sock_path).await })
        .map_err(|e| {
            anyhow::anyhow!("could not reach koma daemon at {}: {e}", sock_path.display())
        })?;

    // Bridge channels: incoming frames (daemon -> loop) and outgoing requests
    // (loop -> daemon). Mirrors the daemon hub's bridge, client-side.
    let (frame_tx, frame_rx) = std::sync::mpsc::channel::<DaemonFrame>();
    let (req_tx, req_rx) = std::sync::mpsc::channel::<ClientRequest>();

    // Split + spawn the two I/O tasks on the runtime (a tokio reactor must be in
    // scope for `into_split` + `spawn`). The writer's `JoinHandle` is kept so the
    // teardown can WAIT for it to flush its final frame(s) (the shutdown
    // `QuitDaemon`/`Detach`) before the runtime is dropped — see below.
    let writer_handle = {
        let _enter = handle.enter();
        let (read_half, write_half) = stream.into_split();
        handle.spawn(reader_task(read_half, frame_tx));
        handle.spawn(writer_task(write_half, req_rx))
    };

    // Send the Attach handshake; the daemon answers with the initial full Snapshot
    // (drained in the loop's first incoming pass).
    let _ = req_tx.send(ClientRequest::Attach {
        foreground_id: None,
    });

    // Terminal setup — identical to the local TUI (`run`). Guard first so a failure
    // anywhere after still restores the terminal.
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = render_loop(&mut terminal, &frame_rx, &req_tx);

    // Polite detach so the daemon passes the controller seat promptly (the socket
    // close would also trigger it, but this is cleaner). If the `/quit` overlay's
    // `[k]` kill-all path ran, a `QuitDaemon` was ALSO queued ahead of this — both
    // MUST reach the daemon or it is left orphaned (socket open, no controller).
    let _ = req_tx.send(ClientRequest::Detach);

    // Deterministic flush of the final frame(s) before the runtime dies. Dropping
    // `req_tx` closes the outbound channel, which the writer observes as
    // `Disconnected`: it then drains EVERY remaining queued request to the socket
    // and returns (see `writer_task`). We must wait for that drain — previously the
    // runtime was dropped immediately, cancelling the writer mid-`poll.tick()` sleep
    // and LOSING the queued `QuitDaemon`/`Detach` (an orphaned daemon). Drop the
    // sender, then JOIN the writer (bounded, so a wedged socket can't hang exit).
    drop(req_tx);
    let _ = handle.block_on(async {
        tokio::time::timeout(WRITER_FLUSH_TIMEOUT, writer_handle).await
    });

    // Writer is done (or the bound elapsed) — its final frames are flushed to the
    // socket. Drop the runtime LAST so the reader task is cancelled after exit.
    drop(rt);

    result
}

/// The synchronous render loop. Each tick: redraw if dirty, drain incoming frames
/// (apply snapshot/delta or recover a seq gap), then poll + forward terminal input.
/// Returns when the user detaches (Ctrl-C) or the daemon's socket closes.
fn render_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    frame_rx: &Receiver<DaemonFrame>,
    req_tx: &Sender<ClientRequest>,
) -> Result<()> {
    // The shadow is a real AppState reconstructed purely from frames. It starts in
    // a neutral Chat with a single empty session; the first Snapshot replaces it.
    let mut shadow = AppState::new(Mode::Chat);
    // Until the first Snapshot lands the shadow is empty — show a clear status so
    // the screen isn't a blank "ready".
    shadow.rest.status = "attaching…".into();

    // Per-connection seq tracking (critique #1). `expected` is the seq the NEXT
    // frame should carry. `0` means "not yet seeded" — the first frame seeds it.
    let mut expected: u64 = 0;
    let mut seeded = false;
    // While true, every frame except a fresh Snapshot is dropped: a gap was seen and
    // a Resync was sent, so the shadow is stale until the full snapshot rebuilds it.
    let mut awaiting_resync = false;

    let mut dirty = true; // paint once on entry
    loop {
        if dirty {
            terminal.draw(|f| view::draw(f, &shadow))?;
            dirty = false;
        }

        // --- drain every queued incoming frame ---
        loop {
            match frame_rx.try_recv() {
                Ok(frame) => {
                    if apply_frame(
                        frame,
                        &mut shadow,
                        &mut expected,
                        &mut seeded,
                        &mut awaiting_resync,
                        req_tx,
                    ) {
                        dirty = true;
                    }
                }
                Err(TryRecvError::Empty) => break,
                // The reader task dropped its sender: the daemon's socket closed.
                // Nothing more will ever arrive — leave the client.
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // --- poll + forward terminal input ---
        // Adaptive cadence mirroring the local loop: fast while the foreground
        // session is working (so streamed deltas flush at >=60fps), idle otherwise.
        let busy = shadow.rest.fg().waiting;
        let timeout = if busy {
            Duration::from_millis(8)
        } else {
            Duration::from_millis(100)
        };
        if event::poll(timeout)? {
            while event::poll(Duration::ZERO)? {
                match event::read()? {
                    Event::Key(key) => {
                        // The `/quit` overlay's choices are CLIENT-process decisions, so
                        // when the shadow is in QuitConfirm (mirrored from the daemon's
                        // mode) the client intercepts its keys locally instead of
                        // forwarding them (daemon stage 12). `Detach`/`Kill` ask the loop
                        // to exit the client.
                        if matches!(shadow.mode, Mode::QuitConfirm(_)) {
                            match handle_quit_confirm_key(&key, req_tx) {
                                QuitConfirmKey::ExitClient => return Ok(()),
                                QuitConfirmKey::Stay => {}
                            }
                            continue;
                        }
                        // Outside the overlay: the ONE locally-interpreted gesture is
                        // Ctrl-C, which detaches the client (leaves the daemon running).
                        // Everything else is forwarded verbatim for the daemon.
                        if is_detach(&key) {
                            return Ok(());
                        }
                        let _ = req_tx.send(ClientRequest::SendKey(KeyWire::from(key)));
                    }
                    // Mouse wheel scrolls the LOCAL shadow transcript (a pure view
                    // concern — the daemon's scroll is its own; scrolling the shadow
                    // gives immediate feedback without a round-trip). Bottom-pinning
                    // follow is reconstructed from snapshots, so a manual scroll just
                    // nudges the local offset for this render.
                    Event::Mouse(m) if matches!(shadow.mode, Mode::Chat) => match m.kind {
                        MouseEventKind::ScrollUp => {
                            for _ in 0..3 {
                                shadow.rest.scroll_up();
                            }
                            dirty = true;
                        }
                        MouseEventKind::ScrollDown => {
                            for _ in 0..3 {
                                shadow.rest.scroll_down();
                            }
                            dirty = true;
                        }
                        _ => {}
                    },
                    Event::Resize(_, _) => dirty = true,
                    // Pasted text is forwarded character-by-character as key events so
                    // the daemon's composer ingests it through the same path as typing.
                    Event::Paste(text) => {
                        for ch in text.chars() {
                            let _ = req_tx.send(ClientRequest::SendKey(KeyWire::from(
                                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
                            )));
                        }
                    }
                    _ => {}
                }
            }
        }

        // Expire a locally-reconstructed toast once its TTL passes (the daemon never
        // sends a "toast cleared" delta; the client owns its own dismissal timer).
        if let Some((_, until, _)) = shadow.rest.toast.as_ref() {
            if Instant::now() >= *until {
                shadow.rest.toast = None;
                dirty = true;
            }
        }
    }
}

/// Apply one incoming [`DaemonFrame`] to the shadow, handling seq-gap recovery.
///
/// Returns `true` if the shadow changed and a redraw is needed. On a detected gap
/// it sends [`ClientRequest::Resync`], sets `awaiting_resync`, and drops the frame;
/// while `awaiting_resync` only a fresh `Snapshot` is applied (it reseeds the seq +
/// clears the flag). `Ack` / `Error` frames advance the seq but are non-visual
/// (an `Error` could surface as a toast in a later refinement).
fn apply_frame(
    frame: DaemonFrame,
    shadow: &mut AppState,
    expected: &mut u64,
    seeded: &mut bool,
    awaiting_resync: &mut bool,
    req_tx: &Sender<ClientRequest>,
) -> bool {
    // --- seq-gap detection (critique #1) ---
    if !*seeded {
        // First frame ever: seed the expectation from it (whatever it is) so we
        // don't false-positive a gap on the initial Snapshot's seq.
        *seeded = true;
        *expected = frame.seq;
    } else if frame.seq != *expected {
        // A frame was dropped (or reordered). Ask for a full rebuild and ignore
        // everything until the fresh Snapshot arrives — UNLESS this very frame is a
        // Snapshot, which is itself a valid full rebuild we can take right now.
        if matches!(frame.event, DaemonEvent::Snapshot(_)) {
            // Fall through to apply it; it reseeds the seq below.
        } else {
            if !*awaiting_resync {
                *awaiting_resync = true;
                let _ = req_tx.send(ClientRequest::Resync);
            }
            // Resync the expectation forward so we don't spam Resync on every
            // subsequent gapped frame; the awaited Snapshot will reseed precisely.
            *expected = frame.seq.wrapping_add(1);
            return false;
        }
    }
    // Next frame should be exactly one past this one.
    *expected = frame.seq.wrapping_add(1);

    match frame.event {
        DaemonEvent::Snapshot(snap) => {
            // A full Snapshot is always a valid rebuild — it clears any pending
            // resync and reseeds the shadow wholesale.
            *awaiting_resync = false;
            apply_snapshot(shadow, snap);
            true
        }
        DaemonEvent::Delta(delta) => {
            // Drop deltas while the shadow is known-stale (waiting on the resync
            // Snapshot) — applying them onto a wrong baseline would corrupt it.
            if *awaiting_resync {
                return false;
            }
            apply_delta(shadow, delta)
        }
        // Non-visual control replies. (A future refinement could toast an Error.)
        DaemonEvent::Ack | DaemonEvent::Error(_) => false,
    }
}

/// Rebuild the entire shadow [`AppState`] from a full [`StateSnapshot`].
///
/// Replaces `rest.sessions` with one reconstructed [`SessionRuntime`] per snapshot
/// session, points `foreground` at the `foreground_id`, and copies the global
/// fields. The transcript-render cache is cleared because a snapshot can replace the
/// committed history wholesale (e.g. a foreground switch to a different session) and
/// the cache's incremental-append guard only covers a pure shrink, not a divergence.
fn apply_snapshot(shadow: &mut AppState, snap: StateSnapshot) {
    let StateSnapshot {
        foreground_id,
        sessions,
        global,
    } = snap;

    // Rebuild every session runtime from its projection.
    let runtimes: Vec<SessionRuntime> = sessions.iter().map(shadow_session_runtime).collect();

    // Resolve the foreground index by stable id (never trust an index across the
    // wire). Fall back to 0 if the id is somehow absent — `sessions` is always >=1.
    let fg = foreground_id
        .as_deref()
        .and_then(|id| sessions.iter().position(|s| s.id == id))
        .unwrap_or(0);

    shadow.rest.sessions = if runtimes.is_empty() {
        // Defensive: never leave `sessions` empty (fg()/fg_mut() index it). The
        // daemon always projects >=1 session, so this is belt-and-suspenders.
        vec![SessionRuntime::new()]
    } else {
        runtimes
    };
    shadow.rest.foreground = fg.min(shadow.rest.sessions.len() - 1);

    // Global render fields.
    shadow.rest.input = global.input;
    shadow.rest.cursor = global.cursor;
    shadow.rest.scroll = global.scroll;
    shadow.rest.follow = global.follow;
    shadow.rest.status = global.status;
    shadow.rest.toast = global.toast.map(|(kind, text)| {
        (text, Instant::now() + TOAST_TTL, toast_kind(&kind))
    });

    // Re-anchor the comet clock from the projected elapsed-ms (authoritative for
    // this snapshot). `work_since = now - elapsed` makes the status shimmer animate
    // from the SAME phase + elapsed-seconds the daemon is at, rather than restarting
    // at 0 each snapshot. `None` (idle) clears it. This REPLACES the old derive-from-
    // working-flag reconcile on the snapshot path; the delta path still reconciles
    // approximately (a working flip there means work just began, so `now` is right).
    shadow.rest.work_since = global
        .work_elapsed_ms
        .map(|ms| Instant::now() - Duration::from_millis(ms));

    // The committed history may have changed wholesale; drop the rendered-lines
    // cache so the next draw rebuilds it against the new messages.
    shadow.rest.transcript_cache.borrow_mut().blocks.clear();

    // Mode: reconstruct from the pure-data `ModeSnapshot`. Chat is the payload-free,
    // fully-projected screen. QuitConfirm (daemon stage 12) is the ONE other mode the
    // client reconstructs: when the daemon enters the `/quit` overlay (forwarded keys),
    // the client mirrors it so the EXISTING overlay view renders AND the client can
    // intercept the lifecycle keys ([d] detach / [k] kill-all) locally — those are
    // client-process decisions, not pure daemon mutations (see `render_loop`). Its
    // busy/total counts now ride on the snapshot (the daemon's exact `QuitConfirmState`),
    // so the header reads the same as the daemon's instead of being re-derived here.
    // Every OTHER `ModeSnapshot` variant is still a STUB (its modal payload is not
    // projected until a later stage), so those fall back to Chat — the safe, correct
    // render — rather than fabricate an empty picker/form.
    shadow.mode = match global.mode {
        ModeSnapshot::QuitConfirm { working, total } => {
            Mode::QuitConfirm(Box::new(QuitConfirmState::new(working, total)))
        }
        _ => Mode::Chat,
    };

    // NOTE: the comet clock (`work_since`) was already set authoritatively from the
    // snapshot's `work_elapsed_ms` above, so it is deliberately NOT reconciled here
    // (re-deriving would discard the precise daemon-anchored phase).
}

/// Build a shadow [`SessionRuntime`] from one [`SessionSnapshot`].
///
/// Carries the stable id + the render-relevant fields (streaming buffers, token /
/// cost counters, approval flags, working/finished-unseen). The committed messages +
/// name + model are reconstructed into a minimal [`Session`] so the unmodified chat
/// transcript/header/input renderers consume it exactly as they do a live session.
/// Every NON-render field stays at `Default` — the client never advances a turn, so
/// the tool/sub-agent state machines and channels are never read.
///
/// The QUEUED [`PendingSubagent`]s ARE reconstructed (they are plain data — no live
/// handles), so once the `$`-panel open-state is itself projected (a later stage) the
/// panel's "pending" rows render off real shadow data. The RUNNING `subagents` are
/// still NOT reconstructed: a live [`crate::app::subagent::SubAgent`] holds a tokio
/// `AbortHandle` + receiver that cannot be minted on the client's sync render thread,
/// so their reconstruction is deferred (their wire projection is already enriched for
/// that later stage).
fn shadow_session_runtime(s: &SessionSnapshot) -> SessionRuntime {
    let mut rt = SessionRuntime::new();
    rt.id = s.id.clone();
    rt.session = Some(shadow_session(s));
    rt.streaming = s.streaming.clone();
    rt.stream_reasoning = s.stream_reasoning.clone();
    rt.tokens_in = s.tokens_in;
    rt.tokens_out = s.tokens_out;
    rt.cost = s.cost;
    rt.tokens_cached = s.tokens_cached;
    // `waiting` drives the local input-poll cadence + the comet; mirror the snapshot's
    // composite `working` so a parked/streaming background session keeps the shadow
    // ticking fast and shimmering, matching the daemon.
    rt.waiting = s.working;
    rt.awaiting_approval = s.awaiting_approval;
    rt.approval_reason = s.approval_reason.clone();
    rt.finished_unseen = s.finished_unseen;
    // Reconstruct the queued delegations (plain data) so the remote `$` panel can
    // list the same "pending" rows the local TUI shows. FIFO order is preserved.
    rt.pending_subagents = s
        .pending_subagents
        .iter()
        .map(|p| PendingSubagent {
            id: p.id,
            agent_name: p.agent_name.clone(),
            prompt: p.prompt.clone(),
            // The turn-bookkeeping call id is daemon-internal + never rendered, and the
            // client never advances a turn, so a shadow pending entry carries `None`.
            tool_call_id: None,
        })
        .collect();
    rt
}

/// Reconstruct a minimal [`Session`] from a [`SessionSnapshot`] for rendering.
///
/// Only the fields the chat view reads are meaningful: `name` (the input-tab label),
/// `conversation` (the transcript), and `settings.model` (the model-name row). The
/// path / pwd_hash / api_key are render-irrelevant on the client and left empty —
/// the client never saves, sends, or locks anything.
fn shadow_session(s: &SessionSnapshot) -> Session {
    // The model row falls back to `settings.model` when the resolved model is empty;
    // the snapshot doesn't carry the resolved model separately, so seed a blank model
    // and let the header's own fallback render. (Model projection can be added to the
    // snapshot later if the client should show the exact resolved id.)
    let settings = Settings {
        name: s.name.clone(),
        model: String::new(),
        ..Default::default()
    };

    Session::new(
        s.id.clone(),
        std::path::PathBuf::new(),
        String::new(),
        settings,
        Conversation::from_messages(s.messages.clone()),
    )
}

/// Apply one incremental [`StateDelta`] to the shadow in place.
///
/// Returns `true` if the shadow changed. Session-scoped deltas resolve their target
/// by stable id (never index); an unknown id is a harmless no-op (the next Snapshot
/// reconciles). The differ only emits these for high-frequency per-tick changes;
/// anything structural (history, tokens, approval, sub-agents, the session set)
/// arrives as a full Snapshot instead (see `ipc::snapshot::diff`).
fn apply_delta(shadow: &mut AppState, delta: StateDelta) -> bool {
    match delta {
        StateDelta::TokenAppended { session_id, text } => {
            if let Some(rt) = session_by_id_mut(shadow, &session_id) {
                // A token before any `streaming` buffer means the daemon went
                // None -> Some("…") this turn (the differ treats None/Some("") alike);
                // initialise the buffer so the append lands.
                rt.streaming.get_or_insert_with(String::new).push_str(&text);
                return true;
            }
            false
        }
        StateDelta::ReasoningAppended { session_id, text } => {
            if let Some(rt) = session_by_id_mut(shadow, &session_id) {
                rt.stream_reasoning.push_str(&text);
                return true;
            }
            false
        }
        StateDelta::StatusChanged { session_id, text } => match session_id {
            // Session-scoped status is not separately rendered today (the status line
            // is global); a `None` (global) status updates the rendered status line.
            None => {
                shadow.rest.status = text;
                true
            }
            Some(_) => false,
        },
        StateDelta::InputChanged { text, cursor } => {
            // The shared composer moved (typed/deleted a char, or the caret moved).
            // Carries the WHOLE input string, so replace wholesale; clamp the caret
            // into bounds defensively (the daemon sends a consistent pair, but the
            // composer renderer indexes by cursor and must never read past the end).
            shadow.rest.input = text;
            shadow.rest.cursor = cursor.min(shadow.rest.input.chars().count());
            true
        }
        StateDelta::ScrollChanged { scroll, follow } => {
            // Global transcript view state moved on the daemon (a forwarded scroll
            // key, or new content re-pinning follow). Mirror it so the rendered
            // offset tracks the daemon between full snapshots. The renderer clamps
            // `scroll` against the live content height each draw, so an offset that
            // momentarily exceeds the shadow's shorter content is self-correcting.
            shadow.rest.scroll = scroll;
            shadow.rest.follow = follow;
            true
        }
        StateDelta::SessionStatusChanged {
            session_id,
            working,
            finished_unseen,
        } => {
            if let Some(rt) = session_by_id_mut(shadow, &session_id) {
                rt.waiting = working;
                rt.finished_unseen = finished_unseen;
                // The working flag feeds the comet clock; reconcile it (only the
                // foreground session's clock is rendered).
                reconcile_work_clock(shadow);
                return true;
            }
            false
        }
        StateDelta::ForegroundChanged { session_id } => {
            if let Some(idx) = shadow
                .rest
                .sessions
                .iter()
                .position(|s| s.id == session_id)
            {
                shadow.rest.foreground = idx;
                // Switching foreground swaps the visible transcript wholesale; clear
                // the rendered-lines cache so it rebuilds for the new session.
                shadow.rest.transcript_cache.borrow_mut().blocks.clear();
                reconcile_work_clock(shadow);
                return true;
            }
            false
        }
        StateDelta::SessionAdded(snap) => {
            // A new parallel session appeared. Append its runtime; the differ would
            // normally send a full Snapshot for a set change, but accept the delta
            // form too (it is in the vocabulary) so the shadow stays in step either way.
            if !shadow.rest.sessions.iter().any(|s| s.id == snap.id) {
                shadow.rest.sessions.push(shadow_session_runtime(&snap));
                return true;
            }
            false
        }
        StateDelta::Toast { kind, text } => {
            shadow.rest.toast = Some((text, Instant::now() + TOAST_TTL, toast_kind(&kind)));
            true
        }
    }
}

/// Find a shadow session runtime by its stable id (mutable).
fn session_by_id_mut<'a>(shadow: &'a mut AppState, id: &str) -> Option<&'a mut SessionRuntime> {
    shadow.rest.sessions.iter_mut().find(|s| s.id == id)
}

/// Map a wire toast `kind` string ("error" / "info") to the local [`ToastKind`].
/// Anything unexpected degrades to `Info` (a neutral box, never a false error).
fn toast_kind(kind: &str) -> ToastKind {
    match kind {
        "error" => ToastKind::Error,
        _ => ToastKind::Info,
    }
}

/// Re-derive the local "comet" animation clock from the FOREGROUND session's working
/// state, mirroring the rising/falling-edge reconcile the daemon/TUI loop does.
///
/// The status-line shimmer renders only when `work_since` is set. The daemon's own
/// `work_since` is daemon-local and not projected (it's a pure animation clock), so
/// the client maintains its own: set it the moment the foreground session is working
/// (and not paused for approval) and it isn't already running; clear it the moment
/// work ends or an approval prompt takes over.
fn reconcile_work_clock(shadow: &mut AppState) {
    let fg = shadow.rest.fg();
    let shimmer = fg.waiting && !fg.awaiting_approval;
    if shimmer {
        if shadow.rest.work_since.is_none() {
            shadow.rest.work_since = Some(Instant::now());
        }
    } else {
        shadow.rest.work_since = None;
    }
}

/// Is this key the client's local DETACH gesture (Ctrl-C)?
///
/// Detaching the client leaves the daemon — and every session — running. Every
/// OTHER key (including Esc, which is meaningful to the remote session's modes) is
/// forwarded to the daemon, so the client never steals a key the session needs.
fn is_detach(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// What a key handled inside the mirrored `/quit` overlay tells the render loop to do.
enum QuitConfirmKey {
    /// Tear down the client process (the request to act on it was already queued).
    ExitClient,
    /// Stay attached and keep rendering (cancel, or a swallowed stray key).
    Stay,
}

/// Handle a key while the shadow mirrors the daemon's `/quit` confirm overlay
/// (daemon stage 12). The overlay's three choices are CLIENT-process-lifecycle
/// decisions, so — unlike every other key, which is forwarded for the daemon to
/// interpret — the client acts on them itself:
///
///   `[d]` DETACH & keep — reset the daemon's overlay back to Chat (a forwarded
///         `Esc` = the daemon's own cancel, so a later reattach lands in Chat, not
///         the stale overlay), send [`ClientRequest::Detach`] (the daemon passes the
///         controller seat and keeps EVERY session cooking headless), then exit ONLY
///         the client.
///   `[k]` KILL ALL & quit — send [`ClientRequest::QuitDaemon`]; the daemon latches
///         its shutdown flag, tombstones every session, releases all locks, unlinks
///         its socket, and self-exits via its graceful teardown. Then exit the client.
///         (No `Esc` first: the daemon is shutting down wholesale, so its mode is moot.)
///   `Esc` / `Ctrl-C` cancel — forward an `Esc` so the daemon's `handle_quit_confirm`
///         runs `QuitCancel` and returns to Chat; the resulting snapshot flips the
///         shadow back. The client stays attached.
///
/// Every other key is swallowed (the overlay has no text entry — mirrors the daemon's
/// own `handle_quit_confirm`, which returns `Action::None` for anything else).
///
/// Requests share the ordered outbound queue, so the `[d]` pair is delivered
/// Esc-then-Detach in sequence, guaranteeing the daemon leaves the overlay before the
/// client drops.
fn handle_quit_confirm_key(key: &KeyEvent, req_tx: &Sender<ClientRequest>) -> QuitConfirmKey {
    // Ctrl-C in the overlay means "cancel", NOT the global detach — match the daemon's
    // `handle_quit_confirm`, which treats Ctrl-C like Esc.
    if is_detach(key) {
        send_overlay_cancel(req_tx);
        return QuitConfirmKey::Stay;
    }
    match key.code {
        KeyCode::Char('d') | KeyCode::Char('D') => {
            // Reset the daemon overlay → Chat first, then detach. Ordered queue keeps
            // the sequence, so a reattaching client sees Chat rather than the overlay.
            send_overlay_cancel(req_tx);
            let _ = req_tx.send(ClientRequest::Detach);
            QuitConfirmKey::ExitClient
        }
        KeyCode::Char('k') | KeyCode::Char('K') => {
            // Tell the daemon to shut down entirely; it tombstones every session,
            // releases locks, unlinks the socket, and self-exits gracefully.
            let _ = req_tx.send(ClientRequest::QuitDaemon);
            QuitConfirmKey::ExitClient
        }
        KeyCode::Esc => {
            send_overlay_cancel(req_tx);
            QuitConfirmKey::Stay
        }
        // No text entry: swallow every other key (don't forward) so nothing leaks.
        _ => QuitConfirmKey::Stay,
    }
}

/// Forward a bare `Esc` so the daemon's `/quit` overlay cancels back to Chat. Used by
/// both the explicit cancel and the detach reset (so the daemon never lingers in
/// QuitConfirm with no input source after the client leaves).
fn send_overlay_cancel(req_tx: &Sender<ClientRequest>) {
    let _ = req_tx.send(ClientRequest::SendKey(KeyWire::from(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::empty(),
    ))));
}

// ─── transport bridge tasks (mirror crate::ipc::conn, client-side) ───────────

/// Reader task: decode framed [`DaemonFrame`]s off the socket and push them onto the
/// loop's incoming channel. On socket EOF / cap violation / decode error it returns,
/// dropping `frame_tx` — the loop's `try_recv` then observes `Disconnected` and exits.
/// `read_frame_from` enforces [`crate::ipc::proto::MAX_FRAME_BYTES`] on every prefix.
async fn reader_task(
    mut read_half: tokio::net::unix::OwnedReadHalf,
    frame_tx: Sender<DaemonFrame>,
) {
    let mut reader = FrameReader::new();
    // `while let Ok(..)` ends the loop on EOF / cap violation / read error (the
    // daemon closed or misbehaved); a malformed-frame decode or a gone loop breaks.
    while let Ok(bytes) = frame::read_frame_from(&mut read_half, &mut reader).await {
        match serde_json::from_slice::<DaemonFrame>(&bytes) {
            // Forward the frame; a send error means the loop is gone (client
            // exiting) -> stop reading.
            Ok(frame) => {
                if frame_tx.send(frame).is_err() {
                    break;
                }
            }
            // A malformed frame from the daemon is a protocol fault; stop the
            // connection rather than guess (the loop sees the dropped sender).
            Err(_) => break,
        }
    }
    // Dropping `frame_tx` here signals the loop the connection is gone.
}

/// Writer task: drain the loop's outbound [`ClientRequest`] queue to the socket on a
/// short interval until the queue closes (the loop dropped its sender at exit) or a
/// write fails.
///
/// The `req_rx` borrow is confined to the synchronous collect step (no `.await` while
/// it is held), then the batch is written — the same collect-then-write that keeps
/// the future `Send` despite `std::sync::mpsc::Receiver` being `!Sync` (see
/// `conn::write_loop`).
///
/// # Drain-on-close (final-frame guarantee)
///
/// When `try_recv` reports `Disconnected` the loop has dropped `req_tx` at teardown,
/// after queuing the shutdown frame(s) (`Detach`, and — on `/quit` `[k]` — a
/// `QuitDaemon` ahead of it). Those frames may still be sitting in the channel, so
/// this task does NOT bail on close: it collects EVERY remaining request in the same
/// pass (the `Disconnected` arm only stops the collect, it does not discard what was
/// already drained) and writes the full batch — `write_frame_to` flushes each frame —
/// BEFORE returning. The teardown joins this task (bounded) so the runtime is not
/// dropped until this final flush completes, which is what guarantees the daemon
/// actually receives `QuitDaemon` instead of being orphaned.
async fn writer_task(
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    req_rx: Receiver<ClientRequest>,
) {
    let mut poll = tokio::time::interval(REQ_POLL);
    loop {
        poll.tick().await;

        // Collect every queued request WITHOUT awaiting while `req_rx` is borrowed.
        // On `Disconnected` keep everything drained so far (the final shutdown
        // frames) and write them below — closing the channel must never drop a
        // queued request, only end the polling loop after this last flush.
        let mut batch: Vec<ClientRequest> = Vec::new();
        let mut closed = false;
        loop {
            match req_rx.try_recv() {
                Ok(req) => batch.push(req),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    closed = true; // loop exited and dropped its sender
                    break;
                }
            }
        }

        // Write the batch (does not touch `req_rx`). Stop on a dead socket.
        // `write_frame_to` flushes each frame, so a successful write is on the wire.
        for req in &batch {
            let bytes = match serde_json::to_vec(req) {
                Ok(b) => b,
                // A request that can't serialise is a client bug, not a transport
                // fault — skip it rather than tear down the connection.
                Err(_) => continue,
            };
            if frame::write_frame_to(&mut write_half, &bytes).await.is_err() {
                return; // dead socket
            }
        }
        // Channel closed AND the final drained batch is flushed: the shutdown
        // frame(s) are on the wire, so it is safe to return (the teardown join then
        // completes and the runtime is dropped).
        if closed {
            break;
        }
    }
}
