//! Headless daemon event loop — the `koma --daemon` core.
//!
//! [`daemon_loop`] mirrors the STRUCTURE of [`super::run_loop`] but with the
//! terminal stripped: no `terminal.draw(...)`, no crossterm input poll/read, no
//! `/select` copy mode. Per tick it does exactly the render-agnostic half of the
//! interactive loop — [`super::sessions::service_all_sessions`] (advance every
//! session's turn) + [`super::global::service_global`] (every global drain) —
//! then drives the [`DaemonHub`]: it drains inbound client messages (register /
//! attach / detach / resync / control) and STREAMS render-state to every attached
//! client as seq-tagged [`DaemonFrame`]s. Sharing `service_all_sessions` +
//! `service_global` is what keeps the daemon and the TUI client from ever diverging
//! on runtime behaviour; sharing [`crate::ipc::snapshot::build_snapshot`] is what
//! keeps their RENDER state from diverging.
//!
//! # Sync-loop bridge (critique #1)
//!
//! This loop is SYNCHRONOUS (it `try_recv`s, it `thread::sleep`s) — it is NOT
//! rewritten async. The eventual socket server runs per-client tokio tasks on the
//! existing runtime; those tasks talk to THIS loop over plain `std::sync::mpsc`
//! channels carried by [`DaemonHub`]: client messages arrive on `msg_rx` (drained
//! here each tick, exactly like a session's `active_rx`), and per-client frame
//! senders are enrolled into `clients` (each per-client task holds the matching
//! receiver and writes frames to its socket). The accept loop that produces those
//! tasks lands in daemon stage 5; this stage proves the hub EMITS correct seq'd
//! frames (snapshot on attach, deltas thereafter) — exercised by the unit test at
//! the bottom of this module, which drives the hub with no socket at all.
//!
//! # Frame seq + gap recovery (critique #4)
//!
//! Every emitted [`DaemonFrame`] carries a monotonic `seq` (bumped once per frame).
//! A client detecting a gap replies [`ClientRequest::Resync`]; the daemon answers a
//! fresh full [`crate::ipc::proto::DaemonEvent::Snapshot`] so the shadow rebuilds.
//!
//! # Atomic attach (critique #2)
//!
//! A client's frame channel is enrolled NOT-yet-attached; it becomes delta-eligible
//! ONLY in the same tick its `Attach` is handled, where the snapshot is built AND
//! sent AND the client flipped to attached together. So no delta can be born in the
//! gap between building a client's snapshot and that client going live.
//!
//! # Single-writer (DECISIONS)
//!
//! The FIRST enrolled client is the controller; later ones are read-only observers.
//! A mutating request from an observer is rejected with
//! [`crate::ipc::proto::DaemonEvent::Error`]; read-only requests (Attach / Resync /
//! Detach / ListSessions) are honoured for everyone.
//!
//! # Lifecycle (daemon stage 10)
//!
//! The loop self-exits when EVERY session is CLOSED (tombstoned via
//! [`ClientRequest::QuitSession`] or a forwarded `/quit` kill-all) AND no client is
//! enrolled, sustained for [`SELF_EXIT_GRACE_TICKS`] consecutive ticks (~1s grace, so
//! a momentary lull never reaps it). A session is closed by TOMBSTONE — a `closed`
//! marker on its [`crate::app::state::SessionRuntime`] slot, NEVER a `Vec::remove`:
//! `service_session` indexes the sessions Vec by position ~40x/tick, so a remove would
//! shift every later index and silently cross-wire in-flight async. Right before the
//! exit unlinks the socket it ACCEPT-DRAINs (re-checks "no client" after draining the
//! bridge) so a client connecting during the grace window aborts the exit. SIGTERM/
//! SIGINT and a controller's `QuitDaemon` remain as the explicit stop paths.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app::mode::Mode;
use crate::app::state::AppState;
use crate::controller::command::Command;
use crate::controller::input::{handle_key, handle_paste, Action};
use crate::ipc::proto::{ClientRequest, DaemonEvent, DaemonFrame, StateSnapshot};
use crate::ipc::snapshot::{build_snapshot, diff};
use crate::service::openrouter::OpenRouterClient;

use super::super::actions::{apply_action, attach_select_for_pwd};
use super::super::stream::deny_all_pending;
use super::global::{has_running_subagents, service_global};
use super::sessions::service_all_sessions;

/// One inbound message on the sync-loop bridge, tagged with the client it came
/// from. The per-client connection task (stage 5, [`crate::ipc::conn`]) emits a
/// [`HubInbound::Register`] first (handing the loop its frame sender), then one
/// [`HubInbound::Request`] per framed [`ClientRequest`] it reads off the socket,
/// and finally a [`HubInbound::Disconnect`] on socket EOF/error.
// `pub(crate)` (not `pub(in crate::app::runtime)`) so the per-client connection
// task in `crate::ipc::conn` — which lives OUTSIDE this module tree — can build and
// send these. Re-exported as `crate::app::runtime::HubInbound`.
pub(crate) enum HubInbound {
    /// A new connection: enrol this client's frame channel (NOT yet attached — it
    /// goes live only when its `Attach` is handled, critique #2).
    Register {
        client_id: u64,
        frame_tx: Sender<DaemonFrame>,
    },
    /// A framed request from an already-registered client.
    Request {
        client_id: u64,
        req: ClientRequest,
    },
    /// The per-client task observed socket EOF/error and is exiting; deregister
    /// this client. Distinct from a protocol-level [`ClientRequest::Detach`]: that
    /// is the client politely leaving while its socket stays up momentarily, this
    /// is the transport going away. Both deregister + pass the controller seat.
    Disconnect { client_id: u64 },
}

/// One enrolled client in the hub registry.
struct HubClient {
    /// Loop-assigned connection id (matches the per-client task's `client_id`).
    id: u64,
    /// Frame sender to this client's per-client task (which writes to its socket).
    /// A send error means the task/socket is gone; the client is dropped on the
    /// next sweep.
    frame_tx: Sender<DaemonFrame>,
    /// First-enrolled client is the single writer; the rest are observers.
    is_controller: bool,
    /// True only AFTER this client's `Attach` was handled (snapshot sent). Deltas
    /// are fanned out ONLY to attached clients, so an enrolled-but-not-yet-attached
    /// client can never receive a delta before its snapshot (critique #2).
    attached: bool,
    /// PER-CLIENT monotonic frame seq (blocker #1): the seq of the last frame this
    /// client was sent; its next frame is `last_seq + 1`. Owned per connection — the
    /// `DaemonFrame.seq` contract is "monotonic PER CONNECTION", so each client's
    /// stream counts up independently. A single hub-global counter would split a
    /// fan-out across clients (client A seq N, client B seq N+1) and every later
    /// frame would read as a gap to both, an infinite Resync storm.
    last_seq: u64,
    /// PER-CLIENT diff baseline (blocker #2): the most-recent snapshot THIS client
    /// has been sent. `None` until this client attaches; reseeded on attach/resync
    /// and advanced every tick its own deltas are computed. Per-client (not one hub-
    /// global baseline) so a second client attaching — which reseeds only ITS own
    /// baseline — can never swallow deltas an already-attached client still owes:
    /// each client's deltas are diffed against exactly what THAT client last saw.
    last_snapshot: Option<StateSnapshot>,
}

/// The sync-loop <-> per-client-task bridge + the render-state streaming engine
/// (critique #1/#2/#4 + single-writer).
///
/// Owns the daemon side of the bridge channel plus the client registry. The
/// monotonic frame `seq` AND the diff baseline are held PER CLIENT (on
/// [`HubClient`], blockers #1/#2) — NOT hub-global — so a fan-out and a late
/// attach can never cross-wire one client's stream into another's. Built empty by
/// [`new`](Self::new); the runner holds the paired [`Sender<HubInbound>`] for the
/// daemon's lifetime so `msg_rx` never goes `Disconnected` before any client
/// connects.
pub(in crate::app::runtime) struct DaemonHub {
    /// Inbound client messages, drained per tick (like `active_rx`).
    msg_rx: Receiver<HubInbound>,
    /// Enrolled clients the loop fans [`DaemonFrame`]s out to. Each owns its own
    /// monotonic seq + diff baseline.
    clients: Vec<HubClient>,
    /// Set by a controller's [`ClientRequest::QuitDaemon`]; the [`daemon_loop`]
    /// observes it via [`should_shutdown`](Self::should_shutdown) and returns, so
    /// the shared teardown (release locks, drop runtime, unlink socket) runs.
    shutdown: bool,
}

impl DaemonHub {
    /// Build an empty hub plus the paired message-sender the accept loop clones
    /// into each per-client task. The caller (the daemon runner) holds the returned
    /// [`Sender`] for the daemon's lifetime so `msg_rx` never observes a premature
    /// `Disconnected` before any client has connected.
    pub(in crate::app::runtime) fn new() -> (Self, Sender<HubInbound>) {
        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        (
            Self {
                msg_rx,
                clients: Vec::new(),
                shutdown: false,
            },
            msg_tx,
        )
    }

    /// Whether a controller asked the daemon to quit ([`ClientRequest::QuitDaemon`]).
    /// The [`daemon_loop`] checks this each tick and breaks so the shared teardown
    /// runs.
    pub(in crate::app::runtime) fn should_shutdown(&self) -> bool {
        self.shutdown
    }

    /// Number of clients currently ENROLLED (registered, attached or not). The
    /// self-exit grace timer (daemon stage 10) treats ANY enrolled client — even one
    /// mid-`Attach` handshake — as "a client is present", so a daemon never reaps
    /// itself out from under a just-connected client. A registered-but-not-yet-
    /// attached client is the exact accept-drain race the exit re-check guards.
    pub(in crate::app::runtime) fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Drain any inbound bridge messages queued RIGHT NOW (e.g. a `Register` from a
    /// client that connected during the self-exit grace window) WITHOUT streaming —
    /// used by the exit re-check (accept-drain, critique #3) so a connection that
    /// landed between the last tick and the unlink is observed before the daemon
    /// commits to exiting. After this returns, [`client_count`](Self::client_count)
    /// reflects any such late client and the exit is aborted.
    pub(in crate::app::runtime) fn drain_inbound_only(
        &mut self,
        state: &mut AppState,
        client: &mut Option<Arc<OpenRouterClient>>,
        handle: &tokio::runtime::Handle,
    ) {
        self.drain_inbound(state, client, handle);
    }

    /// Send one event to a single client as a fresh seq-tagged frame, advancing
    /// THAT client's own monotonic seq (blocker #1: seq is per-connection, so the
    /// next frame seq is the client's `last_seq + 1`). A dead socket (`SendError`)
    /// is ignored here — the seq is NOT advanced on a failed send, so the client's
    /// stream stays gap-free for the frames it actually received; the client is
    /// reaped by [`sweep_dead`](Self::sweep_dead) afterwards.
    fn send_to(&mut self, idx: usize, event: DaemonEvent) {
        // Index validity is the caller's contract (it iterates known indices).
        let seq = self.clients[idx].last_seq + 1;
        let frame = DaemonFrame { seq, event };
        if self.clients[idx].frame_tx.send(frame).is_ok() {
            self.clients[idx].last_seq = seq;
        }
    }

    /// Deregister the client at `idx` and pass the controller seat if it held it.
    ///
    /// Shared by `Detach` (polite leave) and `Disconnect` (socket EOF). Single-
    /// writer (DECISIONS): if the removed client was the controller, the FIRST
    /// remaining client is promoted so a daemon never ends up writer-less while a
    /// client is still attached (which would silently reject every mutation). No
    /// snapshot is re-sent on promotion — the promoted client already holds a live
    /// shadow; it simply gains mutate rights.
    fn deregister(&mut self, idx: usize) {
        let was_controller = self.clients[idx].is_controller;
        self.clients.remove(idx);
        if was_controller && !self.clients.iter().any(|c| c.is_controller) {
            if let Some(first) = self.clients.first_mut() {
                first.is_controller = true;
            }
        }
    }

    /// Handle every inbound bridge message queued this tick, building+sending a
    /// snapshot for each attaching/resyncing client IN THE SAME TICK (critique #2).
    /// Mutating requests are applied against `state`/`client` via the SAME action
    /// handlers the local TUI uses. Returns nothing; frames are pushed onto the
    /// relevant clients' channels.
    fn drain_inbound(
        &mut self,
        state: &mut AppState,
        client: &mut Option<Arc<OpenRouterClient>>,
        handle: &tokio::runtime::Handle,
    ) {
        loop {
            match self.msg_rx.try_recv() {
                Ok(msg) => self.handle_inbound(msg, state, client, handle),
                Err(TryRecvError::Empty) => break,
                // No client has ever connected (the runner still holds the paired
                // sender) or every task dropped its sender — nothing to drain.
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    /// Apply one bridge message against the registry / emit its reply.
    fn handle_inbound(
        &mut self,
        msg: HubInbound,
        state: &mut AppState,
        client: &mut Option<Arc<OpenRouterClient>>,
        handle: &tokio::runtime::Handle,
    ) {
        match msg {
            HubInbound::Register {
                client_id,
                frame_tx,
            } => {
                // First enrolled client is the single writer (DECISIONS).
                let is_controller = self.clients.is_empty();
                self.clients.push(HubClient {
                    id: client_id,
                    frame_tx,
                    is_controller,
                    attached: false,
                    last_seq: 0,
                    // Not delta-eligible until its Attach seeds this baseline.
                    last_snapshot: None,
                });
            }
            HubInbound::Request { client_id, req } => {
                self.handle_request(client_id, req, state, client, handle);
            }
            HubInbound::Disconnect { client_id } => {
                // Transport gone: deregister + pass the controller seat. Unknown id
                // (already removed via Detach) is a harmless no-op.
                if let Some(idx) = self.clients.iter().position(|c| c.id == client_id) {
                    self.deregister(idx);
                }
            }
        }
    }

    /// Route one [`ClientRequest`] from `client_id`. Read-only requests
    /// (Attach / Resync / ListSessions / Detach) are honoured for any client;
    /// mutating requests are rejected for observers (single-writer).
    fn handle_request(
        &mut self,
        client_id: u64,
        req: ClientRequest,
        state: &mut AppState,
        client: &mut Option<Arc<OpenRouterClient>>,
        handle: &tokio::runtime::Handle,
    ) {
        let Some(idx) = self.clients.iter().position(|c| c.id == client_id) else {
            // A request from a client we never registered — ignore (no panic). A
            // well-behaved task always Registers before any Request.
            return;
        };

        match req {
            // --- read-only / control (honoured for everyone) ---
            ClientRequest::Attach { cwd, .. } => {
                // pwd-AWARE attach selection (stage 3): BEFORE snapshotting, point the
                // foreground at a session for the ATTACHING CLIENT's working directory,
                // so launching `koma` from a NEW dir lands on a session for THAT dir —
                // not the daemon's unrelated last session. Runs ONLY for the controller
                // (the single writer): session selection mutates state (it can SWAP /
                // LOAD / CREATE a session), which an observer must not do. An observer,
                // or a controller that sent no `cwd`, just gets the current foreground.
                // Last-attach-wins on foreground (single-client assumption — DECISIONS).
                // Any handler error is swallowed (surfaced via status, never aborts the
                // loop) so a bad selection can't wedge the attach handshake; the snapshot
                // below still goes out, reflecting whatever foreground resulted.
                if self.clients[idx].is_controller {
                    if let Some(cwd) = cwd {
                        let cwd = std::path::PathBuf::from(cwd);
                        if let Err(e) = attach_select_for_pwd(state, client, handle, &cwd) {
                            state.rest.status = format!("attach select error: {e:#}");
                        }
                    }
                }
                // ATOMIC attach (critique #2): build the full snapshot, send it, and
                // flip the client to attached + seed ITS OWN baseline IN THIS TICK.
                // Only this client's baseline is (re)seeded (blocker #2) — never a
                // hub-global one — so a late attach can't swallow deltas another
                // already-attached client still owes; that client diffs against its
                // own untouched baseline. Built AFTER pwd selection so this client's very
                // first snapshot already reflects the resolved (possibly new) foreground.
                let snap = build_snapshot(state);
                self.send_to(idx, DaemonEvent::Snapshot(Box::new(snap.clone())));
                self.clients[idx].attached = true;
                self.clients[idx].last_snapshot = Some(snap);
            }
            ClientRequest::Resync | ClientRequest::ListSessions => {
                // Both answer with a fresh full snapshot (the simplest correct reply
                // for ListSessions too — it carries the full session set). Re-seed
                // ONLY this client's baseline so its subsequent deltas fold onto what
                // it was just sent; other clients' baselines are untouched (blocker
                // #2), so one client's resync never disturbs another's delta stream.
                let snap = build_snapshot(state);
                self.send_to(idx, DaemonEvent::Snapshot(Box::new(snap.clone())));
                self.clients[idx].attached = true;
                self.clients[idx].last_snapshot = Some(snap);
            }
            ClientRequest::Detach => {
                // Polite leave: drop the client + pass the controller seat to the
                // next attached client (single-writer controller-passing, DECISIONS).
                self.deregister(idx);
            }

            // --- mutating (single-writer: observers are rejected) ---
            req => {
                if !self.clients[idx].is_controller {
                    self.send_to(
                        idx,
                        DaemonEvent::Error(
                            "read-only: another client controls this daemon".into(),
                        ),
                    );
                    return;
                }
                self.handle_controller_mutation(idx, req, state, client, handle);
            }
        }
    }

    /// Handle a MUTATING request from the controller by translating it to the SAME
    /// [`Action`] / slash-command the local TUI uses and funnelling it through
    /// [`apply_action`] — so the daemon never forks the submit / key / approval /
    /// new-session logic. UUID-keyed control resolves the id to an index FIRST and
    /// rejects an unknown id with an `Error` + no-op (critique #5: never a panic,
    /// never a wrong-index switch). Each applied request gets an `Ack`; errors get an
    /// `Error`. `apply_action`'s `Result` is surfaced as an `Error` frame rather than
    /// propagated, so one bad request can never abort the daemon loop.
    fn handle_controller_mutation(
        &mut self,
        idx: usize,
        req: ClientRequest,
        state: &mut AppState,
        client: &mut Option<Arc<OpenRouterClient>>,
        handle: &tokio::runtime::Handle,
    ) {
        match req {
            // UUID-keyed foreground switch: resolve the id to an index, reject an
            // unknown id (critique #5), else reuse the local foreground-switch path (LiveSwitch)
            // and clear that session's sticky finished-unseen marker (critique #3 —
            // foregrounding a session counts as "seen").
            ClientRequest::SwitchForeground { session_id } => {
                match state.rest.sessions.iter().position(|s| s.id == session_id) {
                    Some(target) => {
                        let result = apply_action(Action::LiveSwitch(target), state, client, handle);
                        // LiveSwitch sets `foreground = target`; clear the marker on
                        // the now-foreground session (index unchanged by the switch).
                        if let Some(s) = state.rest.sessions.get_mut(target) {
                            s.finished_unseen = false;
                        }
                        self.ack_or_error(idx, result);
                    }
                    None => self.send_to(
                        idx,
                        DaemonEvent::Error(format!("unknown session id: {session_id}")),
                    ),
                }
            }

            // Submit composed text to the foreground session — identical to the local
            // Enter-on-composer path (`Action::Submit` carries the text directly).
            ClientRequest::SubmitInput { text } => {
                let result = apply_action(Action::Submit(text), state, client, handle);
                self.ack_or_error(idx, result);
            }

            // Run a `!` shell command in the foreground session's cwd, no model
            // round-trip — the same `Action::Shell` the local composer's leading-`!`
            // detection emits, so the shell-entry-append logic is never forked.
            ClientRequest::Shell { cmd } => {
                let result = apply_action(Action::Shell(cmd), state, client, handle);
                self.ack_or_error(idx, result);
            }

            // Forward a key to the foreground session through the EXACT local input
            // pipeline: KeyWire -> crossterm KeyEvent -> controller::handle_key ->
            // Action -> apply_action. So the daemon reuses the same per-mode key
            // handling (chat / pickers / forms) as the local TUI.
            ClientRequest::SendKey(key) => {
                let action = handle_key(state, key.to_key_event());
                let result = apply_action(action, state, client, handle);
                self.ack_or_error(idx, result);
            }

            // Forward a bracketed PASTE through the EXACT local paste pipeline:
            // `controller::input::handle_paste` routes the text to the active field of
            // the current mode (deepest-modal priority), and — in Chat — runs the
            // image-path detection: a pasted image-file PATH is ingested DAEMON-SIDE
            // into the foreground session's `images/` dir as an `[Image #N]`
            // attachment (the daemon owns the session + its images dir), while
            // ordinary text lands in the composer with CRLF normalisation. The
            // resulting `input` marker, `pending_attachments`, and any toast are
            // projected to the client by the normal snapshot/delta. `handle_paste`
            // mutates `state` directly and is infallible, so this always Acks (mirrors
            // the local loop, which just calls it then redraws — no `apply_action`).
            ClientRequest::Paste { text } => {
                handle_paste(state, &text);
                self.send_to(idx, DaemonEvent::Ack);
            }

            // Answer the foreground session's pending tool-approval prompt via the
            // local approve/deny handlers.
            ClientRequest::ApproveTool { approve } => {
                let action = if approve {
                    Action::ApproveTool
                } else {
                    Action::DenyTool
                };
                let result = apply_action(action, state, client, handle);
                self.ack_or_error(idx, result);
            }

            // Spawn a fresh parallel session via the local `/new` command. The
            // requested `name` / `working_dir` are not yet honoured (the `/new` path
            // inherits last-used creds + the launch dir); wiring them is a later
            // refinement, so they are accepted-and-ignored rather than rejected.
            ClientRequest::NewSession { .. } => {
                let result = apply_action(Action::Slash(Command::New), state, client, handle);
                self.ack_or_error(idx, result);
            }

            // Quit (close) a single session by stable UUID (daemon stage 10). Resolve
            // the id (reject an unknown one with an Error + no-op, critique #5), then
            // TOMBSTONE that session: `close()` aborts its in-flight stream + sub-
            // agents, drops its receivers, and releases its on-disk lock — but the slot
            // STAYS in `sessions` so no index shifts (a `Vec::remove` would cross-wire
            // the other sessions' index-routed async). If the closed session was the
            // foreground, repoint foreground onto a still-live session so render/service
            // never touch a tombstone. The daemon self-exits later (grace-timed) once
            // EVERY session is closed AND no client is attached.
            ClientRequest::QuitSession { session_id } => {
                match state.rest.sessions.iter().position(|s| s.id == session_id) {
                    Some(target) => {
                        state.rest.sessions[target].close();
                        repoint_foreground_off_closed(state);
                        self.send_to(idx, DaemonEvent::Ack);
                    }
                    None => self.send_to(
                        idx,
                        DaemonEvent::Error(format!("unknown session id: {session_id}")),
                    ),
                }
            }

            // Ask the daemon to shut down: latch the flag the loop polls, then Ack.
            // The actual teardown (release locks, drop runtime, unlink socket) runs
            // once `daemon_loop` observes `should_shutdown()` and returns.
            ClientRequest::QuitDaemon => {
                self.shutdown = true;
                self.send_to(idx, DaemonEvent::Ack);
            }

            // Read-only / already-handled variants never reach here (handle_request
            // dispatches them); treat any residual as a no-op Ack so the match is
            // exhaustive without a spurious error.
            ClientRequest::Attach { .. }
            | ClientRequest::Detach
            | ClientRequest::Resync
            | ClientRequest::ListSessions => {
                self.send_to(idx, DaemonEvent::Ack);
            }
        }
    }

    /// Reply `Ack` on success or `Error(msg)` on a handler error — so a failing
    /// action surfaces to the client instead of aborting the daemon loop.
    fn ack_or_error(&mut self, idx: usize, result: anyhow::Result<()>) {
        match result {
            Ok(()) => self.send_to(idx, DaemonEvent::Ack),
            Err(e) => self.send_to(idx, DaemonEvent::Error(format!("{e:#}"))),
        }
    }

    /// Stream this tick's render-state changes to every ATTACHED client.
    ///
    /// Builds ONE fresh snapshot from live `state`, then for EACH attached client
    /// diffs it against THAT client's own `last_snapshot` baseline (blocker #2) and
    /// EITHER sends that client a full `Snapshot` (structural change) OR one `Delta`
    /// frame per change, advancing only that client's baseline. Per-client diffing
    /// is what makes a late attach / resync safe: clients that attached at different
    /// moments hold different baselines, so each receives exactly the updates IT is
    /// missing — never a shared baseline that one client's reseed could shortcut.
    /// Each emitted frame bumps the receiving client's own seq (blocker #1). No-op
    /// for a client whose baseline already equals `next`.
    fn stream_deltas(&mut self, state: &AppState) {
        // Nothing to do until at least one client has attached. Enrolled-but-not-
        // attached clients have no baseline and receive nothing (critique #2).
        if !self.clients.iter().any(|c| c.attached) {
            return;
        }

        // Build the live projection ONCE; every attached client diffs against it
        // from its own baseline below.
        let next = build_snapshot(state);

        for i in 0..self.clients.len() {
            if !self.clients[i].attached {
                continue;
            }

            // Diff this client's OWN baseline -> next. Scoped so the immutable
            // borrow of `last_snapshot` ends before the `&mut self` sends below.
            // An attached client always has a baseline (seeded at attach/resync).
            let result = {
                let prev = self.clients[i]
                    .last_snapshot
                    .as_ref()
                    .expect("attached client always has a baseline");
                diff(prev, &next)
            };

            if result.needs_full {
                // Structural change: resend this client a full Snapshot + advance
                // its baseline. `next` is shared across the loop, so clone per send.
                self.send_to(i, DaemonEvent::Snapshot(Box::new(next.clone())));
                self.clients[i].last_snapshot = Some(next.clone());
            } else if !result.deltas.is_empty() {
                for d in result.deltas {
                    self.send_to(i, DaemonEvent::Delta(d));
                }
                self.clients[i].last_snapshot = Some(next.clone());
            }
            // else: this client's shadow already matches — keep its baseline, emit
            // nothing to it.
        }
    }
}

/// Number of consecutive QUALIFYING ticks (all sessions quiesced AND no client)
/// required before the daemon self-exits (daemon stage 10). At the idle 100ms
/// cadence this is ~1s of sustained quiet — a grace window so a momentary lull
/// (a session closed but a client about to attach, or a `/new` mid-flight) does
/// NOT reap the daemon. The counter resets to 0 the instant a session is live-
/// working again or a client is enrolled.
const SELF_EXIT_GRACE_TICKS: u32 = 10;

/// How long a session may stay PARKED on tool-approval while DETACHED (no client
/// attached) before the daemon AUTO-DENIES the pending risky call(s) (daemon stage
/// 11, item 1). Rationale (critique): an immortal parked daemon holding a session
/// lock with no operator on the wire is strictly worse than a denied tool — the deny
/// keeps the conversation API-valid and lets the session go idle so the daemon can
/// eventually self-exit and release its lock. The window is generous (30 min) so an
/// operator who merely stepped away has ample time to reattach and answer; while a
/// client IS attached the timer never runs (it is cleared on attach), so an attached
/// operator can leave an approval pending indefinitely — that is the intended
/// pause-till-reattach. Measured from `SessionRuntime::park_started_at`, stamped the
/// first detached+awaiting tick.
const APPROVAL_PARK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// True when every session is CLOSED (tombstoned) — the "nothing left to run" half
/// of the self-exit condition (daemon stage 10). The documented contract
/// ([`crate::ipc::proto::ClientRequest::Detach`]) is that the daemon self-exits only
/// when ZERO sessions AND no client remain; in the tombstone model a closed session
/// IS the daemon's notion of a removed session (the slot lingers only so positions
/// never shift), so "zero sessions" == "every session closed".
///
/// It deliberately does NOT use `sessions.is_empty()`: the Vec still HOLDS tombstoned
/// slots, so an empty check would never fire. It also does NOT self-exit on a merely
/// IDLE-but-live session — a user who detached a still-open session expects it to
/// persist for the next attach; only an explicit close (per-session quit or kill-all)
/// tombstones it. An empty `sessions` (defensive — there is always >=1) counts as
/// all-closed.
fn all_sessions_closed(state: &AppState) -> bool {
    state.rest.sessions.iter().all(|s| s.closed)
}

/// Repoint `foreground` off a CLOSED (tombstoned) session onto a still-live one
/// (daemon stage 10, item 5). If the current foreground is not closed, this is a
/// no-op. Otherwise it picks the FIRST non-closed session as the new foreground so
/// render / `service_session` never touch a tombstone. If NO session is live (every
/// one is closed) it leaves `foreground` as-is: the daemon is about to self-exit
/// anyway, and `service_session` skips the closed foreground regardless, so a
/// tombstone foreground is harmless in that terminal window. Never goes out of
/// range (only ever set to a valid EXISTING index — we never reorder/remove the
/// Vec, so this can't cross-wire index-routed async).
fn repoint_foreground_off_closed(state: &mut AppState) {
    let fg = state.rest.foreground;
    // Current foreground still live → nothing to do.
    if !state.rest.sessions.get(fg).map(|s| s.closed).unwrap_or(false) {
        return;
    }
    if let Some(live) = state.rest.sessions.iter().position(|s| !s.closed) {
        state.rest.foreground = live;
    }
    // else: every session closed → leave fg; the daemon self-exits and
    // service_session skips the closed foreground meanwhile.
}

/// Close (tombstone) EVERY session — the daemon-side "kill all" used by the `/quit`
/// `[k]` path (daemon stage 10, item 4). Each session's `close()` aborts its stream
/// and sub-agents, drops receivers, and releases its lock; the slots stay in place so
/// no index shifts. Foreground is repointed afterwards — it lands on a tombstone since
/// all are closed, which is harmless: the grace-timed self-exit then fires because
/// `all_sessions_closed` is now true and no further live work can start.
fn close_all_sessions(state: &mut AppState) {
    for s in &mut state.rest.sessions {
        s.close();
    }
    repoint_foreground_off_closed(state);
}

/// Drive the DETACHED-approval park timer for every live session and AUTO-DENY any
/// that has been parked-on-approval too long with no operator on the wire (daemon
/// stage 11, item 1). Called once per tick AFTER `drain_inbound` (so a same-tick
/// attach / approve / deny is already reflected) and BEFORE `stream_deltas` (so an
/// auto-deny folds into this tick's frames).
///
/// `client_attached` is `hub.client_count() > 0`. Per live (non-closed) session:
/// - PARKED + DETACHED (`awaiting_approval && !client_attached`): stamp
///   `park_started_at` on the first such tick; once `Instant::elapsed()` crosses
///   [`APPROVAL_PARK_TIMEOUT`], answer the pending risky call(s) as DENIED via the
///   shared [`deny_all_pending`] path — which keeps the conversation API-valid (no
///   dangling `tool_call` ids), resets the agentic machine, clears `awaiting_approval`
///   (so the session goes idle), and tears down any sub-agents — then clear the timer.
/// - NOT parked, OR a client is attached: clear `park_started_at`. An attached client
///   waits for its operator indefinitely (no timeout), so the timer must not run while
///   attached; clearing also restarts the grace from zero on the next detach, so an
///   operator who reattaches then leaves again gets a full fresh window.
///
/// Returns `true` if any session was auto-denied (so the caller flags the loop dirty,
/// purely for symmetry with the other servicers — headless, nothing is drawn).
fn service_approval_park_timeouts(state: &mut AppState, client_attached: bool) -> bool {
    let mut denied_any = false;
    let now = Instant::now();
    // Index-based throughout (no long-lived `&mut` session borrow) so the auto-deny
    // branch can re-borrow `state` for `deny_all_pending` with no borrow-checker
    // gymnastics. Each arm touches only `sessions[idx]` (or hands `idx` to the deny).
    for idx in 0..state.rest.sessions.len() {
        if state.rest.sessions[idx].closed {
            continue;
        }
        let parked_detached =
            state.rest.sessions[idx].awaiting_approval && !client_attached;
        if !parked_detached {
            // Not parked, or a client is attached → no timeout; reset the clock.
            state.rest.sessions[idx].park_started_at = None;
            continue;
        }
        // Detached + parked: start (or keep) the timer, then check expiry. `Instant`
        // is `Copy`, so this reads out the (possibly just-stamped) start instant.
        let started = *state.rest.sessions[idx].park_started_at.get_or_insert(now);
        if now.duration_since(started) >= APPROVAL_PARK_TIMEOUT {
            // Auto-deny via the shared deny path (keeps every tool_call id answered, so
            // the conversation stays API-valid). `deny_all_pending` clears
            // `awaiting_approval`; clear the timer too so a fresh park (should the model
            // somehow re-enter approval) re-stamps from now.
            deny_all_pending(
                state,
                idx,
                "auto-denied: approval request timed out while detached",
            );
            state.rest.sessions[idx].park_started_at = None;
            denied_any = true;
        }
    }
    denied_any
}

/// True when the daemon has NO self-advancing work to do this tick — every live
/// session is either idle or PARKED on tool-approval, no global async (catalogue
/// fetch / loading splash / running sub-agent) is in flight, AND (when any session
/// is parked) no client is attached (daemon stage 11, item 2). In that state the
/// only thing that could change is an operator's approve — which, while DETACHED,
/// can't arrive — so the loop should nap on the SLOW idle cadence instead of spinning
/// the busy 8ms tick.
///
/// "Self-advancing work" is anything that progresses on its own via an async channel:
/// a live stream, a parked deferred tool-task / sub-agent lane, a running sub-agent,
/// or a pending catalogue/loading transition. `awaiting_approval` is the ONE working
/// state that does NOT self-advance (it needs an external answer), so it does NOT
/// count as busy here. While a client IS attached, a parked session keeps the FAST
/// cadence (caller handles that) so a reattached operator's approve is processed
/// with minimal latency.
fn all_idle_or_parked_detached(state: &AppState, client_attached: bool) -> bool {
    // Any global async work pending → not quiescent.
    if state.rest.catalogue_pending.is_some()
        || matches!(state.mode, Mode::Loading(_))
        || has_running_subagents(state)
    {
        return false;
    }
    // Any live session doing self-advancing work (anything working that ISN'T merely
    // awaiting approval) → not quiescent.
    let any_progressing = state
        .rest
        .sessions
        .iter()
        .any(|s| !s.closed && s.is_working() && !s.awaiting_approval);
    if any_progressing {
        return false;
    }
    // Here: nothing is self-advancing. If a client is attached AND a session is parked
    // on approval, keep fast (responsive approve); otherwise (detached, or fully idle)
    // we can nap slow.
    if client_attached
        && state
            .rest
            .sessions
            .iter()
            .any(|s| !s.closed && s.awaiting_approval)
    {
        return false;
    }
    true
}

/// The headless daemon loop. Each tick services every session + every global
/// concern, drives the hub (drain inbound requests + apply mutations + stream
/// deltas), then sleeps on the adaptive cadence. No terminal, no input, no draw.
///
/// Returns on ANY shutdown trigger so the caller's teardown (release every session
/// lock, drop the runtime, unlink socket + pidfile) runs:
/// 1. a controller sends [`ClientRequest::QuitDaemon`] (the hub latches its own
///    flag, observed via [`DaemonHub::should_shutdown`]), or
/// 2. the process receives SIGTERM/SIGINT — the signal task (installed in
///    [`super::super::run_daemon`]) flips `shutting_down`, which this loop polls
///    each tick. The loop stays SYNCHRONOUS: the async signal task only sets the
///    atomic; no awaiting happens in the loop body, or
/// 3. SELF-EXIT (daemon stage 10): every session is CLOSED (tombstoned) AND no
///    client is enrolled, sustained for [`SELF_EXIT_GRACE_TICKS`] CONSECUTIVE ticks
///    (a ~1s grace window so a momentary lull never reaps the daemon). The grace
///    counter resets the instant any session is live OR a client is enrolled. Right
///    before committing to the self-exit the loop does an ACCEPT-DRAIN re-check
///    (critique #3): it drains the bridge once more and re-tests "no client", so a
///    client that connected DURING the grace window aborts the exit rather than being
///    left with a half-open socket.
///
/// `/quit` kill-all (item 4): the CLIENT (`--attach`) and the LOCAL TUI reach this
/// the SAME end (every session tombstoned, daemon torn down) via DIFFERENT paths:
///   - CLIENT: `handle_quit_confirm_key`'s `[k]` does NOT forward a `SendKey`; it
///     sends [`ClientRequest::QuitDaemon`] directly. The hub latches its shutdown
///     flag (observed via [`DaemonHub::should_shutdown`], trigger 1 above), so the
///     daemon tears down through the shared graceful path — no `should_quit` round
///     trip is involved on the client `[k]` path.
///   - LOCAL TUI: there is no IPC; the forwarded-key story does not apply. The kill-
///     all key runs through `handle_key` -> `QuitKillAll` -> `handle_quit_kill_all`,
///     which sets `state.rest.should_quit`. This loop observes that flag, CLOSES every
///     session (tombstone), and clears it — which makes [`all_sessions_closed`] true so
///     the grace-timed self-exit (3) fires and tears down cleanly. It does NOT break
///     immediately: letting self-exit drive the exit keeps the teardown path single and
///     flushes a final closed-state snapshot to any attached client.
///
/// `shutting_down` is the process-level (signal-driven) stop flag; it is ORed with
/// the hub's client-driven `QuitDaemon` flag so either path tears down identically.
/// The daemon-selftest passes a never-set flag (signals don't apply there).
///
/// `client` is `&mut` both to match `service_*`'s signature (a debounced catalogue
/// fetch can replace the keyless client) AND so a controller's mutating request can
/// rebuild it at a session boundary (e.g. `/new`, a foreground switch).
pub(in crate::app::runtime) fn daemon_loop(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
    hub: &mut DaemonHub,
    shutting_down: &Arc<AtomicBool>,
) {
    // Consecutive qualifying ticks toward self-exit (all closed AND no client). Reset
    // to 0 whenever a session is live or a client is enrolled (daemon stage 10).
    let mut quiesce_ticks: u32 = 0;

    loop {
        // 1. Service EVERY session: drain each session's stream / tool-task /
        //    sub-agent channels and advance its turn. Identical to the TUI loop —
        //    the `dirty` return is irrelevant headless (nothing is drawn). Closed
        //    sessions are skipped inside `service_session`.
        let _ = service_all_sessions(state, client, handle);

        // 2. Service every GLOBAL concern (endpoint/warm/clipboard drains, the
        //    loading-splash state machine, deferred compaction apply, missing-root
        //    warning, comet-shimmer reconcile, toast tick). Same shared call the
        //    TUI loop uses, so the daemon never diverges on global handling.
        let _ = service_global(state, client, handle);

        // 3. Drive the hub: handle inbound client messages (register / attach /
        //    detach / resync / control) — atomically snapshotting each attaching
        //    client in THIS tick AND applying a controller's mutating requests
        //    against state/client via the shared action handlers (including
        //    `QuitSession`, which tombstones one session). Stream AFTER the kill-all
        //    handling below so a closed-state snapshot reflects the tombstones.
        hub.drain_inbound(state, client, handle);

        // 3a. Kill-all (item 4): a forwarded QuitConfirm `[k]` set `should_quit` via
        //     `handle_quit_kill_all`. In the DAEMON that means "close every session"
        //     (NOT an abrupt loop break — that is the LOCAL TUI's behaviour, where the
        //     run_loop breaks on `should_quit`). Tombstone them all and clear the flag;
        //     `all_sessions_closed` is now true, so the grace-timed self-exit below
        //     drives a single clean teardown. Foreground is repointed inside
        //     `close_all_sessions`. (Detach `[d]` leaves sessions live and is a CLIENT-
        //     side exit — it never reaches here as a daemon close.)
        if state.rest.should_quit {
            close_all_sessions(state);
            state.rest.should_quit = false;
        }

        // 3a-bis. DETACHED-approval park timeout (stage 11, item 1). With the inbound
        //     batch (incl. any attach / approve / deny) already applied above, drive
        //     each live session's park timer: a session parked on tool-approval while
        //     NO client is attached is auto-denied once it crosses
        //     `APPROVAL_PARK_TIMEOUT`, via the shared `deny_all_pending` path (so the
        //     conversation stays API-valid and the session goes idle — freeing the
        //     daemon to eventually self-exit and release its lock). A client being
        //     attached clears the timer (an attached operator waits indefinitely).
        //     Run BEFORE `stream_deltas` so an auto-deny's idle state folds into this
        //     tick's frames.
        let _ = service_approval_park_timeouts(state, hub.client_count() > 0);

        // 3b. Stream this tick's render-state changes to every attached client as
        //     seq'd frames (after kill-all so a tombstoned set folds back).
        hub.stream_deltas(state);

        // 3c. Honour the EXPLICIT shutdown triggers so the caller's teardown runs:
        //       - the hub's client-driven QuitDaemon flag, or
        //       - the process-level signal flag (SIGTERM/SIGINT) the signal task set.
        //     Checked AFTER streaming so a pending QuitDaemon Ack is flushed first.
        //     `Relaxed` is sufficient: this is a single boolean flag with no other
        //     memory it must publish/acquire — teardown reads only owned `state`.
        if hub.should_shutdown() || shutting_down.load(Ordering::Relaxed) {
            break;
        }

        // 3d. SELF-EXIT grace timer (daemon stage 10, items 2+3). Qualify this tick
        //     only when EVERY session is closed AND no client is enrolled. Any live
        //     session or any enrolled client resets the counter (so a lull mid-`/new`,
        //     or a still-attached client, never trips it). Once the counter reaches
        //     the grace threshold, ACCEPT-DRAIN re-check: drain the bridge one more
        //     time and re-test "no client" — a client that connected during the grace
        //     window (its `Register` now sitting on the bridge) aborts the exit, so we
        //     never reap the daemon out from under a just-connected client and leave it
        //     a half-open socket. Only if STILL client-less do we break for teardown
        //     (which unlinks the socket); the re-check + break is the atomic
        //     "no-client-then-unlink" the critique requires.
        if all_sessions_closed(state) && hub.client_count() == 0 {
            quiesce_ticks = quiesce_ticks.saturating_add(1);
            if quiesce_ticks >= SELF_EXIT_GRACE_TICKS {
                // Final accept-drain: observe any connection that landed during grace.
                hub.drain_inbound_only(state, client, handle);
                if hub.client_count() == 0 {
                    break; // no client raced in → commit to self-exit + teardown
                }
                // A client connected during grace: abort the exit, serve it. Reset the
                // counter and flush it a snapshot next loop (its Attach was queued).
                quiesce_ticks = 0;
            }
        } else {
            // Live session or enrolled client → not quiescing; restart the grace clock.
            quiesce_ticks = 0;
        }

        // 4. Adaptive sleep — the SAME cadence the TUI input poll uses, minus the
        //    terminal: 8ms while there is live work (so background streams flush at
        //    >=60fps and animations advance), 100ms when fully idle so a quiet
        //    daemon burns no CPU. The busy branch keeps in-flight turns + delta
        //    emission prompt; a quiet daemon with an attached idle client still
        //    wakes every 100ms to notice the next change. A closed foreground reads
        //    `waiting == false` (see `SessionRuntime::close`), so a fully-tombstoned
        //    daemon naps at the idle cadence through its grace window.
        //    Stage 11, item 2: an APPROVAL-PARKED session keeps `waiting`/`is_working`
        //    true (so the daemon stays alive), but while DETACHED nothing can advance
        //    it — so don't busy-spin on it. `all_idle_or_parked_detached` is true when
        //    no session has self-advancing async work AND (any parked session has no
        //    client attached); in that case nap slow even though a session is "waiting".
        //    As soon as a client attaches (responsive approve) or any session resumes
        //    real work, the predicate flips false and the fast cadence returns. (This
        //    also tightens the old foreground-only `fg().waiting` check to ALL sessions,
        //    so a background stream now correctly keeps the daemon fast too.)
        let nap = if all_idle_or_parked_detached(state, hub.client_count() > 0) {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(8)
        };
        std::thread::sleep(nap);
    }
}

#[cfg(test)]
mod tests {
    //! Hub drive proof: exercise the hub with NO socket and assert (a) the seq'd
    //! frame stream — a Snapshot on attach, then a Delta on the next state change
    //! with `seq = N+1` (stage 4), and (b) that a controller's mutating request is
    //! actually applied through the shared action path, single-writer is enforced,
    //! the controller seat passes on detach, and QuitDaemon latches shutdown
    //! (stage 5). These stand in for the accept loop so the full drive path is
    //! covered without a real socket.

    use super::*;
    use crate::ipc::proto::{key_mods, KeyCodeWire, KeyWire, StateDelta};

    /// A keyless client + a current-thread tokio runtime — the minimal context the
    /// mutating-request path needs. `client = None` means a `Submit`/`Resend`-style
    /// action short-circuits to "no active session" (still `Ok`, so still `Ack`),
    /// while a `SendKey` editing the composer mutates `state` with no client at all.
    fn ctx() -> (Option<Arc<OpenRouterClient>>, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build test runtime");
        (None, rt)
    }

    /// Attaching a client yields a `Snapshot{seq=1}`; a subsequent status change
    /// yields a `Delta{seq=2}` carrying the new global status.
    #[test]
    fn attach_then_change_emits_snapshot_then_seqd_delta() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();

        // Stand in for a per-client task: a channel whose receiver we inspect.
        let (frame_tx, frame_rx) = std::sync::mpsc::channel::<DaemonFrame>();

        // Register + Attach in one drained batch (same as the bridge would deliver).
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 1,
                frame_tx,
            },
            &mut state,
            &mut client,
            &h,
        );
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::Attach {
                    foreground_id: None,
                    cwd: None,
                },
            },
            &mut state,
            &mut client,
            &h,
        );

        // Attach must have produced exactly a Snapshot at seq 1.
        let f1 = frame_rx.try_recv().expect("snapshot frame on attach");
        assert_eq!(f1.seq, 1, "first frame seq");
        assert!(
            matches!(f1.event, DaemonEvent::Snapshot(_)),
            "attach emits a Snapshot, got {:?}",
            f1.event
        );

        // Mutate render state: change the global status line.
        state.rest.status = "streaming".into();

        // One delta-stream pass must emit a single StatusChanged delta at seq 2.
        hub.stream_deltas(&state);
        let f2 = frame_rx.try_recv().expect("delta frame after change");
        assert_eq!(f2.seq, 2, "delta seq is N+1");
        match f2.event {
            DaemonEvent::Delta(StateDelta::StatusChanged { session_id, text }) => {
                assert_eq!(session_id, None, "global status delta");
                assert_eq!(text, "streaming");
            }
            other => panic!("expected StatusChanged delta, got {other:?}"),
        }

        // No spurious extra frames when nothing changed.
        hub.stream_deltas(&state);
        assert!(
            frame_rx.try_recv().is_err(),
            "no frame emitted when state is unchanged"
        );
    }

    /// A structural change (a session entering tool-approval) resyncs with a full
    /// `Snapshot`, not a partial delta. `awaiting_approval` is a structural field in
    /// the differ — it can't be folded incrementally, so the hub resends the whole
    /// projection.
    #[test]
    fn structural_change_emits_full_snapshot() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (frame_tx, frame_rx) = std::sync::mpsc::channel::<DaemonFrame>();

        hub.handle_inbound(
            HubInbound::Register {
                client_id: 7,
                frame_tx,
            },
            &mut state,
            &mut client,
            &h,
        );
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 7,
                req: ClientRequest::Attach {
                    foreground_id: None,
                    cwd: None,
                },
            },
            &mut state,
            &mut client,
            &h,
        );
        let _snap = frame_rx.try_recv().expect("attach snapshot");

        // Enter tool-approval on the foreground session — a structural change vs the
        // initial projection (awaiting_approval flipped).
        state.rest.fg_mut().awaiting_approval = true;

        hub.stream_deltas(&state);
        let f = frame_rx.try_recv().expect("frame after structural change");
        assert_eq!(f.seq, 2);
        assert!(
            matches!(f.event, DaemonEvent::Snapshot(_)),
            "structural change must resync with a full Snapshot, got {:?}",
            f.event
        );
    }

    /// An observer (second client) is rejected when it sends a mutating request,
    /// and the controller (first client) is acknowledged.
    #[test]
    fn observer_mutation_is_rejected_controller_acked() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<DaemonFrame>();
        let (obs_tx, obs_rx) = std::sync::mpsc::channel::<DaemonFrame>();

        // First registered = controller; second = observer.
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 1,
                frame_tx: ctl_tx,
            },
            &mut state,
            &mut client,
            &h,
        );
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 2,
                frame_tx: obs_tx,
            },
            &mut state,
            &mut client,
            &h,
        );

        // Controller submits input -> Ack (applied; with no client it lands as a
        // "no active session" status, still Ok -> Ack).
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::SubmitInput { text: "hi".into() },
            },
            &mut state,
            &mut client,
            &h,
        );
        assert!(matches!(
            ctl_rx.try_recv().expect("controller reply").event,
            DaemonEvent::Ack
        ));

        // Observer submits input -> Error, no mutation.
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 2,
                req: ClientRequest::SubmitInput { text: "nope".into() },
            },
            &mut state,
            &mut client,
            &h,
        );
        assert!(matches!(
            obs_rx.try_recv().expect("observer reply").event,
            DaemonEvent::Error(_)
        ));
    }

    /// An unknown session UUID on a UUID-keyed control request is an Error + no-op
    /// (critique #5), never a panic.
    #[test]
    fn unknown_session_uuid_errors_not_panics() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (tx, rx) = std::sync::mpsc::channel::<DaemonFrame>();
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 1,
                frame_tx: tx,
            },
            &mut state,
            &mut client,
            &h,
        );
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::SwitchForeground {
                    session_id: "does-not-exist".into(),
                },
            },
            &mut state,
            &mut client,
            &h,
        );
        assert!(matches!(
            rx.try_recv().expect("reply").event,
            DaemonEvent::Error(_)
        ));
    }

    /// A controller's `SendKey` is routed through the SAME local input pipeline:
    /// a printable char in Chat mode edits the shared composer. Proves the daemon
    /// drives real state via `handle_key` + `apply_action`, not a stub.
    #[test]
    fn controller_sendkey_edits_composer() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (tx, rx) = std::sync::mpsc::channel::<DaemonFrame>();

        hub.handle_inbound(
            HubInbound::Register {
                client_id: 1,
                frame_tx: tx,
            },
            &mut state,
            &mut client,
            &h,
        );
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::SendKey(KeyWire {
                    code: KeyCodeWire::Char('z'),
                    mods: 0,
                }),
            },
            &mut state,
            &mut client,
            &h,
        );

        assert_eq!(state.rest.input, "z", "key reached the composer via apply_action");
        assert!(matches!(
            rx.try_recv().expect("reply").event,
            DaemonEvent::Ack
        ));
    }

    /// When the controller detaches, the seat passes to the next remaining client,
    /// which can then mutate (single-writer controller-passing, DECISIONS).
    #[test]
    fn controller_seat_passes_on_detach() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (c1_tx, _c1_rx) = std::sync::mpsc::channel::<DaemonFrame>();
        let (c2_tx, c2_rx) = std::sync::mpsc::channel::<DaemonFrame>();

        // 1 = controller, 2 = observer.
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 1,
                frame_tx: c1_tx,
            },
            &mut state,
            &mut client,
            &h,
        );
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 2,
                frame_tx: c2_tx,
            },
            &mut state,
            &mut client,
            &h,
        );

        // Controller detaches -> seat passes to client 2.
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::Detach,
            },
            &mut state,
            &mut client,
            &h,
        );

        // Client 2's mutating request is now honoured (Ack), not rejected.
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 2,
                req: ClientRequest::SendKey(KeyWire {
                    code: KeyCodeWire::Char('q'),
                    mods: key_mods::CONTROL, // Ctrl+Q is a no-op key here, still Ack
                }),
            },
            &mut state,
            &mut client,
            &h,
        );
        assert!(matches!(
            c2_rx.try_recv().expect("promoted controller reply").event,
            DaemonEvent::Ack
        ));
    }

    /// `QuitDaemon` from the controller latches the shutdown flag the loop polls.
    #[test]
    fn quit_daemon_latches_shutdown() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (tx, rx) = std::sync::mpsc::channel::<DaemonFrame>();

        hub.handle_inbound(
            HubInbound::Register {
                client_id: 1,
                frame_tx: tx,
            },
            &mut state,
            &mut client,
            &h,
        );
        assert!(!hub.should_shutdown(), "shutdown not latched before QuitDaemon");

        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::QuitDaemon,
            },
            &mut state,
            &mut client,
            &h,
        );
        assert!(hub.should_shutdown(), "QuitDaemon latches shutdown");
        assert!(matches!(
            rx.try_recv().expect("reply").event,
            DaemonEvent::Ack
        ));
    }

    /// A `Disconnect` (socket EOF) deregisters the client and passes the controller
    /// seat, exactly like `Detach`.
    #[test]
    fn disconnect_deregisters_and_passes_seat() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (c1_tx, _c1_rx) = std::sync::mpsc::channel::<DaemonFrame>();
        let (c2_tx, c2_rx) = std::sync::mpsc::channel::<DaemonFrame>();

        hub.handle_inbound(
            HubInbound::Register {
                client_id: 1,
                frame_tx: c1_tx,
            },
            &mut state,
            &mut client,
            &h,
        );
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 2,
                frame_tx: c2_tx,
            },
            &mut state,
            &mut client,
            &h,
        );

        // Controller's socket dies.
        hub.handle_inbound(
            HubInbound::Disconnect { client_id: 1 },
            &mut state,
            &mut client,
            &h,
        );

        // Client 2 is now the controller and may mutate.
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 2,
                req: ClientRequest::SubmitInput { text: "x".into() },
            },
            &mut state,
            &mut client,
            &h,
        );
        assert!(matches!(
            c2_rx.try_recv().expect("promoted controller reply").event,
            DaemonEvent::Ack
        ));
    }

    // ─── daemon stage 10: tombstone close + self-exit ────────────────────────

    use crate::app::state::SessionRuntime;

    /// Append a fresh (live) session to `state` and return its stable id. Mirrors a
    /// `/new`-spawned parallel session for the close/foreground tests, but bypasses
    /// the credential flow (these tests only exercise tombstoning, not turns).
    fn push_session(state: &mut AppState) -> String {
        let rt = SessionRuntime::new();
        let id = rt.id.clone();
        state.rest.sessions.push(rt);
        id
    }

    /// `QuitSession` on a known id TOMBSTONES that session (sets `closed`, keeps the
    /// slot so no index shifts) and Acks; the other session stays live.
    #[test]
    fn quit_session_tombstones_keeps_slot_and_acks() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (tx, rx) = std::sync::mpsc::channel::<DaemonFrame>();

        // Two sessions: index 0 (initial) + index 1 (appended). Close index 1.
        let id1 = push_session(&mut state);
        let id0 = state.rest.sessions[0].id.clone();
        let len_before = state.rest.sessions.len();

        hub.handle_inbound(
            HubInbound::Register { client_id: 1, frame_tx: tx },
            &mut state,
            &mut client,
            &h,
        );
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::QuitSession { session_id: id1.clone() },
            },
            &mut state,
            &mut client,
            &h,
        );

        assert!(matches!(rx.try_recv().expect("reply").event, DaemonEvent::Ack));
        // Slot count unchanged (no Vec::remove — tombstone in place).
        assert_eq!(state.rest.sessions.len(), len_before, "tombstone keeps the slot");
        // The targeted session is closed; the other stays live.
        let s1 = state.rest.sessions.iter().find(|s| s.id == id1).expect("slot kept");
        assert!(s1.closed, "quit session is tombstoned");
        let s0 = state.rest.sessions.iter().find(|s| s.id == id0).expect("other slot");
        assert!(!s0.closed, "the other session stays live");
    }

    /// `QuitSession` on an unknown id is an Error + no-op (no session is closed).
    #[test]
    fn quit_session_unknown_id_errors_no_close() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (tx, rx) = std::sync::mpsc::channel::<DaemonFrame>();

        hub.handle_inbound(
            HubInbound::Register { client_id: 1, frame_tx: tx },
            &mut state,
            &mut client,
            &h,
        );
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::QuitSession { session_id: "nope".into() },
            },
            &mut state,
            &mut client,
            &h,
        );

        assert!(matches!(rx.try_recv().expect("reply").event, DaemonEvent::Error(_)));
        assert!(
            state.rest.sessions.iter().all(|s| !s.closed),
            "no session closed on unknown id"
        );
    }

    /// Closing the FOREGROUND session repoints `foreground` onto a still-live one so
    /// service/render never touch a tombstone (item 5).
    #[test]
    fn quit_foreground_repoints_to_live_session() {
        let mut state = AppState::new(Mode::Chat);
        let (mut client, rt) = ctx();
        let h = rt.handle().clone();
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (tx, _rx) = std::sync::mpsc::channel::<DaemonFrame>();

        // Session 1 appended; make IT the foreground, then close it.
        let id1 = push_session(&mut state);
        state.rest.foreground = 1;

        hub.handle_inbound(
            HubInbound::Register { client_id: 1, frame_tx: tx },
            &mut state,
            &mut client,
            &h,
        );
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::QuitSession { session_id: id1 },
            },
            &mut state,
            &mut client,
            &h,
        );

        // Foreground moved off the tombstone to the live session 0.
        assert_eq!(state.rest.foreground, 0, "foreground repointed to the live session");
        assert!(!state.rest.fg().closed, "foreground is a live session");
    }

    /// A CLOSED session is SKIPPED by `service_all_sessions`: the tombstone-skip guard
    /// short-circuits BEFORE any drain, so a Token queued on its `active_rx` is never
    /// consumed (the buffer stays empty) and the receiver is left in place untouched.
    /// Here the `closed` flag is set DIRECTLY (not via `close()`, which would drop the
    /// receiver + buffer) so the test isolates the servicer's skip, not `close()`'s
    /// teardown.
    #[test]
    fn closed_session_is_skipped_by_servicer() {
        let mut state = AppState::new(Mode::Chat);
        let (client, rt) = ctx();
        let h = rt.handle().clone();

        // Arm session 0 with a live receiver carrying one Token + an empty streaming
        // buffer, then tombstone it WITHOUT tearing it down.
        let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel::<crate::service::StreamEvent>();
        ev_tx
            .send(crate::service::StreamEvent::Token("hi".into()))
            .expect("queue a token");
        state.rest.sessions[0].active_rx = Some(ev_rx);
        state.rest.sessions[0].begin_stream(); // streaming = Some("")
        state.rest.sessions[0].closed = true;

        // Service: a skipped session drains nothing — the token is NOT appended and
        // the receiver is still parked on the session.
        let _ = service_all_sessions(&mut state, &client, &h);

        assert_eq!(
            state.rest.sessions[0].streaming.as_deref(),
            Some(""),
            "closed session was skipped: its streaming buffer stayed empty"
        );
        assert!(
            state.rest.sessions[0].active_rx.is_some(),
            "closed session was skipped: its receiver was never taken/drained"
        );
        // Keep the sender alive past the asserts so the channel isn't dropped early.
        drop(ev_tx);
    }

    /// `close_all_sessions` tombstones every session; `all_sessions_closed` then
    /// reports true (the trigger half of the self-exit condition).
    #[test]
    fn close_all_then_all_closed_true() {
        let mut state = AppState::new(Mode::Chat);
        let _id1 = push_session(&mut state);
        let _id2 = push_session(&mut state);
        assert!(!all_sessions_closed(&state), "not closed before kill-all");

        close_all_sessions(&mut state);

        assert!(all_sessions_closed(&state), "every session closed after kill-all");
        assert!(
            state.rest.sessions.iter().all(|s| !s.is_working()),
            "no tombstone reads as working"
        );
    }

    /// The forwarded `/quit` kill-all path: `handle_quit_kill_all` sets `should_quit`;
    /// one daemon-loop turn of that flag (simulated here) closes every session and
    /// clears the flag, so `all_sessions_closed` becomes true.
    #[test]
    fn should_quit_flag_drives_close_all() {
        let mut state = AppState::new(Mode::Chat);
        let _id1 = push_session(&mut state);

        // Stand in for a forwarded QuitConfirm [k]: set the flag the loop observes.
        state.rest.should_quit = true;

        // The loop's 3a step, inlined:
        if state.rest.should_quit {
            close_all_sessions(&mut state);
            state.rest.should_quit = false;
        }

        assert!(!state.rest.should_quit, "flag cleared by the daemon close path");
        assert!(all_sessions_closed(&state), "kill-all flag closed every session");
    }

    // ─── daemon stage 11: detached-approval park timeout + parked cadence ─────

    /// Put session `idx` into the PARKED-on-approval state with one unanswered risky
    /// tool call, mirroring what the approval machine leaves set when it pauses.
    fn park_on_approval(state: &mut AppState, idx: usize) {
        use crate::dto::chat::{FunctionCall, ToolCall};
        let s = &mut state.rest.sessions[idx];
        s.waiting = true; // a parked turn is still "working" (keeps the daemon alive)
        s.awaiting_approval = true;
        s.approval_reason = Some("writes outside workspace".into());
        s.pending_tool_calls = vec![ToolCall {
            id: "call-1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        }];
        s.tool_idx = 0;
    }

    /// Detached + awaiting: the first pass STAMPS the park timer but does NOT deny
    /// before the window elapses (the session stays parked, awaiting an operator).
    #[test]
    fn park_timer_stamps_when_detached_no_premature_deny() {
        let mut state = AppState::new(Mode::Chat);
        park_on_approval(&mut state, 0);
        assert!(state.rest.sessions[0].park_started_at.is_none(), "no timer yet");

        // No client attached → detached.
        let denied = service_approval_park_timeouts(&mut state, false);

        assert!(!denied, "nothing denied on the first detached tick");
        assert!(
            state.rest.sessions[0].park_started_at.is_some(),
            "the park timer is stamped on the first detached+awaiting tick"
        );
        assert!(
            state.rest.sessions[0].awaiting_approval,
            "still parked — the window has not elapsed"
        );
    }

    /// While a client IS attached, the timer never runs: it is cleared each pass and
    /// the session waits for its operator indefinitely (the intended pause).
    #[test]
    fn park_timer_cleared_while_client_attached() {
        let mut state = AppState::new(Mode::Chat);
        park_on_approval(&mut state, 0);
        // Pretend a prior detached tick had stamped it.
        state.rest.sessions[0].park_started_at = Some(Instant::now());

        // A client is attached → no timeout; the clock is reset.
        let denied = service_approval_park_timeouts(&mut state, true);

        assert!(!denied, "an attached operator is never auto-denied");
        assert!(
            state.rest.sessions[0].park_started_at.is_none(),
            "the timer is cleared while a client is attached"
        );
        assert!(state.rest.sessions[0].awaiting_approval, "still parked, waiting for the operator");
    }

    /// Once a DETACHED park exceeds `APPROVAL_PARK_TIMEOUT`, the pending call is
    /// auto-denied: `awaiting_approval` clears, the pending calls drain, `waiting`
    /// drops (the session goes idle), and the timer is cleared.
    #[test]
    fn park_timeout_auto_denies_after_window() {
        let mut state = AppState::new(Mode::Chat);
        park_on_approval(&mut state, 0);
        // Backdate the park start to just past the timeout so this pass expires it.
        let past = Instant::now()
            .checked_sub(APPROVAL_PARK_TIMEOUT + Duration::from_secs(1))
            .expect("instant far enough in the past");
        state.rest.sessions[0].park_started_at = Some(past);

        let denied = service_approval_park_timeouts(&mut state, false);

        assert!(denied, "the expired park was auto-denied");
        let s = &state.rest.sessions[0];
        assert!(!s.awaiting_approval, "auto-deny clears the approval park");
        assert!(s.pending_tool_calls.is_empty(), "pending calls were answered/drained");
        assert!(!s.waiting, "the session goes idle after the auto-deny");
        assert!(s.park_started_at.is_none(), "the park timer is cleared after the deny");
    }

    /// A session that is NOT awaiting approval has its timer cleared every pass — this
    /// is how an operator's approve/deny (which clears `awaiting_approval`) resets the
    /// clock for any future park.
    #[test]
    fn park_timer_reset_when_not_awaiting() {
        let mut state = AppState::new(Mode::Chat);
        // Stamp a stale timer but leave the session NOT awaiting.
        state.rest.sessions[0].park_started_at = Some(Instant::now());
        state.rest.sessions[0].awaiting_approval = false;

        let denied = service_approval_park_timeouts(&mut state, false);

        assert!(!denied, "an idle session is never denied");
        assert!(
            state.rest.sessions[0].park_started_at.is_none(),
            "a non-awaiting session's timer is cleared"
        );
    }

    /// A CLOSED (tombstoned) session is ignored by the park timer — even if it somehow
    /// still carries `awaiting_approval`, it is skipped (no deny, no timer touch).
    #[test]
    fn park_timeout_skips_closed_session() {
        let mut state = AppState::new(Mode::Chat);
        park_on_approval(&mut state, 0);
        let stamp = Instant::now()
            .checked_sub(APPROVAL_PARK_TIMEOUT + Duration::from_secs(1))
            .expect("past instant");
        state.rest.sessions[0].park_started_at = Some(stamp);
        // Tombstone the flag directly (not via close(), which would clear awaiting).
        state.rest.sessions[0].closed = true;

        let denied = service_approval_park_timeouts(&mut state, false);

        assert!(!denied, "a closed session is skipped, never auto-denied");
        assert!(
            state.rest.sessions[0].park_started_at.is_some(),
            "a closed session's fields are left untouched (skipped before any reset)"
        );
    }

    /// Cadence predicate: a DETACHED daemon with a session parked on approval and no
    /// other work reports "idle-or-parked-detached" (→ slow 100ms nap); attaching a
    /// client flips it false (→ fast 8ms for a responsive approve).
    #[test]
    fn cadence_slow_when_parked_detached_fast_when_attached() {
        let mut state = AppState::new(Mode::Chat);
        park_on_approval(&mut state, 0);

        // Detached + only-parked → nap slow.
        assert!(
            all_idle_or_parked_detached(&state, false),
            "detached + parked-on-approval should nap on the slow cadence"
        );
        // A client attached → keep the fast cadence so its approve is low-latency.
        assert!(
            !all_idle_or_parked_detached(&state, true),
            "an attached client over a parked session keeps the fast cadence"
        );
    }

    /// Cadence predicate: a session doing SELF-ADVANCING work (a live stream) is never
    /// "idle-or-parked" regardless of client attachment — the daemon must stay fast.
    #[test]
    fn cadence_fast_when_session_streaming() {
        let mut state = AppState::new(Mode::Chat);
        state.rest.sessions[0].begin_stream(); // streaming = Some("") → is_working()

        assert!(
            !all_idle_or_parked_detached(&state, false),
            "a streaming session keeps the daemon on the fast cadence (detached)"
        );
        assert!(
            !all_idle_or_parked_detached(&state, true),
            "a streaming session keeps the daemon on the fast cadence (attached)"
        );
    }

    /// Cadence predicate: a fully idle daemon (no work, nothing parked) naps slow
    /// whether or not a client is attached — the prior idle behaviour is preserved.
    #[test]
    fn cadence_slow_when_fully_idle() {
        let state = AppState::new(Mode::Chat);
        assert!(
            all_idle_or_parked_detached(&state, false),
            "idle + detached naps slow"
        );
        assert!(
            all_idle_or_parked_detached(&state, true),
            "idle + an attached-but-quiet client still naps slow"
        );
    }
}
