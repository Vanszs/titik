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
//! # Lifecycle (later stage)
//!
//! There is NO self-exit yet: the loop runs forever and is stopped with Ctrl-C.
//! The "live while >=1 session OR a client; self-exit on zero sessions AND no
//! client" rule is wired alongside the accept loop in a later stage.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use crate::app::mode::Mode;
use crate::app::state::AppState;
use crate::controller::command::Command;
use crate::controller::input::{handle_key, Action};
use crate::ipc::proto::{ClientRequest, DaemonEvent, DaemonFrame, StateSnapshot};
use crate::ipc::snapshot::{build_snapshot, diff};
use crate::service::openrouter::OpenRouterClient;

use super::super::actions::apply_action;
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
            ClientRequest::Attach { .. } => {
                // ATOMIC attach (critique #2): build the full snapshot, send it, and
                // flip the client to attached + seed ITS OWN baseline IN THIS TICK.
                // Only this client's baseline is (re)seeded (blocker #2) — never a
                // hub-global one — so a late attach can't swallow deltas another
                // already-attached client still owes; that client diffs against its
                // own untouched baseline.
                let snap = build_snapshot(state);
                self.send_to(idx, DaemonEvent::Snapshot(snap.clone()));
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
                self.send_to(idx, DaemonEvent::Snapshot(snap.clone()));
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

            // Forward a key to the foreground session through the EXACT local input
            // pipeline: KeyWire -> crossterm KeyEvent -> controller::handle_key ->
            // Action -> apply_action. So the daemon reuses the same per-mode key
            // handling (chat / pickers / forms) as the local TUI.
            ClientRequest::SendKey(key) => {
                let action = handle_key(state, key.to_key_event());
                let result = apply_action(action, state, client, handle);
                self.ack_or_error(idx, result);
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

            // Quit (close) a single session. Per-session close does not exist yet
            // (every live session holds its lock for its whole lifetime; `sessions`
            // is append-only this phase), so validate the id then reply a clear
            // Error — never a misleading Ack, never a panic.
            ClientRequest::QuitSession { session_id } => {
                let known = state.rest.sessions.iter().any(|s| s.id == session_id);
                if known {
                    self.send_to(
                        idx,
                        DaemonEvent::Error("QuitSession not yet supported".into()),
                    );
                } else {
                    self.send_to(
                        idx,
                        DaemonEvent::Error(format!("unknown session id: {session_id}")),
                    );
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
                self.send_to(i, DaemonEvent::Snapshot(next.clone()));
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

/// The headless daemon loop. Each tick services every session + every global
/// concern, drives the hub (drain inbound requests + apply mutations + stream
/// deltas), then sleeps on the adaptive cadence. No terminal, no input, no draw.
///
/// Returns on EITHER shutdown trigger so the caller's teardown (release every
/// session lock, drop the runtime, unlink socket + pidfile) runs:
/// 1. a controller sends [`ClientRequest::QuitDaemon`] (the hub latches its own
///    flag, observed via [`DaemonHub::should_shutdown`]), or
/// 2. the process receives SIGTERM/SIGINT — the signal task (installed in
///    [`super::super::run_daemon`]) flips `shutting_down`, which this loop polls
///    each tick. The loop stays SYNCHRONOUS: the async signal task only sets the
///    atomic; no awaiting happens in the loop body. The broader "self-exit on zero
///    sessions AND no client" lifecycle rule is a later stage.
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
    loop {
        // 1. Service EVERY session: drain each session's stream / tool-task /
        //    sub-agent channels and advance its turn. Identical to the TUI loop —
        //    the `dirty` return is irrelevant headless (nothing is drawn).
        let _ = service_all_sessions(state, client, handle);

        // 2. Service every GLOBAL concern (endpoint/warm/clipboard drains, the
        //    loading-splash state machine, deferred compaction apply, missing-root
        //    warning, comet-shimmer reconcile, toast tick). Same shared call the
        //    TUI loop uses, so the daemon never diverges on global handling.
        let _ = service_global(state, client, handle);

        // 3. Drive the hub: handle inbound client messages (register / attach /
        //    detach / resync / control) — atomically snapshotting each attaching
        //    client in THIS tick AND applying a controller's mutating requests
        //    against state/client via the shared action handlers — then stream this
        //    tick's render-state changes to every attached client as seq'd frames.
        hub.drain_inbound(state, client, handle);
        hub.stream_deltas(state);

        // 3b. Honour EITHER shutdown trigger so the caller's teardown (release every
        //     session lock, drop the runtime, unlink socket + pidfile) runs:
        //       - the hub's client-driven QuitDaemon flag, or
        //       - the process-level signal flag (SIGTERM/SIGINT) the signal task set.
        //     Checked AFTER streaming so a pending QuitDaemon Ack is flushed first.
        //     `Relaxed` is sufficient: this is a single boolean flag with no other
        //     memory it must publish/acquire — teardown reads only owned `state`.
        if hub.should_shutdown() || shutting_down.load(Ordering::Relaxed) {
            break;
        }

        // 4. Adaptive sleep — the SAME cadence the TUI input poll uses, minus the
        //    terminal: 8ms while there is live work (so background streams flush at
        //    >=60fps and animations advance), 100ms when fully idle so a quiet
        //    daemon burns no CPU. The busy branch keeps in-flight turns + delta
        //    emission prompt; a quiet daemon with an attached idle client still
        //    wakes every 100ms to notice the next change.
        let busy = state.rest.fg().waiting
            || state.rest.catalogue_pending.is_some()
            || matches!(state.mode, Mode::Loading(_))
            || has_running_subagents(state);
        let nap = if busy {
            Duration::from_millis(8)
        } else {
            Duration::from_millis(100)
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
}
