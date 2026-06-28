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

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use crate::app::mode::Mode;
use crate::app::state::AppState;
use crate::ipc::proto::{ClientRequest, DaemonEvent, DaemonFrame, StateSnapshot};
use crate::ipc::snapshot::{build_snapshot, diff};
use crate::service::openrouter::OpenRouterClient;

use super::global::{has_running_subagents, service_global};
use super::sessions::service_all_sessions;

/// One inbound message on the sync-loop bridge, tagged with the client it came
/// from. A per-client tokio task (later stage) emits a [`HubInbound::Register`]
/// first (handing the loop its frame sender), then one [`HubInbound::Request`] per
/// framed [`ClientRequest`] it reads off the socket, and finally drops its sender
/// (which the loop reads as that client disconnecting).
#[allow(dead_code)] // accept loop that produces these lands in daemon stage 5
pub(in crate::app::runtime) enum HubInbound {
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
}

impl DaemonHub {
    /// Build an empty hub plus the paired message-sender the accept loop will clone
    /// into each per-client task. The caller (the daemon runner) holds the returned
    /// [`Sender`] for the daemon's lifetime so `msg_rx` never observes a premature
    /// `Disconnected` before any client has connected.
    #[allow(dead_code)] // accept loop that clones the sender lands in daemon stage 5
    pub(in crate::app::runtime) fn new() -> (Self, Sender<HubInbound>) {
        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        (
            Self {
                msg_rx,
                clients: Vec::new(),
            },
            msg_tx,
        )
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

    /// Drop any client whose frame channel has closed (its per-client task ended).
    /// Detecting closure on the SENDER side isn't directly possible, so closed
    /// channels are pruned lazily: a send that errored leaves the client in place
    /// here, but an explicit `Detach` (or a future task-exit signal) removes it. For
    /// now this prunes nothing extra — closure is observed via `Detach`/`msg_rx`.
    fn sweep_dead(&mut self) {
        // Placeholder for stage 5's task-exit reaping; intentionally a no-op now so
        // the call site reads clearly. (A `Sender` cannot probe receiver liveness.)
    }

    /// Handle every inbound bridge message queued this tick, building+sending a
    /// snapshot for each attaching/resyncing client IN THE SAME TICK (critique #2).
    /// Returns nothing; frames are pushed onto the relevant clients' channels.
    fn drain_inbound(&mut self, state: &AppState) {
        loop {
            match self.msg_rx.try_recv() {
                Ok(msg) => self.handle_inbound(msg, state),
                Err(TryRecvError::Empty) => break,
                // No client has ever connected (the runner still holds the paired
                // sender) or every task dropped its sender — nothing to drain.
                Err(TryRecvError::Disconnected) => break,
            }
        }
        self.sweep_dead();
    }

    /// Apply one bridge message against the registry / emit its reply.
    fn handle_inbound(&mut self, msg: HubInbound, state: &AppState) {
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
                self.handle_request(client_id, req, state);
            }
        }
    }

    /// Route one [`ClientRequest`] from `client_id`. Read-only requests are honoured
    /// for any client; mutating requests are rejected for observers (single-writer).
    fn handle_request(&mut self, client_id: u64, req: ClientRequest, state: &AppState) {
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
                // Drop the client from the registry. If the controller detaches, the
                // controller seat is vacated; promoting the next observer to writer
                // is a lifecycle-stage concern (DECISIONS) — left to stage 5.
                self.clients.remove(idx);
            }

            // --- mutating (single-writer: observers are rejected) ---
            req => {
                if !self.clients[idx].is_controller {
                    self.send_to(
                        idx,
                        DaemonEvent::Error("read-only observer: mutation rejected".into()),
                    );
                    return;
                }
                self.handle_controller_mutation(idx, req, state);
            }
        }
    }

    /// Handle a MUTATING request from the controller. The actual state mutation
    /// (foreground switch, input submit, key routing, approval, new/quit session)
    /// lands in daemon stage 5; this stage validates the request shape and replies,
    /// but does NOT yet mutate `state` — so it can already enforce the locked
    /// invariant (critique #5: an unknown session UUID is an Error + no-op, never a
    /// panic, never a wrong-index switch). Known/valid requests get an `Ack` stub.
    fn handle_controller_mutation(&mut self, idx: usize, req: ClientRequest, state: &AppState) {
        match req {
            // UUID-keyed control: reject an unknown id with Error (critique #5).
            ClientRequest::SwitchForeground { session_id }
            | ClientRequest::QuitSession { session_id } => {
                let known = state.rest.sessions.iter().any(|s| s.id == session_id);
                if known {
                    // Real switch/quit is stage 5; acknowledge the (validated) id.
                    self.send_to(idx, DaemonEvent::Ack);
                } else {
                    self.send_to(
                        idx,
                        DaemonEvent::Error(format!("unknown session id: {session_id}")),
                    );
                }
            }
            // Other mutations (SubmitInput / SendKey / ApproveTool / NewSession /
            // QuitDaemon): the real handlers are stage 5. Acknowledge so the wire
            // contract is satisfied; no state is touched yet.
            _ => {
                self.send_to(idx, DaemonEvent::Ack);
            }
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

/// The headless daemon loop. Runs forever (no self-exit this stage): each tick
/// services every session + every global concern, drives the hub (drain inbound +
/// stream deltas), then sleeps on the adaptive cadence. No terminal, no input, no
/// draw.
///
/// `client` is `&mut` only to match `service_*`'s signature (a debounced catalogue
/// fetch can replace the keyless client); the daemon never reads it.
pub(in crate::app::runtime) fn daemon_loop(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
    hub: &mut DaemonHub,
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
        //    client in THIS tick — then stream this tick's render-state changes to
        //    every attached client as seq-tagged frames.
        hub.drain_inbound(state);
        hub.stream_deltas(state);

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
    //! Hub emission proof (daemon stage 4): drive the hub with NO socket and assert
    //! the seq'd frame stream — a Snapshot on attach, then a Delta on the next
    //! state change with `seq = N+1`. This stands in for the accept loop (stage 5)
    //! so the emission path is covered now.

    use super::*;
    use crate::ipc::proto::StateDelta;

    /// Attaching a client yields a `Snapshot{seq=1}`; a subsequent status change
    /// yields a `Delta{seq=2}` carrying the new global status.
    #[test]
    fn attach_then_change_emits_snapshot_then_seqd_delta() {
        let mut state = AppState::new(Mode::Chat);
        let (mut hub, _runner_tx) = DaemonHub::new();

        // Stand in for a per-client task: a channel whose receiver we inspect.
        let (frame_tx, frame_rx) = std::sync::mpsc::channel::<DaemonFrame>();

        // Register + Attach in one drained batch (same as the bridge would deliver).
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 1,
                frame_tx,
            },
            &state,
        );
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::Attach {
                    foreground_id: None,
                },
            },
            &state,
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
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (frame_tx, frame_rx) = std::sync::mpsc::channel::<DaemonFrame>();

        hub.handle_inbound(
            HubInbound::Register {
                client_id: 7,
                frame_tx,
            },
            &state,
        );
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 7,
                req: ClientRequest::Attach {
                    foreground_id: None,
                },
            },
            &state,
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
        let state = AppState::new(Mode::Chat);
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (ctl_tx, ctl_rx) = std::sync::mpsc::channel::<DaemonFrame>();
        let (obs_tx, obs_rx) = std::sync::mpsc::channel::<DaemonFrame>();

        // First registered = controller; second = observer.
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 1,
                frame_tx: ctl_tx,
            },
            &state,
        );
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 2,
                frame_tx: obs_tx,
            },
            &state,
        );

        // Controller submits input -> Ack (stub; real mutation is stage 5).
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::SubmitInput { text: "hi".into() },
            },
            &state,
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
            &state,
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
        let state = AppState::new(Mode::Chat);
        let (mut hub, _runner_tx) = DaemonHub::new();
        let (tx, rx) = std::sync::mpsc::channel::<DaemonFrame>();
        hub.handle_inbound(
            HubInbound::Register {
                client_id: 1,
                frame_tx: tx,
            },
            &state,
        );
        hub.handle_inbound(
            HubInbound::Request {
                client_id: 1,
                req: ClientRequest::SwitchForeground {
                    session_id: "does-not-exist".into(),
                },
            },
            &state,
        );
        assert!(matches!(
            rx.try_recv().expect("reply").event,
            DaemonEvent::Error(_)
        ));
    }
}
