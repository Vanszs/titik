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
//! The render loop is SYNCHRONOUS (crossterm draw + input poll) and runs on a FIXED
//! ~60fps frame cadence that is DECOUPLED from the socket: it never blocks on a read
//! or a write. Socket I/O lives in two tokio tasks bridged over `std::sync::mpsc`:
//! - a READER task: `read_frame_from` -> decode [`DaemonFrame`] -> push onto the
//!   loop's incoming `std::sync::mpsc::Sender<DaemonFrame>`. On EOF/error it drops
//!   the sender, which the loop observes as the daemon going away. The loop drains
//!   this channel NON-BLOCKING (`try_recv`) once per frame, so a slow/quiet daemon
//!   can never stall a paint.
//! - a WRITER task: owns the outbound `std::sync::mpsc::Receiver<ClientRequest>`,
//!   polls it on a short interval, and writes each as a frame. (Same `!Sync`
//!   reasoning as `conn::write_loop`: a `std::sync::mpsc::Receiver` held across an
//!   await makes the future non-`Send`; collect-then-write keeps it off the await.)
//!   The render loop only ever PUSHES onto this channel — it never awaits a socket
//!   write — so a typed key is enqueued in O(1) and the frame proceeds.
//!
//! # Why render is decoupled from the socket (the "broken ship" fix)
//!
//! An earlier loop redrew only when a frame changed the shadow (`dirty`-gated) AND
//! blocked inside `event::poll(timeout)` for up to the poll interval. The effect was
//! that animations (the status "comet", the loading spinner) only advanced at the
//! daemon's frame rate / the poll cadence, and every keystroke round-tripped to the
//! daemon before it appeared — laggy and jittery. The fix (same medicine as the
//! tool-call freeze): the render loop runs at a FIXED ~16ms cadence, drains all
//! pending frames non-blocking, advances every animation from a LOCAL monotonic
//! clock (`Instant::elapsed()` read at draw time — never daemon ticks), repaints
//! UNCONDITIONALLY (ratatui's buffer diff makes an unchanged frame ~free), then
//! polls input with a ZERO timeout. No socket operation can block a frame.
//!
//! # Local input echo (render-ahead)
//!
//! For the PLAIN composer edits (typing a char, Backspace, Left/Right/Home) the loop
//! applies the keystroke to the shadow's `input`/`cursor` IMMEDIATELY — the SAME
//! mutation `controller::input` would make — AND forwards the key. The daemon's
//! authoritative [`StateDelta::InputChanged`] (or any full Snapshot) reconciles on a
//! later frame and ALWAYS wins, so a mispredicted echo self-corrects within a frame
//! or two. Only the unambiguous text edits are echoed; mode-changing / submitting /
//! history / completion keys (Enter, Up/Down, Tab, Esc, `$`-on-empty, Ctrl-anything)
//! are NOT faked locally — they depend on daemon-side state, so they are forwarded
//! and the resulting snapshot drives the shadow.
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

use std::io::{stdout, Write};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::app::mode::agents::{
    AgentEditField, AgentScope, AgentSubMode, AgentsState, ModelPickerState, ToolPickerState,
};
use crate::app::mode::editor::TextEditorState;
use crate::app::mode::settings::{
    ModelDraft, ModelModal, PathPicker, PickerMode, ProviderDraft, ProviderModal, RolePickerState,
    SettingsState,
};
use crate::app::mode::{
    CookingEntry, EffortPickerState, HistoryEntry, HubPane, KeyInputForm, LoadingState, Mode,
    PickerState, QuitConfirmState, RewindEntry, RewindState, SessionHub, UsageMetric, UsageNavState,
    UsageRange, UsageView, WarmStatus,
};
use crate::app::state::{AppState, SessionRuntime, ToastKind};
use crate::app::subagent::{PendingSubagent, SubAgent, SubAgentStatus};
use crate::dto::chat::Role;
use crate::dto::openrouter::{ModelEndpoint, ModelPricing};
use crate::ipc::frame::{self, FrameReader};
use crate::ipc::proto::{
    AgentModelPickerSnapshot, AgentsSnapshot, ClientRequest, DaemonEvent, DaemonFrame,
    KeyInputSnapshot, KeyWire, LoadingSnapshot, ModeSnapshot, ModelModalSnapshot, PathPickerSnapshot,
    PickerSnapshot, RewindSnapshot, SessionHubSnapshot, SessionSnapshot, SettingsSnapshot,
    StateDelta, StateSnapshot, SubAgentSnapshot, TextEditorSnapshot, ToolPickerSnapshot,
    UsageSnapshot, WarmStatusWire,
};
use crate::model::app_config::{ApiType, ModelEntry, ModelRole, ProviderConn, ThemeMode};
use crate::model::conversation::Conversation;
use crate::model::session::Session;
use crate::model::settings::{InternetMode, Settings};
use crate::model::store;
use crate::model::store::SessionMeta;
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

/// How long the pre-render build-skew handshake waits for the daemon's first
/// [`DaemonEvent::Hello`] frame before giving up and proceeding UNVERIFIED (task
/// #142). Generous relative to the daemon's sub-ms attach reply, but bounded so a
/// wedged / pre-Hello daemon can never hang the client before it even paints. On a
/// timeout the client renders against whatever daemon answered (it never restarts on
/// a mere absence — only on a CONFIRMED mismatch).
const HELLO_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// One live daemon connection: the bridge channels + writer join handle the render
/// loop and teardown drive, plus the frames the pre-render handshake already pulled
/// off the wire and the daemon version it observed.
struct Connection {
    /// Incoming daemon frames (reader task -> render loop).
    frame_rx: Receiver<DaemonFrame>,
    /// Outgoing client requests (render loop -> writer task).
    req_tx: Sender<ClientRequest>,
    /// Writer task handle, joined at teardown so the final `Detach`/`QuitDaemon`
    /// flushes before the runtime is dropped.
    writer_handle: tokio::task::JoinHandle<()>,
    /// Frames the handshake read off `frame_rx` while hunting for `Hello` (normally
    /// none — `Hello` is the first frame — but any that arrived first are carried here
    /// so the render loop applies them BEFORE its own drain and no frame/seq is lost).
    prebuffered: Vec<DaemonFrame>,
    /// The daemon's reported build fingerprint, or `None` if no `Hello` arrived within
    /// the handshake window (a daemon predating the handshake, or a slow one).
    daemon_version: Option<String>,
}

/// Connect to the daemon, spawn the I/O bridge, send `Attach`, and run the pre-render
/// build-skew handshake (task #142): read frames until the daemon's first
/// [`DaemonEvent::Hello`] (bounded by [`HELLO_HANDSHAKE_TIMEOUT`]), recording its
/// reported fingerprint. Returns a live [`Connection`]; the CALLER compares
/// `daemon_version` to its own fingerprint and decides whether to restart+reconnect.
///
/// The handshake is synchronous and runs BEFORE any terminal setup so a stale-daemon
/// restart happens cleanly on the normal screen. Frames that arrive ahead of `Hello`
/// (defensive — the daemon emits `Hello` first) are stashed in `prebuffered` for the
/// render loop to apply first, so the seq stream the loop sees stays gap-free.
fn connect_attach_and_handshake(
    handle: &tokio::runtime::Handle,
    sock_path: &std::path::Path,
) -> Result<Connection> {
    // Connect first so a missing daemon fails BEFORE we touch the terminal (no
    // alt-screen flash on "no daemon"). The connected stream is split into the two
    // task halves below.
    let stream = handle
        .block_on(async { crate::ipc::client::connect(sock_path).await })
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

    // Send the Attach handshake; the daemon answers with a `Hello` (build-skew
    // fingerprint) FOLLOWED by the initial full Snapshot. Carry THIS client's launch
    // cwd so the daemon does pwd-aware session selection (stage 3): launching from a
    // NEW dir foregrounds/loads/creates a session for THAT dir, not the daemon's last
    // one. `current_dir` failing is non-fatal — `None` just keeps the daemon's foreground.
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let _ = req_tx.send(ClientRequest::Attach {
        foreground_id: None,
        cwd,
    });

    // Pre-render handshake: pull frames until the daemon's `Hello` (bounded). `Hello`
    // is normally the very first frame, so this typically reads exactly one. Any
    // non-`Hello` frame seen first is buffered for the render loop (so nothing is lost
    // and the seq stays monotonic). A timeout / closed socket ends the wait with
    // `daemon_version = None` — the caller proceeds unverified rather than restarting.
    let mut prebuffered: Vec<DaemonFrame> = Vec::new();
    let mut daemon_version: Option<String> = None;
    let deadline = Instant::now() + HELLO_HANDSHAKE_TIMEOUT;
    // Loop until the Hello arrives, the socket closes, or the window elapses
    // (`checked_duration_since` returns `None` once `deadline` is in the past).
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match frame_rx.recv_timeout(remaining) {
            Ok(frame) => match frame.event {
                DaemonEvent::Hello { version } => {
                    daemon_version = Some(version);
                    break;
                }
                // A non-Hello frame arrived first: keep it for the render loop to apply
                // before its own drain, then keep waiting for the Hello.
                _ => prebuffered.push(frame),
            },
            // Timed out, or the reader task dropped its sender (socket closed): stop
            // waiting. `None` daemon_version => unverified; the caller won't restart.
            Err(_) => break,
        }
    }

    Ok(Connection {
        frame_rx,
        req_tx,
        writer_handle,
        prebuffered,
        daemon_version,
    })
}

/// Attach to a running daemon and run the thin render+forward client.
///
/// Connects to the daemon socket (an `Err` means no daemon is up — surfaced to the
/// caller, which prints it), spawns the reader/writer bridge tasks, sends
/// [`ClientRequest::Attach`], runs the build-skew handshake (task #142), then enters
/// the synchronous render loop. Returns when the user detaches (Ctrl-C) or the
/// daemon's socket closes; the terminal is restored by [`TerminalGuard`]'s drop and
/// the runtime is dropped last.
///
/// # Build-skew auto-restart (task #142)
///
/// The koma daemon outlives a rebuild, so a freshly-built client can attach to a
/// daemon still running OLD code and silently render its stale frames (this already
/// caused a phantom `/agents` bug). On connect the client compares its OWN build
/// fingerprint ([`store::build_fingerprint`], computed fresh now) against the
/// daemon's reported one (the `Hello` value, which the daemon captured AT ITS
/// STARTUP). On a mismatch it restarts the stale daemon via the SAME machinery
/// `koma daemon restart` uses ([`super::manage::restart_daemon`]) and reconnects.
///
/// LOOP GUARD: the auto-restart fires AT MOST ONCE per launch. If the freshly-spawned
/// daemon STILL mismatches (it shouldn't — it was just built from the current binary),
/// the client prints an error and renders against it anyway rather than restart-looping
/// forever. A daemon that sends no `Hello` (predates the handshake, or is slow) is
/// never restarted on that absence alone — only a CONFIRMED mismatch triggers a restart.
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

    // THIS client's build fingerprint, read fresh now (the on-disk binary as it exists
    // at launch). Compared below to each daemon's reported `Hello` to detect a daemon
    // running stale code.
    let my_fingerprint = store::build_fingerprint();

    // Connect + attach + handshake, restarting a version-skewed daemon AT MOST ONCE
    // (the loop guard). On a confirmed mismatch we restart the stale daemon and
    // reconnect; on the (unexpected) second mismatch we give up and render against it.
    let mut conn = connect_attach_and_handshake(&handle, &sock_path)?;
    let mut already_restarted = false;
    while conn
        .daemon_version
        .as_deref()
        .is_some_and(|v| v != my_fingerprint)
    {
        if already_restarted {
            // The just-restarted daemon STILL reports a different fingerprint. This
            // shouldn't happen (it was spawned from the current binary); don't loop
            // forever — warn and render against it.
            eprintln!(
                "koma: daemon still reports a different build after a restart; \
                 continuing against it"
            );
            break;
        }
        eprintln!("koma: daemon running stale code — restarting...");
        already_restarted = true;

        // Tear down the stale connection's bridge before restarting: drop our request
        // sender (the writer drains + exits) and let the reader task observe the
        // daemon's death as EOF. Both old tasks self-terminate; the runtime persists
        // for the reconnect below.
        drop(conn.req_tx);
        drop(conn.frame_rx);

        // Reuse the EXACT `koma daemon restart` path (kill escalation + spawn-and-
        // confirm). A failure here is fatal — we can't recover a usable daemon.
        super::manage::restart_daemon()
            .map_err(|e| anyhow::anyhow!("failed to restart the stale koma daemon: {e:#}"))?;

        // Reconnect to the freshly-spawned daemon and re-handshake.
        conn = connect_attach_and_handshake(&handle, &sock_path)?;
    }

    // Unpack the connection we settled on (fresh-built match, an unverified daemon, or
    // a post-restart daemon we chose to accept).
    let Connection {
        frame_rx,
        req_tx,
        writer_handle,
        prebuffered,
        daemon_version: _,
    } = conn;

    // Terminal setup — identical to the local TUI (`run`). Guard first so a failure
    // anywhere after still restores the terminal.
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Run the synchronous render loop with the runtime context entered on THIS thread,
    // SCOPED so the `EnterGuard` is dropped the instant the loop returns — BEFORE the
    // teardown's `handle.block_on` below (which panics if called while a runtime
    // context is entered). The context is needed only so a snapshot rebuild can mint
    // the inert `AbortHandle` a reconstructed shadow `SubAgent` carries (`tokio::spawn`
    // needs a runtime in scope — see `shadow_subagent`); the loop itself stays sync.
    let result = {
        let _rt_ctx = handle.enter();
        render_loop(&mut terminal, &frame_rx, &req_tx, prebuffered)
    };

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

/// Target frame budget: ~60fps. Each loop iteration paints once and then sleeps the
/// remainder of this budget, so animations advance smoothly from the local clock and
/// the client never busy-spins. This is the FIXED cadence the render loop runs at,
/// independent of the daemon's frame rate (the socket is drained non-blocking).
const FRAME_BUDGET: Duration = Duration::from_millis(16);

/// The synchronous render loop, decoupled from the socket and paced at ~60fps.
///
/// Each frame, in order: (a) drain ALL pending [`DaemonFrame`]s non-blocking and
/// apply them (snapshot/delta or seq-gap -> Resync); (b) advance animations from a
/// LOCAL monotonic clock (reconcile the comet's `work_since`, re-anchor the loading
/// spinner) — never from daemon ticks; (c) repaint the shadow UNCONDITIONALLY (the
/// ratatui buffer diff makes an unchanged frame ~free); (d) poll terminal input with
/// a ZERO timeout and handle it (local echo for the plain composer edits, forward the
/// rest). The loop NEVER blocks on the socket: if no frame arrived it still paints and
/// animations still advance. Returns when the user detaches (Ctrl-C) or the socket
/// closes.
fn render_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    frame_rx: &Receiver<DaemonFrame>,
    req_tx: &Sender<ClientRequest>,
    prebuffered: Vec<DaemonFrame>,
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

    // Apply any frames the pre-render handshake pulled off the wire while hunting for
    // `Hello` (task #142) BEFORE the live drain, through the SAME `apply_frame` path so
    // seq seeding + snapshot/delta handling are identical. Normally empty (the daemon
    // sends `Hello` first), so usually a no-op; when non-empty these are the lowest-seq
    // frames and must be folded first to keep the seq stream gap-free. An `EnterSelect`
    // can't occur this early (it needs a forwarded `/select` first), so a throwaway
    // `select_requested` here is never acted on.
    {
        let mut select_requested = false;
        for frame in prebuffered {
            apply_frame(
                frame,
                &mut shadow,
                &mut expected,
                &mut seeded,
                &mut awaiting_resync,
                &mut select_requested,
                req_tx,
            );
        }
    }

    loop {
        // Pace to ~60fps: stamp the frame start, do the work, sleep the remainder.
        let frame_start = Instant::now();

        // Latched by `apply_frame` when a `DaemonEvent::EnterSelect` arrives this drain
        // pass: the daemon asked THIS (controller) client to run the `/select` transcript
        // dump on its own terminal. Acted on AFTER the drain (we own `terminal` here).
        let mut select_requested = false;

        // --- (a) drain every queued incoming frame (NON-BLOCKING) ---
        // try_recv never blocks, so a quiet daemon can't stall the paint below. The
        // per-frame `dirty` bookkeeping is gone: we repaint unconditionally, so the
        // only thing that matters here is keeping the shadow current.
        loop {
            match frame_rx.try_recv() {
                Ok(frame) => {
                    apply_frame(
                        frame,
                        &mut shadow,
                        &mut expected,
                        &mut seeded,
                        &mut awaiting_resync,
                        &mut select_requested,
                        req_tx,
                    );
                }
                Err(TryRecvError::Empty) => break,
                // The reader task dropped its sender: the daemon's socket closed.
                // Nothing more will ever arrive — leave the client.
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // --- (a-bis) `/select` transcript dump (controller-side) ---
        // If the daemon signalled EnterSelect this pass, run the dump NOW — we hold the
        // `terminal`, so we can leave the alt-screen, print the shadow conversation,
        // block for a keypress, and re-enter. This is a synchronous, blocking detour
        // (exactly like the standalone loop's `/select`); the socket keeps buffering
        // frames meanwhile and the next pass drains them. A no-op if there is no shadow
        // session/conversation (the dump leaves the terminal exactly as it found it).
        if select_requested {
            client_select_dump(terminal, &shadow)?;
        }

        // --- (b) advance LOCAL animations from the monotonic clock ---
        // The comet shimmer + loading spinner derive their phase from
        // `Instant::elapsed()` read inside `view::draw`, so they advance every frame
        // for free once we repaint at 60fps below. Two things still need a nudge:
        // reconcile the comet's `work_since` on the rising/falling working edge (so it
        // starts/stops promptly between snapshots), and tick the loading splash's
        // local spinner counter (the daemon's projected `frame` is stale between
        // snapshots — drive it locally so the braille glyph cycles).
        advance_local_animations(&mut shadow);

        // Expire a locally-reconstructed toast once its TTL passes (the daemon never
        // sends a "toast cleared" delta; the client owns its own dismissal timer).
        if let Some((_, until, _)) = shadow.rest.toast.as_ref() {
            if Instant::now() >= *until {
                shadow.rest.toast = None;
            }
        }

        // --- (c) repaint UNCONDITIONALLY ---
        // ratatui computes the cell-level diff against the previous buffer, so an
        // unchanged frame flushes ~nothing; painting every frame is what lets the
        // local animations advance smoothly without any dirty-tracking.
        terminal.draw(|f| view::draw(f, &shadow))?;

        // --- (d) poll + handle terminal input (ZERO timeout, never blocks) ---
        // Drain EVERY buffered event this frame so fast typing / paste never lag.
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
                    if is_detach(&key) {
                        return Ok(());
                    }
                    // Render-ahead: apply the plain composer edits to the shadow NOW
                    // (the daemon's authoritative InputChanged reconciles later), then
                    // forward the key verbatim for the daemon to interpret. Only the
                    // unambiguous text edits are echoed — see `local_echo`.
                    local_echo(&mut shadow, &key);
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
                    }
                    MouseEventKind::ScrollDown => {
                        for _ in 0..3 {
                            shadow.rest.scroll_down();
                        }
                    }
                    _ => {}
                },
                // A resize just needs the next unconditional paint to relayout.
                Event::Resize(_, _) => {}
                // Bracketed paste: forward the WHOLE text as one Paste request so the
                // daemon runs the SAME `handle_paste` the local TUI uses. This is what
                // makes path-image paste work remotely — a pasted image-file path is
                // detected daemon-side and ingested into the session's `images/` dir as
                // an `[Image #N]` attachment, and multi-line text keeps its newlines
                // (CRLF-normalised). Forwarding char-by-char (the old behaviour) ran the
                // daemon's plain `Char` handler instead, which can't detect an image
                // path and mangles line endings. NOT echoed locally: a paste may become
                // a marker rather than literal text, so faking the raw text would
                // flicker — the daemon's InputChanged/Snapshot reconciles within a frame.
                Event::Paste(text) => {
                    let _ = req_tx.send(ClientRequest::Paste { text });
                }
                _ => {}
            }
        }

        // --- frame pacing: sleep the remainder of the ~16ms budget ---
        // Keeps the loop at ~60fps instead of busy-spinning. If a frame overran the
        // budget (a big snapshot rebuild) we skip the sleep and proceed immediately.
        if let Some(rem) = FRAME_BUDGET.checked_sub(frame_start.elapsed()) {
            std::thread::sleep(rem);
        }
    }
}

/// Run the `/select` transcript dump on the CLIENT's terminal (the controller-side
/// half of the `/select` hand-off — see [`crate::ipc::proto::DaemonEvent::EnterSelect`]).
///
/// The daemon owns no TTY, so when a forwarded `/select` set its `select_pending` flag
/// it signalled THIS client to perform the dump. This mirrors the standalone loop's
/// [`super::super::event_loop::drains`] `enter_select`/`exit_select`, but sourced from
/// the SHADOW conversation and self-contained (it blocks for the return keypress here
/// rather than threading a `select_active` state through the render loop):
///   1. leave the alt-screen + disable mouse capture,
///   2. print the foreground shadow session's conversation as plain text (so the user
///      can select/copy with the terminal's native selection) — raw mode stays on, so
///      lines are terminated with `\r\n`,
///   3. block until the user presses any key,
///   4. re-enter the alt-screen + mouse capture and force a full repaint
///      (`terminal.clear()`), so the next loop pass redraws the live shadow cleanly.
///
/// Robustness: if the shadow has no foreground session/conversation there is nothing to
/// dump, so it returns immediately WITHOUT touching the terminal — the alt-screen is
/// never left, so the terminal can't be stranded in a half-restored state.
fn client_select_dump(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    shadow: &AppState,
) -> Result<()> {
    // No shadow session → nothing to dump. Return before any terminal mutation so we
    // never leave the alt-screen with nothing to show (clean no-op).
    if shadow.rest.fg().session.is_none() {
        return Ok(());
    }

    // (1) Drop to the normal screen so the printed transcript uses the scrollback the
    // user can select from.
    execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture)?;

    // (2) Print the conversation as plain text (raw mode is on → `\r\n`). Mirrors
    // `drains::enter_select`'s formatting exactly: skip System/Tool, label you/ai.
    let mut out = stdout();
    if let Some(sess) = shadow.rest.fg().session.as_ref() {
        for m in sess.conversation.messages() {
            let label = match m.role {
                Role::System | Role::Tool => continue,
                Role::User => "you",
                Role::Assistant => "ai",
            };
            write!(out, "\r\n{label}:\r\n")?;
            for line in m.content.split('\n') {
                write!(out, "{line}\r\n")?;
            }
        }
    }
    write!(out, "\r\n-- copy with your mouse, then press any key to return --\r\n")?;
    out.flush()?;

    // (3) Block until a key is pressed. Read events (blocking) and ignore non-key ones
    // (a stray resize/mouse must NOT count as the "any key" return).
    loop {
        if let Event::Key(_) = event::read()? {
            break;
        }
    }

    // (4) Restore the alt-screen + mouse and force a full repaint next draw.
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;
    Ok(())
}

/// Advance the LOCAL-clock animations on the shadow once per frame.
///
/// The client owns NO daemon ticks, so animations that the local TUI advances from
/// its event loop must be advanced here from the client's own monotonic clock:
///
/// - **Comet shimmer (`work_since`).** Reconcile it on the rising/falling working
///   edge exactly like the local loop's `service_global`: stamp `now` when the
///   foreground session starts working (and isn't paused on a y/n approval) and it
///   isn't already running; clear it when work ends or an approval takes over. The
///   travelling head + elapsed counter then derive from `work_since.elapsed()` at
///   draw time. (A snapshot may also set `work_since` from the daemon's anchored
///   `work_elapsed_ms`; this only fills the rising/falling edges BETWEEN snapshots so
///   the comet never freezes or lingers.)
/// - **Loading splash spinner (`frame`).** The braille glyph is indexed by
///   `frame % 10`; the daemon's projected `frame` is frozen between snapshots, so
///   tick it locally each frame to keep the spinner rotating (the footer's elapsed
///   counter already derives from `started.elapsed()`).
fn advance_local_animations(shadow: &mut AppState) {
    // Comet: rising/falling-edge reconcile (mirrors `service_global`).
    reconcile_work_clock(shadow);

    // Loading splash: keep the local spinner counter rotating between snapshots.
    if let Mode::Loading(s) = &mut shadow.mode {
        s.frame = s.frame.wrapping_add(1);
    }
}

/// Apply the SAFE subset of composer edits to the shadow immediately (render-ahead),
/// so typing appears with zero round-trip. The key is ALSO forwarded to the daemon by
/// the caller; the daemon's authoritative [`StateDelta::InputChanged`] (or a full
/// Snapshot) reconciles on a later frame and ALWAYS wins, so a mispredicted echo is
/// self-correcting.
///
/// Only edits that PURELY mutate `input`/`cursor` with no dependence on daemon-side
/// state are echoed — using the EXACT same `AppStateRest` helpers `controller::input`
/// calls, so the local result matches the daemon's byte-for-byte:
///   - a plain `Char(c)` (no Ctrl) — EXCEPT `$` on an empty input, which opens the
///     sub-agents panel daemon-side (a mode change, not a text edit);
///   - `Backspace`, and the pure caret moves `Left` / `Right` / `Home`.
///
/// Everything else is deliberately NOT echoed (forwarded only), because its meaning
/// depends on state the client doesn't authoritatively own: `Enter` (submit / slash /
/// palette-complete), `Up`/`Down` (history recall / palette nav / multiline caret),
/// `End` (scroll-to-bottom when empty, else caret), `Tab`/`BackTab` (completion /
/// mode toggle), `Esc` (interrupt / rewind), and any Ctrl-modified key. Those still
/// reconcile from the daemon's snapshot.
///
/// The echo is suppressed unless the shadow is in plain `Chat` with no modal surface
/// open (help / sub-agents panel / viewer / tool-approval), matching where the
/// daemon's chat composer actually consumes these keys.
fn local_echo(shadow: &mut AppState, key: &KeyEvent) {
    // Only echo in plain Chat with no modal overlay capturing keys. In any other mode
    // (or with a modal open) the daemon routes the key elsewhere, so faking a text
    // edit would desync until the next snapshot corrects it.
    if !matches!(shadow.mode, Mode::Chat) {
        return;
    }
    let rest = &mut shadow.rest;
    if rest.help_open
        || rest.subagents_open
        || rest.agent_viewer.is_some()
        || rest.fg().awaiting_approval
    {
        return;
    }
    // Never echo a Ctrl-modified key (Ctrl-J newline, Ctrl-V paste, interrupts, …);
    // those are gestures, not plain text the composer inserts at the caret.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return;
    }
    match key.code {
        // `$` on an EMPTY input opens the `$` panel daemon-side (not a typed char), so
        // don't echo it; with text present it is a normal character and echoes below.
        KeyCode::Char('$') if rest.input.is_empty() => {}
        KeyCode::Char(c) => rest.push_char(c),
        KeyCode::Backspace => rest.backspace(),
        KeyCode::Left => rest.cursor_left(),
        KeyCode::Right => rest.cursor_right(),
        KeyCode::Home => rest.cursor_home(),
        // Enter / Up / Down / End / Tab / BackTab / Esc / everything else: forwarded
        // only (handled above by NOT matching here) — the daemon snapshot reconciles.
        _ => {}
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
    select_requested: &mut bool,
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
            // resync and reseeds the shadow wholesale. (`snap` is boxed on the wire
            // to keep the `DaemonEvent` enum small; unbox it for `apply_snapshot`.)
            *awaiting_resync = false;
            apply_snapshot(shadow, *snap);
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
        // The controller's `/select` hand-off: the daemon (which owns no TTY) asks
        // THIS client to run the transcript dump on its own terminal. The dump leaves
        // the alt-screen + blocks on a keypress, which can't happen here (no `terminal`
        // handle, mid-frame-drain); just latch the request so the render loop performs
        // it after this drain pass completes. Non-visual to the shadow itself.
        DaemonEvent::EnterSelect => {
            *select_requested = true;
            false
        }
        // The build-skew handshake frame (task #142) is consumed BEFORE the render
        // loop, in the pre-render handshake (see `client_run`). If one still reaches
        // here (a re-attach mid-session re-emits it), it is non-visual: the version was
        // already verified at connect time, so just advance the seq and render nothing.
        DaemonEvent::Hello { .. } => false,
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
    // The on-demand model catalogue + the endpoint it was fetched for. The Settings
    // model modal + KeyInput step-1 search render their omnisearch dropdowns from
    // these; without them a remote client's dropdown would sit on `searching
    // models…` forever (it has no fetch path of its own).
    shadow.rest.models_cache = global.models_cache;
    shadow.rest.models_cache_endpoint = global.models_cache_endpoint;

    // Global theme + accent. `view::draw` frames every screen via
    // `theme::palette(&shadow.rest.config)` BEFORE dispatching to a mode renderer, so
    // without these the shadow's `config` stays at `AppConfig::default()` (Dark/green)
    // and a Light-theme or non-green daemon renders every label/border/highlight in
    // the wrong palette. Theme decodes from its wire token (reusing the Settings
    // helper, unknown → Dark); accent is an opaque palette key copied verbatim.
    shadow.rest.config.theme = shadow_theme(&global.theme);
    shadow.rest.config.accent = global.accent;
    // The shadow `AppConfig`'s registered-model + provider catalogue is populated
    // ONLY for the `/agents` screen (which resolves a chosen `model_uuid` to a
    // `name @ provider` label off `rest.config`), from that mode's KEYLESS projection.
    // Reset it here every snapshot so a stale catalogue from a previous Agents view
    // never lingers into another screen; the Agents arm below refills it when active.
    shadow.rest.config.models.clear();
    shadow.rest.config.providers.clear();

    // Full-screen sub-agent viewer + `$` panel state (rendered FROM Chat mode off the
    // foreground session's reconstructed `subagents`). Mirror the daemon's
    // `agent_viewer` index / scroll / follow + the panel open-state + selection so the
    // unmodified chat renderer takes the same full-screen-viewer / overlay branch.
    shadow.rest.agent_viewer = global.agent_viewer;
    shadow.rest.agent_viewer_scroll = global.agent_viewer_scroll;
    shadow.rest.agent_viewer_follow = global.agent_viewer_follow;
    shadow.rest.subagents_open = global.subagents_open;
    shadow.rest.subagent_sel = global.subagent_sel;
    // The `@`-file / `/`-command picker highlighted-row index — mirrored like
    // `subagent_sel` so Up/Down on either picker moves the highlight on the client
    // (without this the shadow `palette_sel` stays at 0 and the row never moves).
    shadow.rest.palette_sel = global.palette_sel;

    // Staged composer attachments (ingested daemon-side via path-paste / clipboard /
    // @-picker). The `[Image #N]` marker text already arrives in `input`; mirror the
    // attachment RECORDS too so the shadow composer matches the daemon's exactly.
    shadow.rest.pending_attachments = global.pending_attachments;
    // The precomputed `@`-file palette (the daemon ran `dir_cache.search` on its
    // index). The client's reconstructed `dir_cache` is empty, so the unmodified
    // file-palette view renders this projected list instead (see
    // `view::chat::render_file_palette`). `None` when the composer isn't on an
    // `@token`; seeding it every snapshot (including with `None`) means a stale list
    // never lingers after the `@token` is completed/cleared.
    shadow.rest.file_palette = global.file_palette;

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

    // Mode: reconstruct from the pure-data `ModeSnapshot` into REAL mode state so the
    // unmodified `view::draw` renders every screen faithfully — the client never
    // mutates these (input is forwarded), it only needs enough to DRAW. Chat is
    // payload-free. The QuitConfirm overlay is special-cased so the client can ALSO
    // intercept its lifecycle keys ([d] detach / [k] kill-all) locally (see
    // `render_loop`). With stage 3 EVERY variant carries its payload, so nothing falls
    // back to a blank Chat render any more.
    //
    // The `/usage` dashboard is special: its numbers come from the daemon's ledger,
    // which the client cannot read, so the projection carries the pre-fetched data —
    // seed `rest.usage_data` from it so the unmodified dashboard renders DB-free.
    // Clear it first so a stale projection never lingers into the next non-Usage mode.
    shadow.rest.usage_data = None;
    shadow.mode = match global.mode {
        ModeSnapshot::KeyInput(f) => Mode::KeyInput(shadow_key_input(f)),
        ModeSnapshot::SessionPicker(p) => Mode::SessionPicker(shadow_picker(p)),
        ModeSnapshot::SessionHub(h) => Mode::SessionHub(Box::new(shadow_session_hub(h))),
        ModeSnapshot::Chat => Mode::Chat,
        ModeSnapshot::Loading(s) => Mode::Loading(shadow_loading(s)),
        ModeSnapshot::Settings(s) => Mode::Settings(Box::new(shadow_settings(*s))),
        ModeSnapshot::Agents(a) => {
            // Seed the shadow config's KEYLESS catalogue so the agents view resolves
            // the model label (`name @ provider`) off `rest.config`, exactly as the
            // daemon does — without any API key (the reconstructed providers carry an
            // empty `api_key`; the client only resolves labels, never sends a request).
            shadow.rest.config.models = a
                .catalogue_models
                .iter()
                .map(|m| ModelEntry {
                    uuid: m.uuid.clone(),
                    name: m.name.clone(),
                    model_id: m.model_id.clone(),
                    provider_uuid: m.provider_uuid.clone(),
                    ..ModelEntry::default()
                })
                .collect();
            shadow.rest.config.providers = a
                .catalogue_providers
                .iter()
                .map(|p| ProviderConn {
                    uuid: p.uuid.clone(),
                    name: p.name.clone(),
                    endpoint: p.endpoint.clone(),
                    ..ProviderConn::default()
                })
                .collect();
            Mode::Agents(Box::new(shadow_agents(*a)))
        }
        ModeSnapshot::Effort(e) => Mode::Effort(Box::new(shadow_effort(e))),
        ModeSnapshot::Usage(u) => {
            let UsageSnapshot { view, range, metric, data } = *u;
            shadow.rest.usage_data = Some(data);
            Mode::Usage(Box::new(shadow_usage_nav(&view, &range, &metric)))
        }
        ModeSnapshot::MessageRewind(rw) => Mode::MessageRewind(Box::new(shadow_rewind(rw))),
        ModeSnapshot::QuitConfirm { working, total } => {
            Mode::QuitConfirm(Box::new(QuitConfirmState::new(working, total)))
        }
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
/// Both the QUEUED [`PendingSubagent`]s and the RUNNING/finished `subagents` are
/// reconstructed from their plain-data projections, so the `$` panel rows AND the
/// full-screen sub-agent viewer (both rendered FROM Chat mode) draw off real shadow
/// data. A live [`crate::app::subagent::SubAgent`] needs a tokio `AbortHandle` +
/// receiver, which require a runtime in scope — `client_run` enters the runtime
/// context for the render loop so [`shadow_subagent`] can mint inert ones (the
/// client never drives a sub-agent; the handle/rx exist only to satisfy the type).
fn shadow_session_runtime(s: &SessionSnapshot) -> SessionRuntime {
    let mut rt = SessionRuntime::new();
    rt.id = s.id.clone();
    rt.session = Some(shadow_session(s));
    // Mirror the daemon's effective cwd onto the shadow as the live override, so
    // the reconstructed runtime's `effective_cwd()` matches (the shadow session's
    // own `settings.workdir` isn't projected, so this is the only cwd source).
    // Empty only when the daemon had no session; leave the default `None` then.
    if !s.cwd.is_empty() {
        rt.active_cwd = Some(std::path::PathBuf::from(&s.cwd));
    }
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
    // The pending tool-call round + cursor so the Chat-mode approval overlay (gated
    // on `awaiting_approval`) renders the real paused call — name, args, payload
    // preview — exactly as the local TUI does. `ToolCall` is plain data; the client
    // never executes it (the y/n is forwarded as `ApproveTool`).
    rt.pending_tool_calls = s.pending_tool_calls.clone();
    rt.tool_idx = s.tool_idx;
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
    // Reconstruct the running/finished sub-agents (plain data + an inert handle/rx)
    // so the `$` panel list AND the full-screen viewer render off real shadow data.
    rt.subagents = s.subagents.iter().map(shadow_subagent).collect();
    rt
}

/// Rebuild a render-only [`SubAgent`] from its projection.
///
/// Carries every field the `$` panel + the full-screen viewer read (id, agent name,
/// label, status, transcript, structured messages). The two runtime-bound fields a
/// live `SubAgent` requires — the `abort` [`tokio::task::AbortHandle`] and the `rx`
/// event receiver — are minted INERT here: the client never drains `rx` and never
/// aborts (it forwards every key to the daemon, which owns the real sub-agent), so a
/// no-op aborted task's handle + a fresh unused channel satisfy the type without ever
/// being driven. `tokio::spawn` needs a runtime in scope, which `client_run` enters
/// for the render loop. The non-rendered bookkeeping (`model_id`, `tool_call_id`,
/// usage counters) is left empty/zero — the viewer and panel never read it.
fn shadow_subagent(sa: &SubAgentSnapshot) -> SubAgent {
    // Inert abort handle: a task that completes immediately; its handle is never used
    // to abort anything (the daemon owns the real task). Cheap + completes at once.
    let abort = tokio::spawn(std::future::ready(())).abort_handle();
    // Fresh receiver the client never drains (the daemon folds real events; a shadow
    // sub-agent's content arrives wholesale via the next snapshot's `messages`).
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    SubAgent {
        id: sa.id,
        agent_name: sa.name.clone(),
        label: sa.label.clone(),
        // Not rendered by the panel/viewer; left blank (the wire omits it).
        model_id: String::new(),
        status: shadow_subagent_status(&sa.status),
        abort,
        rx,
        transcript: sa.transcript.clone(),
        messages: sa.messages.clone(),
        tool_call_id: None,
        usage_tokens_in: 0,
        usage_tokens_out: 0,
        usage_cost: 0.0,
    }
}

/// Map a wire sub-agent status string back to a [`SubAgentStatus`].
///
/// The daemon flattens the lifecycle enum to a short string (see
/// `ipc::snapshot::subagent_snapshot`): `"running"` / `"done"` / `"killed"` /
/// `"error: <detail>"`. The `Done` final-answer payload is NOT carried (the viewer —
/// the projection's target — renders the transcript + the short status tag, neither of
/// which uses it), so a reconstructed `Done` carries an empty answer; an `Error`
/// keeps its detail (the panel's status line shows it). Unknown → `Running` (the
/// safe "still going" default, never lost).
fn shadow_subagent_status(status: &str) -> SubAgentStatus {
    match status {
        "done" => SubAgentStatus::Done(String::new()),
        "killed" => SubAgentStatus::Killed,
        s if s.starts_with("error:") => {
            SubAgentStatus::Error(s.trim_start_matches("error:").trim().to_string())
        }
        _ => SubAgentStatus::Running,
    }
}

/// Reconstruct a minimal [`Session`] from a [`SessionSnapshot`] for rendering.
///
/// Only the fields the chat view reads are meaningful: `name` (the input-tab label),
/// `conversation` (the transcript), and `settings.model` (the model-name row). The
/// path / pwd_hash / api_key are render-irrelevant on the client and left empty —
/// the client never saves, sends, or locks anything.
fn shadow_session(s: &SessionSnapshot) -> Session {
    // Seed `settings.model` with the daemon-side resolved model id projected in the
    // snapshot. The client's shadow config is keyless + catalogue-cleared, so
    // resolve_role on the client would return empty; using the projected id means
    // the chat header always shows the same model name the daemon resolved.
    let settings = Settings {
        name: s.name.clone(),
        model: s.resolved_model_id.clone(),
        ..Default::default()
    };

    // Re-attach the display-only reasoning the wire carried out-of-band. The
    // `ChatMessage::reasoning` field is `#[serde(skip)]`, so every deserialised
    // message arrives with `reasoning: None`; without this fold-back a committed
    // turn's thinking block would never render on the client (it would only show
    // while the live `stream_reasoning` buffer streamed, then vanish on finalize).
    // The side-channel is index-aligned with `messages`; a missing/short entry
    // (the common no-reasoning case ships an empty vec) leaves `reasoning` at None.
    let mut messages = s.messages.clone();
    for (i, msg) in messages.iter_mut().enumerate() {
        if let Some(Some(reasoning)) = s.committed_reasoning.get(i) {
            msg.reasoning = Some(reasoning.clone());
        }
    }

    Session::new(
        s.id.clone(),
        std::path::PathBuf::new(),
        String::new(),
        settings,
        Conversation::from_messages(messages),
    )
}

// ─── mode reconstruction (stage 2: core interactive modes) ───────────────────
//
// Each rebuilds a REAL mode-state value from its wire projection so the unmodified
// `view::draw` renders it. The client never mutates these (input is forwarded to
// the daemon); they only need to be faithful enough to DRAW. None hold a channel /
// `Instant`-clock that must keep ticking except `Loading::started`, which is
// re-anchored from the projected elapsed-ms so its footer counter matches.

/// Rebuild the first-run wizard form ([`KeyInputForm`]) from its projection.
fn shadow_key_input(f: KeyInputSnapshot) -> KeyInputForm {
    KeyInputForm {
        step: f.step,
        field: f.field,
        endpoint: f.endpoint,
        api_key: f.api_key,
        model: f.model,
        query: f.query,
        result_sel: f.result_sel,
        first_run: f.first_run,
        from_picker: f.from_picker,
    }
}

/// Rebuild the loading splash ([`LoadingState`]) from its projection. The footer's
/// elapsed clock is re-anchored (`now - elapsed`) so it continues from the daemon's
/// phase rather than resetting to 0 on each snapshot.
fn shadow_loading(s: LoadingSnapshot) -> LoadingState {
    LoadingState {
        started: Instant::now() - Duration::from_millis(s.elapsed_ms),
        frame: s.frame,
        workspace: shadow_warm_status(s.workspace),
        awareness: shadow_warm_status(s.awareness),
    }
}

/// Map a [`WarmStatusWire`] back to a [`WarmStatus`].
fn shadow_warm_status(w: WarmStatusWire) -> WarmStatus {
    match w {
        WarmStatusWire::Pending => WarmStatus::Pending,
        WarmStatusWire::Running => WarmStatus::Running,
        WarmStatusWire::Done(d) => WarmStatus::Done(d),
        WarmStatusWire::Skipped => WarmStatus::Skipped,
        WarmStatusWire::Failed => WarmStatus::Failed,
    }
}

/// Rebuild the two-pane session hub ([`SessionHub`]) from its projection.
///
/// The COOKING rows' live `idx` (the daemon's `sessions` index, used on Enter) is
/// not projected and not rendered, so reconstructed rows carry `0` for it; the
/// HISTORY rows' live `path` is likewise daemon-only, rebuilt as an empty path. The
/// client never acts on these — Enter is forwarded for the daemon to resolve.
fn shadow_session_hub(h: SessionHubSnapshot) -> SessionHub {
    SessionHub {
        cooking: h
            .cooking
            .into_iter()
            .map(|c| CookingEntry {
                idx: 0, // daemon-side index; not rendered, resolved on the daemon
                name: c.name,
                working: c.working,
                is_foreground: c.is_foreground,
            })
            .collect(),
        history: h
            .history
            .into_iter()
            .map(|e| HistoryEntry {
                path: std::path::PathBuf::new(), // daemon-side load target; not rendered
                name: e.name,
                last_active: std::time::UNIX_EPOCH
                    + Duration::from_secs(e.last_active_secs),
            })
            .collect(),
        focus: if h.focus_cooking {
            HubPane::Cooking
        } else {
            HubPane::History
        },
        cooking_selected: h.cooking_selected,
        history_selected: h.history_selected,
    }
}

/// Rebuild the `/settings` dashboard ([`SettingsState`]) from its projection — the
/// largest reconstruction. Every draft + list + modal + picker is restored so the
/// settings view (and its pure helper methods, which recompute from these same
/// fields) renders exactly as the daemon's would.
fn shadow_settings(s: SettingsSnapshot) -> SettingsState {
    SettingsState {
        cat: s.cat,
        field: s.field,
        in_detail: s.in_detail,
        editing: s.editing,
        api_key: s.api_key,
        model: s.model,
        provider: s.provider,
        name: s.name,
        theme: shadow_theme(&s.theme),
        accent: s.accent,
        workdir: s.workdir,
        awareness_enabled: s.awareness_enabled,
        awareness_inherit: s.awareness_inherit,
        awareness_model: s.awareness_model,
        awareness_provider: s.awareness_provider,
        classifier_enabled: s.classifier_enabled,
        classifier_model: s.classifier_model,
        classifier_provider: s.classifier_provider,
        allowed_folders: s.allowed_folders,
        short_send_enabled: s.short_send_enabled,
        sliding_cache: s.sliding_cache,
        internet_mode: shadow_internet_mode(&s.internet_mode),
        cwd: std::path::PathBuf::from(s.cwd),
        list_editing: s.list_editing,
        list_sel: s.list_sel,
        picker: s.picker.map(shadow_path_picker),
        providers: s
            .providers
            .into_iter()
            .map(|p| ProviderDraft {
                uuid: p.uuid,
                name: p.name,
                endpoint: p.endpoint,
                api_type: shadow_api_type(&p.api_type),
                api_key: p.api_key,
            })
            .collect(),
        prov_sel: s.prov_sel,
        prov_delete_armed: s.prov_delete_armed,
        prov_modal: s.prov_modal.map(|m| ProviderModal {
            name: m.name,
            endpoint: m.endpoint,
            api_type: shadow_api_type(&m.api_type),
            api_key: m.api_key,
            field: m.field,
        }),
        models: s
            .models
            .into_iter()
            .map(|m| ModelDraft {
                uuid: m.uuid,
                name: m.name,
                model_id: m.model_id,
                provider_idx: m.provider_idx,
                roles: m.roles.iter().map(|r| shadow_role(r)).collect(),
                route: m.route,
                session_only: m.session_only,
            })
            .collect(),
        model_sel: s.model_sel,
        model_delete_armed: s.model_delete_armed,
        model_modal: s.model_modal.map(shadow_model_modal),
    }
}

/// Rebuild the add/edit-model modal ([`ModelModal`]) from its projection. The
/// endpoints are reconstructed from the serde mirror back into [`ModelEndpoint`]
/// (a `Default`-padded copy carrying just the rendered fields).
fn shadow_model_modal(m: ModelModalSnapshot) -> ModelModal {
    ModelModal {
        editing_idx: m.editing_idx,
        uuid: m.uuid,
        name: m.name,
        provider_idx: m.provider_idx,
        model_id: m.model_id,
        field: m.field,
        roles: m.roles.iter().map(|r| shadow_role(r)).collect(),
        role_picker: m.role_picker.map(|rp| RolePickerState {
            checked: rp.checked,
            cursor: rp.cursor,
        }),
        query: m.query,
        result_sel: m.result_sel,
        route: m.route,
        route_sel: m.route_sel,
        endpoints: m.endpoints.map(|eps| {
            eps.into_iter()
                .map(|ep| ModelEndpoint {
                    name: ep.name,
                    provider_name: ep.provider_name,
                    pricing: Some(ModelPricing {
                        prompt: ep.price_prompt,
                        completion: ep.price_completion,
                    }),
                    context_length: None,
                    quantization: None,
                    max_completion_tokens: None,
                    uptime_last_30m: ep.uptime_last_30m,
                    status: None,
                })
                .collect()
        }),
        endpoints_loading: m.endpoints_loading,
        endpoints_for: m.endpoints_for,
    }
}

/// Rebuild the FS directory picker overlay ([`PathPicker`]) from its projection.
///
/// The matches are the daemon's already-computed `read_dir` results, used VERBATIM
/// (the client never walks its own filesystem — its cwd is unrelated to the
/// daemon's session). Constructed as a struct literal rather than via
/// `PathPicker::new`, which would re-run `list_dirs` against the local FS.
fn shadow_path_picker(p: PathPickerSnapshot) -> PathPicker {
    PathPicker {
        query: p.query,
        matches: p.matches,
        sel: p.sel,
        mode: match p.replace_idx {
            None => PickerMode::Add,
            Some(i) => PickerMode::Replace(i),
        },
    }
}

// ─── mode reconstruction (stage 3: secondary full-screen views) ──────────────

/// Rebuild the `--resume` session picker ([`PickerState`]) from its projection.
///
/// Constructed as a struct literal (NOT `PickerState::new`, which would re-run the
/// filter against a freshly-discovered local session list): the daemon's `all`
/// metadata + the `filtered_idx` it computed are carried verbatim so the SAME rows
/// render. Each row's `PathBuf` (the daemon-side load target) is rebuilt empty — the
/// client never loads it (Enter is forwarded), and the picker view doesn't render it.
fn shadow_picker(p: PickerSnapshot) -> PickerState {
    PickerState {
        query: p.query,
        all: p
            .all
            .into_iter()
            .map(|m| SessionMeta {
                id: m.id,
                name: m.name,
                path: std::path::PathBuf::new(), // daemon-side load target; not rendered
                modified: std::time::UNIX_EPOCH + Duration::from_secs(m.modified_secs),
                message_count: m.message_count,
                locked: m.locked,
            })
            .collect(),
        filtered_idx: p.filtered_idx,
        selected: p.selected,
    }
}

/// Rebuild the `/effort` reasoning-effort picker ([`EffortPickerState`]) from its
/// projection (all plain data the overlay reads).
fn shadow_effort(e: crate::ipc::proto::EffortSnapshot) -> EffortPickerState {
    EffortPickerState {
        options: e.options,
        selected: e.selected,
        note: e.note,
    }
}

/// Rebuild the `/usage` dashboard nav state ([`UsageNavState`]) from its wire tokens.
/// The dashboard's DATA is seeded separately into `rest.usage_data` (it crosses on the
/// same `UsageSnapshot`), so this only restores the view/range/metric selections.
fn shadow_usage_nav(view: &str, range: &str, metric: &str) -> UsageNavState {
    UsageNavState {
        view: match view {
            "session" => UsageView::Session,
            _ => UsageView::Global,
        },
        range: match range {
            "week" => UsageRange::Week,
            "year" => UsageRange::Year,
            _ => UsageRange::Today,
        },
        metric: match metric {
            "tokens" => UsageMetric::Tokens,
            _ => UsageMetric::Cost,
        },
    }
}

/// Rebuild the message-rewind picker ([`RewindState`]) from its projection — the
/// newest-first entry list + the cursor.
fn shadow_rewind(rw: RewindSnapshot) -> RewindState {
    RewindState {
        entries: rw
            .entries
            .into_iter()
            .map(|e| RewindEntry {
                vec_index: e.vec_index,
                content: e.content,
            })
            .collect(),
        selected: rw.selected,
    }
}

/// Rebuild the `/agents` dashboard ([`AgentsState`]) from its projection.
///
/// Restores the agent list, the working drafts + sub-mode + field cursor (from wire
/// tokens), the three overlays, and a minimal `session_dir` (empty — the client never
/// saves). The KEYLESS model+provider catalogue is folded into `rest.config` by the
/// caller's `shadow_settings`-style path? No — it is reconstructed HERE into a private
/// `AppConfig` the agents view resolves the model label against, so the client renders
/// `name @ provider` exactly as the daemon would WITHOUT any API key. The reconstructed
/// state is render-only; key handling is forwarded to the daemon.
fn shadow_agents(a: AgentsSnapshot) -> AgentsState {
    AgentsState {
        agents: a.agents,
        list_sel: a.list_sel,
        in_detail: a.in_detail,
        mode: match a.mode.as_str() {
            "edit" => AgentSubMode::Edit,
            "create" => AgentSubMode::Create,
            "delete_confirm" => AgentSubMode::DeleteConfirm,
            _ => AgentSubMode::Browse,
        },
        field: shadow_agent_field(&a.field),
        editing: a.editing,
        create_scope: match a.create_scope.as_str() {
            "global" => AgentScope::Global,
            _ => AgentScope::Session,
        },
        draft_name: a.draft_name,
        draft_description: a.draft_description,
        draft_conditions: a.draft_conditions,
        draft_model_uuid: a.draft_model_uuid,
        draft_model_legacy: a.draft_model_legacy,
        draft_tools: a.draft_tools,
        draft_body: a.draft_body,
        // The session dir is the daemon-side save target; the client never saves, and
        // the view doesn't render it, so an empty path is fine.
        session_dir: std::path::PathBuf::new(),
        tool_picker: a.tool_picker.map(shadow_tool_picker),
        model_picker: a.model_picker.map(shadow_agent_model_picker),
        editor: a
            .editor
            .map(|(field, ed)| (shadow_agent_field(&field), shadow_text_editor(ed))),
        editor_clear_confirm: a.editor_clear_confirm,
    }
}

/// Rebuild the `/agents` tool multi-select picker ([`ToolPickerState`]).
fn shadow_tool_picker(p: ToolPickerSnapshot) -> ToolPickerState {
    ToolPickerState {
        options: p.options,
        checked: p.checked,
        cursor: p.cursor,
        filter: p.filter,
    }
}

/// Rebuild the `/agents` single-select model picker ([`ModelPickerState`]).
fn shadow_agent_model_picker(p: AgentModelPickerSnapshot) -> ModelPickerState {
    ModelPickerState {
        options: p.options,
        cursor: p.cursor,
    }
}

/// Rebuild the full-screen nano editor ([`TextEditorState`]) from its projection. The
/// render-published `wrap_w` cell is re-seeded to `usize::MAX` (its `from_text`
/// default), so before the first client frame every line is one segment — exactly the
/// editor's own safe fallback; the next draw publishes the real width.
fn shadow_text_editor(ed: TextEditorSnapshot) -> TextEditorState {
    TextEditorState {
        lines: ed.lines,
        row: ed.row,
        col: ed.col,
        scroll: ed.scroll,
        wrap_w: std::cell::Cell::new(usize::MAX),
    }
}

/// Map an `/agents` field wire token back to an [`AgentEditField`] (unknown →
/// Description, the editor's default focus — never lost).
fn shadow_agent_field(f: &str) -> AgentEditField {
    match f {
        "name" => AgentEditField::Name,
        "conditions" => AgentEditField::Conditions,
        "model" => AgentEditField::Model,
        "tools" => AgentEditField::Tools,
        "prompt" => AgentEditField::Body,
        _ => AgentEditField::Description,
    }
}

/// Map a theme wire token back to a [`ThemeMode`] (unknown → Dark).
fn shadow_theme(t: &str) -> ThemeMode {
    match t {
        "light" => ThemeMode::Light,
        _ => ThemeMode::Dark,
    }
}

/// Map an internet-mode wire token back to an [`InternetMode`] (unknown → Simple).
fn shadow_internet_mode(t: &str) -> InternetMode {
    match t {
        "full" => InternetMode::Full,
        _ => InternetMode::Simple,
    }
}

/// Map an api-type wire token back to an [`ApiType`] (unknown → OpenAiCompatible).
fn shadow_api_type(t: &str) -> ApiType {
    match t {
        "anthropic" => ApiType::AnthropicCompatible,
        _ => ApiType::OpenAiCompatible,
    }
}

/// Map a role wire token back to a [`ModelRole`] (unknown → Main, never lost).
fn shadow_role(r: &str) -> ModelRole {
    match r {
        "awareness" => ModelRole::Awareness,
        "safeguard" => ModelRole::Safeguard,
        "compactor" => ModelRole::Compactor,
        _ => ModelRole::Main,
    }
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
                // Clear the transcript cache since a new session may become foreground,
                // and the committed history can change wholesale on a foreground switch.
                shadow.rest.transcript_cache.borrow_mut().blocks.clear();
                // Reconcile the work clock to match the daemon's state with the new session
                // in place, so the comet animation stays in sync.
                reconcile_work_clock(shadow);
                return true;
            }
            // (`snap` is `Box<SessionSnapshot>`; `&snap` derefs to `&SessionSnapshot`.)
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
