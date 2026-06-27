//! Headless daemon event loop — the `koma --daemon` core.
//!
//! [`daemon_loop`] mirrors the STRUCTURE of [`super::run_loop`] but with the
//! terminal stripped: no `terminal.draw(...)`, no crossterm input poll/read, no
//! `/select` copy mode. Per tick it does exactly the render-agnostic half of the
//! interactive loop — [`super::sessions::service_all_sessions`] (advance every
//! session's turn) + [`super::global::service_global`] (every global drain) —
//! then drains client requests off the [`DaemonHub`] and sleeps on the SAME
//! adaptive 8ms-busy / 100ms-idle cadence the TUI uses. Sharing
//! `service_all_sessions` + `service_global` is what keeps the daemon and the TUI
//! client from ever diverging on runtime behaviour.
//!
//! # Sync-loop bridge (critique #1)
//!
//! This loop is SYNCHRONOUS (it `try_recv`s, it `thread::sleep`s) — it is NOT
//! rewritten async. The eventual socket server runs per-client tokio tasks on the
//! existing runtime; those tasks talk to THIS loop over plain `std::sync::mpsc`
//! channels carried by [`DaemonHub`]: client requests arrive on `req_rx` (drained
//! here each tick, exactly like a session's `active_rx`), and per-client frame
//! senders live in `clients` for the loop to push [`DaemonFrame`]s to. This stage
//! only proves that bridge SHAPE compiles and ticks: requests are drained and
//! logged/ignored, and no frames are emitted yet. Real request handling + frame
//! emission + the accept loop land in daemon stage 4-5.
//!
//! # Lifecycle (later stage)
//!
//! There is NO self-exit yet: the loop runs forever and is stopped with Ctrl-C
//! (nothing here traps SIGINT, so the default terminate handler applies). The
//! "live while >=1 session OR a client; self-exit on zero sessions AND no client"
//! rule is wired alongside the accept loop in a later stage.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use crate::app::mode::Mode;
use crate::app::state::AppState;
use crate::ipc::proto::{ClientRequest, DaemonFrame};
use crate::service::openrouter::OpenRouterClient;

use super::global::{has_running_subagents, service_global};
use super::sessions::service_all_sessions;

/// The sync-loop <-> per-client-task bridge (critique #1).
///
/// Owns the daemon side of the channels the eventual socket server uses to talk
/// to the synchronous [`daemon_loop`]:
/// - `req_rx`: client requests, produced by per-client tokio tasks (each holding a
///   clone of the paired [`Sender<ClientRequest>`]) and `try_recv`'d each tick —
///   the same shape as a session's `active_rx` drain.
/// - `clients`: per-client frame senders the loop pushes [`DaemonFrame`]s onto
///   (one per connected client). Registered by the accept loop in a later stage.
///
/// Dead until stage 4-5 wires the accept loop + request handling; constructed
/// (empty) here so the bridge compiles and the loop drains it every tick.
#[allow(dead_code)] // wired in daemon stage 4-5 (accept loop + request handling)
pub(in crate::app::runtime) struct DaemonHub {
    /// Inbound client requests, drained per tick (like `active_rx`).
    pub req_rx: Receiver<ClientRequest>,
    /// Outbound per-client frame senders; the loop fans [`DaemonFrame`]s out here.
    pub clients: Vec<Sender<DaemonFrame>>,
}

impl DaemonHub {
    /// Build an empty hub plus the paired request-sender the accept loop will
    /// clone into each per-client task. The caller (the daemon runner) holds the
    /// returned [`Sender`] for the daemon's lifetime so `req_rx` never observes a
    /// premature `Disconnected` before any client has connected.
    #[allow(dead_code)] // wired in daemon stage 4-5 (accept loop + request handling)
    pub(in crate::app::runtime) fn new() -> (Self, Sender<ClientRequest>) {
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        (
            Self {
                req_rx,
                clients: Vec::new(),
            },
            req_tx,
        )
    }
}

/// The headless daemon loop. Runs forever (no self-exit this stage): each tick
/// services every session + every global concern, drains pending client requests
/// off `hub`, then sleeps on the adaptive cadence. No terminal, no input, no draw.
///
/// `client` is `&mut` only to match `service_*`'s signature (a debounced
/// catalogue fetch can replace the keyless client); the daemon never reads it.
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

        // 3. Drain inbound client requests (critique #1: the sync-loop bridge).
        //    SCAFFOLDING ONLY this stage — requests are logged and ignored; the
        //    accept loop that feeds `req_rx` and the per-request handlers (which
        //    will mutate `state` and emit frames onto `hub.clients`) land in daemon
        //    stage 4-5. Drain to empty each tick so a future backlog can't build up.
        loop {
            match hub.req_rx.try_recv() {
                Ok(req) => {
                    // No handling yet — record at debug level so the bridge is
                    // observable without affecting the (silent) headless run.
                    debug_ignore_request(&req);
                }
                Err(TryRecvError::Empty) => break,
                // No client has ever connected (the runner still holds the paired
                // sender), or every client task has dropped its sender. Either way
                // there is nothing to drain; stop until the next tick.
                Err(TryRecvError::Disconnected) => break,
            }
        }

        // 4. Adaptive sleep — the SAME cadence the TUI input poll uses, minus the
        //    terminal: 8ms while there is live work (so background streams flush at
        //    >=60fps and animations advance), 100ms when fully idle so a quiet
        //    daemon burns no CPU. With no socket wired yet there is nothing to wake
        //    the loop early, so this sleep IS the tick clock; the busy branch keeps
        //    in-flight turns progressing promptly.
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

/// Record a drained-but-unhandled client request without doing anything with it.
///
/// Stage-3 scaffolding: the request vocabulary exists and the bridge ticks, but
/// no handler is wired yet. Kept as a named no-op (rather than `let _ = req`) so
/// the drain site reads clearly and a later stage has an obvious seam to replace.
#[allow(dead_code)] // wired in daemon stage 4-5 (replaced by real request handlers)
fn debug_ignore_request(_req: &ClientRequest) {
    // Intentionally empty: real handling lands in daemon stage 4-5.
}
