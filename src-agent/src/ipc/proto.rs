//! Wire protocol vocabulary for the koma daemon <-> client split.
//!
//! These are PURE-DATA, serde-round-trippable types — the only things that ever
//! cross the unix-socket boundary between the headless `koma-daemon` (which owns
//! the agent runtime + session locks) and a thin attach/detach TUI client.
//!
//! # Why dedicated DTOs (not the live runtime types)
//!
//! The live state types ([`crate::app::state::SessionRuntime`],
//! [`crate::app::subagent::SubAgent`], the [`crate::app::mode::Mode`] variants,
//! …) hold non-serialisable, non-`Send`-friendly machinery: tokio channels,
//! `AbortHandle`s, `Instant`s, `Cell`/`RefCell`s. Deriving serde on them is
//! impossible and would also be wrong — a snapshot must be a frozen *projection*
//! of display state, not a live handle. So every type here is plain data built
//! by copying out of the live state at send time (see the daemon snapshot stage).
//!
//! # Critique fixes baked into the protocol (designed in now, on purpose)
//!
//! - **Stable session ids (#2).** Sessions are addressed by their UUID
//!   ([`crate::app::state::SessionRuntime::id`]), NEVER by `Vec` index. Later
//!   lifecycle will TOMBSTONE sessions rather than `Vec::remove` them, so an
//!   index would silently cross-wire streams; the client only ever speaks UUIDs.
//! - **Monotonic seq (#4).** Every [`DaemonFrame`] carries a `seq`. The client
//!   detects a gap (one dropped delta = a permanently-wrong shadow) and recovers
//!   by sending [`ClientRequest::Resync`] to get a fresh full [`StateSnapshot`].
//! - **Frame-size cap (#5).** [`MAX_FRAME_BYTES`] bounds any length-prefixed read
//!   so a hostile/garbage length prefix can never trigger an unbounded alloc.
//!
//! No callers reference these yet — they are the wire vocabulary the daemon and
//! client will speak in a later stage. They are exercised (and kept warning-free)
//! by the round-trip test at the bottom of [`super`].

use serde::{Deserialize, Serialize};

use crate::dto::chat::ChatMessage;
use crate::service::StreamEvent;

/// Hard upper bound on a single length-prefixed frame's payload size (64 MiB).
///
/// The framed-read side MUST reject any length prefix exceeding this BEFORE
/// allocating the read buffer (critique #5): the length prefix is attacker- /
/// corruption-controlled, so trusting it would let one bad frame OOM the process.
#[allow(dead_code)] // wired in daemon stage 2+ (framed read enforces it)
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

// ─── client -> daemon ────────────────────────────────────────────────────────

/// A request sent from a TUI client to the daemon over the unix socket.
///
/// One framed JSON value per request. The daemon applies it against the owned
/// runtime and replies (where applicable) with a [`DaemonFrame`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub enum ClientRequest {
    /// Attach this client to the daemon. `foreground_id` optionally requests
    /// which session should be foreground on attach (by stable UUID); `None`
    /// keeps the daemon's current foreground. The daemon replies with a full
    /// [`DaemonEvent::Snapshot`].
    Attach { foreground_id: Option<String> },
    /// Detach this client. The daemon keeps every session running (approval-
    /// when-detached pauses a session awaiting approval until a client reattaches);
    /// it self-exits only when zero sessions AND no client remain.
    Detach,
    /// Ask the daemon to enumerate its sessions (answered with a fresh snapshot).
    ListSessions,
    /// Recover from a detected seq gap: the daemon replies with a full
    /// [`DaemonEvent::Snapshot`] so the client can rebuild its shadow from scratch.
    Resync,
    /// Switch the foreground session to the one with this stable UUID.
    SwitchForeground { session_id: String },
    /// Submit composed input text to the FOREGROUND session (equivalent to the
    /// user pressing Enter on a finished composer).
    SubmitInput { text: String },
    /// Forward a single key event to the daemon (routed to whatever modal/handler
    /// the foreground session's mode dictates), as a serde-safe [`KeyWire`].
    SendKey(KeyWire),
    /// Answer the foreground session's pending tool-approval prompt.
    ApproveTool { approve: bool },
    /// Spawn a fresh parallel session, optionally named / rooted at a directory.
    NewSession {
        name: Option<String>,
        working_dir: Option<String>,
    },
    /// Quit (abort + release lock + drop) the session with this stable UUID.
    QuitSession { session_id: String },
    /// Ask the daemon to shut down entirely (abort all sessions, release all
    /// locks, remove the socket, exit).
    QuitDaemon,
}

// ─── key projection ──────────────────────────────────────────────────────────

/// A serde-safe projection of a crossterm key code.
///
/// crossterm's `KeyCode` is not serde here, so this mirrors the subset the TUI
/// controller actually consumes (see `controller::input`), plus a catch-all
/// [`KeyCodeWire::Other`] so an unmapped key round-trips losslessly rather than
/// being silently dropped. Modifiers ride alongside in [`KeyWire::mods`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub enum KeyCodeWire {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// Any key code outside the mapped set, preserved as crossterm's Debug string
    /// so a future key never round-trips to the wrong variant. Not re-injectable
    /// into a precise crossterm `KeyCode` (maps back to `Null`), but never lost.
    Other(String),
}

/// Modifier bitfield bits for [`KeyWire::mods`]. A serde-stable, crossterm-
/// independent encoding (crossterm's `KeyModifiers` bits are not part of our wire
/// contract). Combine with bitwise OR.
pub mod key_mods {
    pub const SHIFT: u8 = 0b0000_0001;
    pub const CONTROL: u8 = 0b0000_0010;
    pub const ALT: u8 = 0b0000_0100;
}

/// A serde-safe projection of a crossterm `KeyEvent` (code + modifier bitfield).
///
/// Built from a live `KeyEvent` with [`From`]; converted back to one the daemon
/// can feed to the controller with [`KeyWire::to_key_event`]. Only `KeyPress`-
/// relevant data is carried (kind/state are reconstructed as defaults), which is
/// all the TUI controller inspects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct KeyWire {
    pub code: KeyCodeWire,
    /// OR of the [`key_mods`] bits.
    pub mods: u8,
}

impl From<ratatui::crossterm::event::KeyEvent> for KeyWire {
    fn from(ev: ratatui::crossterm::event::KeyEvent) -> Self {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let code = match ev.code {
            KeyCode::Char(c) => KeyCodeWire::Char(c),
            KeyCode::Enter => KeyCodeWire::Enter,
            KeyCode::Esc => KeyCodeWire::Esc,
            KeyCode::Backspace => KeyCodeWire::Backspace,
            KeyCode::Delete => KeyCodeWire::Delete,
            KeyCode::Tab => KeyCodeWire::Tab,
            KeyCode::BackTab => KeyCodeWire::BackTab,
            KeyCode::Up => KeyCodeWire::Up,
            KeyCode::Down => KeyCodeWire::Down,
            KeyCode::Left => KeyCodeWire::Left,
            KeyCode::Right => KeyCodeWire::Right,
            KeyCode::Home => KeyCodeWire::Home,
            KeyCode::End => KeyCodeWire::End,
            KeyCode::PageUp => KeyCodeWire::PageUp,
            KeyCode::PageDown => KeyCodeWire::PageDown,
            other => KeyCodeWire::Other(format!("{other:?}")),
        };
        let mut mods = 0u8;
        if ev.modifiers.contains(KeyModifiers::SHIFT) {
            mods |= key_mods::SHIFT;
        }
        if ev.modifiers.contains(KeyModifiers::CONTROL) {
            mods |= key_mods::CONTROL;
        }
        if ev.modifiers.contains(KeyModifiers::ALT) {
            mods |= key_mods::ALT;
        }
        Self { code, mods }
    }
}

impl KeyWire {
    /// Rebuild a crossterm `KeyEvent` for the daemon to feed to the controller.
    ///
    /// [`KeyCodeWire::Other`] maps to `KeyCode::Null` (it carries only a debug
    /// label, not a re-injectable code); every mapped variant round-trips exactly.
    #[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
    pub fn to_key_event(&self) -> ratatui::crossterm::event::KeyEvent {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let code = match &self.code {
            KeyCodeWire::Char(c) => KeyCode::Char(*c),
            KeyCodeWire::Enter => KeyCode::Enter,
            KeyCodeWire::Esc => KeyCode::Esc,
            KeyCodeWire::Backspace => KeyCode::Backspace,
            KeyCodeWire::Delete => KeyCode::Delete,
            KeyCodeWire::Tab => KeyCode::Tab,
            KeyCodeWire::BackTab => KeyCode::BackTab,
            KeyCodeWire::Up => KeyCode::Up,
            KeyCodeWire::Down => KeyCode::Down,
            KeyCodeWire::Left => KeyCode::Left,
            KeyCodeWire::Right => KeyCode::Right,
            KeyCodeWire::Home => KeyCode::Home,
            KeyCodeWire::End => KeyCode::End,
            KeyCodeWire::PageUp => KeyCode::PageUp,
            KeyCodeWire::PageDown => KeyCode::PageDown,
            KeyCodeWire::Other(_) => KeyCode::Null,
        };
        let mut modifiers = KeyModifiers::empty();
        if self.mods & key_mods::SHIFT != 0 {
            modifiers |= KeyModifiers::SHIFT;
        }
        if self.mods & key_mods::CONTROL != 0 {
            modifiers |= KeyModifiers::CONTROL;
        }
        if self.mods & key_mods::ALT != 0 {
            modifiers |= KeyModifiers::ALT;
        }
        KeyEvent::new(code, modifiers)
    }
}

// ─── daemon -> client ────────────────────────────────────────────────────────

/// The daemon -> client envelope. Carries a monotonic `seq` so the client can
/// detect a dropped frame (critique #4): on a gap it issues
/// [`ClientRequest::Resync`] and rebuilds from the next [`DaemonEvent::Snapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct DaemonFrame {
    /// Monotonically increasing per connection; a gap means a frame was lost.
    pub seq: u64,
    pub event: DaemonEvent,
}

/// What a [`DaemonFrame`] carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub enum DaemonEvent {
    /// A full state projection — sent on attach and on resync. The client rebuilds
    /// its entire shadow from this.
    Snapshot(StateSnapshot),
    /// An incremental update folded onto the existing shadow.
    Delta(StateDelta),
    /// Acknowledgement of a request that produces no other reply.
    Ack,
    /// A request failed; the `String` is a human-readable reason.
    Error(String),
}

// ─── full-state snapshot (pure data) ─────────────────────────────────────────

/// A complete, frozen projection of the daemon's renderable state. Built by
/// copying out of the live state (never by deriving serde on it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct StateSnapshot {
    /// Stable UUID of the foreground session, or `None` if there are none.
    pub foreground_id: Option<String>,
    pub sessions: Vec<SessionSnapshot>,
    pub global: GlobalSnapshot,
}

/// A per-session projection of everything the client needs to render ONE session
/// tab. Mirrors the display-relevant fields of
/// [`crate::app::state::SessionRuntime`] (plus the session name) as plain data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct SessionSnapshot {
    /// Stable UUID — the ONLY handle the client uses to address this session.
    pub id: String,
    /// Human-readable session name (from its `Session`), empty if unnamed/none.
    pub name: String,
    /// Committed conversation messages (already serde via `ChatMessage`).
    pub messages: Vec<ChatMessage>,
    /// In-progress assistant content buffer, or `None` when not streaming.
    pub streaming: Option<String>,
    /// In-progress reasoning/thinking buffer (display-only; empty if none).
    pub stream_reasoning: String,
    /// Current context size (latest prompt tokens), not a running sum.
    pub tokens_in: u64,
    /// Cumulative completion tokens.
    pub tokens_out: u64,
    /// Cumulative USD cost.
    pub cost: f64,
    /// Prompt tokens served from cache on the latest response.
    pub tokens_cached: u64,
    /// A turn is waiting on the model/tool/fold.
    pub waiting: bool,
    /// A risky tool call is paused for `y/n` approval.
    pub awaiting_approval: bool,
    /// Why the tool-call classifier flagged the paused call (if classifier-driven).
    pub approval_reason: Option<String>,
    /// Mirror of `SessionRuntime::is_working()` at snapshot time — any work in
    /// flight (streaming, waiting, parked lanes, or a running sub-agent).
    pub working: bool,
    /// A NON-foreground session finished and its completion hasn't been seen yet
    /// (drives the "session ready" nudge / unseen marker).
    pub finished_unseen: bool,
    /// Projections of this session's sub-agents (running + finished history).
    pub subagents: Vec<SubAgentSnapshot>,
    /// Projections of this session's QUEUED-but-not-yet-started delegations (the
    /// over-cap [`crate::app::subagent::PendingSubagent`] FIFO). Carried so the
    /// remote `$` panel can render the "pending" rows a local TUI shows — without
    /// this the client's reconstructed session has an empty queue and the panel
    /// never lists waiting delegations.
    pub pending_subagents: Vec<PendingSubagentSnapshot>,
}

/// A plain-data projection of one [`crate::app::subagent::SubAgent`] — NOT the
/// live handle (no `rx` channel, no `AbortHandle`).
///
/// Carries everything the `$` panel + the full-screen viewer read off a live
/// `SubAgent`: the stable id, the agent name + compact label, the lifecycle
/// status string, the FULL transcript (so the viewer's body and the panel's tail
/// both render), and the structured `messages` the viewer renders as a chat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct SubAgentSnapshot {
    /// Stable per-session id (the `#N` shown in the panel/viewer headers).
    pub id: usize,
    /// The agent definition's name this sub-agent runs.
    pub name: String,
    /// Compact one-line label (the truncated task) shown in the panel list.
    pub label: String,
    /// Lifecycle state rendered as a short string ("running" / "done" /
    /// "killed" / "error: …"), so the client needn't mirror the status enum.
    pub status: String,
    /// Progress proxy: number of accumulated transcript lines so far.
    pub steps: usize,
    /// The accumulated transcript lines. Carried in full (not just a tail) so the
    /// client can reconstruct a `SubAgent` whose panel preview AND inline running
    /// indicator render exactly as the local TUI's do.
    pub transcript: Vec<String>,
    /// The sub-agent's structured conversation, rendered by the full-screen viewer
    /// exactly like the main chat. Empty until the first turn commits.
    pub messages: Vec<ChatMessage>,
}

/// A plain-data projection of one queued [`crate::app::subagent::PendingSubagent`]
/// — a delegation accepted but parked behind the concurrency cap. Carries only the
/// fields the `$` panel's "pending" row renders (id + agent + prompt); the live
/// `tool_call_id` is daemon-internal turn-bookkeeping, never rendered, so it is not
/// projected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct PendingSubagentSnapshot {
    /// Stable id pre-allocated at enqueue time (the `#N` in the pending row).
    pub id: usize,
    /// The agent definition's name the queued delegation will run.
    pub agent_name: String,
    /// The task prompt the queued delegation will be seeded with.
    pub prompt: String,
}

/// Projection of the mode-independent, NON-session global UI state (the flat
/// fields on [`crate::app::state::AppStateRest`] the client renders).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct GlobalSnapshot {
    /// The shared composer text.
    pub input: String,
    /// Caret position within `input` (char index).
    pub cursor: usize,
    /// Transcript scroll offset (top visual line).
    pub scroll: u16,
    /// Whether the transcript is pinned to the bottom (auto-follow).
    pub follow: bool,
    /// The status-line text.
    pub status: String,
    /// Elapsed milliseconds since the foreground session's CURRENT run of work
    /// began, or `None` when idle. The live state holds an `Instant`
    /// ([`crate::app::state::AppStateRest::work_since`]) which cannot serialise;
    /// projected here as elapsed-ms so the client re-anchors its OWN clock
    /// (`Instant::now() - elapsed`) and the status comet animates from the SAME
    /// phase + elapsed-seconds counter the daemon is at — instead of resetting to
    /// 0 every time a snapshot rebuilds the shadow.
    pub work_elapsed_ms: Option<u64>,
    /// A PURE-DATA projection of the current [`crate::app::mode::Mode`] carrying
    /// each mode's render-relevant payload, so the thin client can reconstruct +
    /// draw every screen (not just Chat). See [`ModeSnapshot`].
    pub mode: ModeSnapshot,
    /// Active toast as `(kind, text)`, or `None`. `kind` is "error" / "info".
    pub toast: Option<(String, String)>,
}

/// A PURE-DATA projection of the live [`crate::app::mode::Mode`].
///
/// Unlike the old lightweight discriminant, this carries each mode's
/// render-relevant payload so the thin client can RECONSTRUCT the mode and draw
/// it through the unmodified `view::draw` — the goal of making the client render
/// every screen, not just Chat. It stays pure data (no channels / `Instant` /
/// `Cell`): every field is copied out of the live mode at snapshot time.
///
/// # Staging (task #122)
///
/// This stage (1) fills the payloads for the two ALREADY-projected screens —
/// `Chat` (payload-free) and `QuitConfirm` (its busy/total counts) — and keeps
/// every OTHER variant a STUB (no payload yet). The stubbed variants exist so the
/// enum is 1:1 with `Mode` and the wire type is stable; their forms/pickers/
/// dashboards are projected in stages 2-3 when the client renders them. A stubbed
/// variant tells the client only "this screen is active"; the client falls back to
/// the safe Chat render for it until its payload lands (never fabricating an empty
/// modal), exactly as the old tag did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub enum ModeSnapshot {
    /// Credentials form (`Mode::KeyInput`). STUB — payload projected in a later stage.
    KeyInput,
    /// `--resume` disk session picker (`Mode::SessionPicker`). STUB.
    SessionPicker,
    /// Two-pane session hub (`Mode::SessionHub`). STUB.
    SessionHub,
    /// Normal chat view (`Mode::Chat`). Payload-free: everything Chat renders lives
    /// in the session + global projections already, so the variant carries nothing.
    Chat,
    /// Startup warming splash (`Mode::Loading`). STUB.
    Loading,
    /// `/settings` dashboard (`Mode::Settings`). STUB.
    Settings,
    /// `/agents` manager (`Mode::Agents`). STUB.
    Agents,
    /// `/effort` reasoning-effort picker (`Mode::Effort`). STUB.
    Effort,
    /// `/usage` dashboard (`Mode::Usage`). STUB.
    Usage,
    /// Message-rewind picker (`Mode::MessageRewind`). STUB.
    MessageRewind,
    /// `/quit` confirm overlay (`Mode::QuitConfirm`). Carries the busy/total session
    /// counts the overlay's warning text reads (the inner `QuitConfirmState`), so the
    /// client renders the EXACT header the daemon would — no longer re-derived from
    /// the shadow sessions on the client side.
    QuitConfirm {
        /// Count of sessions with work in flight at open time (display only).
        working: usize,
        /// Total number of sessions at open time (display only).
        total: usize,
    },
}

// ─── incremental deltas ──────────────────────────────────────────────────────

/// An incremental state update the daemon emits between full snapshots. This is
/// a first-cut vocabulary (critique-driven: every session-scoped delta carries a
/// stable `session_id`, never an index); the actual delta-EMISSION logic is a
/// later stage. Defined now so the wire types exist and round-trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub enum StateDelta {
    /// Append streamed assistant content to a session's streaming buffer.
    TokenAppended { session_id: String, text: String },
    /// Append streamed reasoning to a session's reasoning buffer.
    ReasoningAppended { session_id: String, text: String },
    /// A status-line change. `session_id` is `Some` for a session-scoped status,
    /// `None` for the global status line.
    StatusChanged {
        session_id: Option<String>,
        text: String,
    },
    /// The shared composer text and/or caret moved. Carries the WHOLE input string
    /// (not a suffix): the composer is small, edited in the middle (insert/delete/
    /// arrow), and not append-only like the streaming buffers, so a full replace is
    /// the only always-correct fold. Without this delta a controller client's typed
    /// characters stay invisible until the next structural change forces a full
    /// snapshot (the composer would appear permanently blank as the user types).
    InputChanged { text: String, cursor: usize },
    /// The transcript scroll offset and/or its bottom-pinning follow flag moved.
    /// Both are GLOBAL view state ([`GlobalSnapshot::scroll`] / `follow`), not
    /// session-scoped. Carried as a dedicated delta so a daemon-side scroll (e.g.
    /// PageUp forwarded by a client, or new content nudging follow) propagates
    /// incrementally — previously only a full snapshot moved it, so a controller
    /// client's scroll position lagged until the next structural change.
    ScrollChanged { scroll: u16, follow: bool },
    /// A session's working / finished-unseen flags changed.
    SessionStatusChanged {
        session_id: String,
        working: bool,
        finished_unseen: bool,
    },
    /// The foreground session changed (by stable id).
    ForegroundChanged { session_id: String },
    /// A new session was added; carries its full initial projection.
    SessionAdded(SessionSnapshot),
    /// Show a toast. `kind` is "error" / "info".
    Toast { kind: String, text: String },
}

// ─── StreamEvent wire mirror ─────────────────────────────────────────────────

/// A serde mirror of the cross-session-relevant [`StreamEvent`] variants.
///
/// `StreamEvent` is `Clone` but not serde-cleanly transferable (the endpoint /
/// catalogue variants carry `ModelEndpoint`, which is `Deserialize`-only). Those
/// variants are CLIENT-LOCAL UI concerns (the model modal + catalogue fetch live
/// on `AppStateRest` directly, not per-session) and never cross the daemon
/// boundary, so they are deliberately omitted: [`From<&StreamEvent>`] yields
/// `None` for them. The eight variants that actually drive a session's turn are
/// mirrored faithfully.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub enum StreamEventWire {
    Token(String),
    Reasoning(String),
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: u64,
        cost: f64,
    },
    ToolCalls(Vec<crate::dto::chat::ToolCall>),
    Done,
    Error(String),
    Compacted {
        summary: String,
        kept_tail: Vec<ChatMessage>,
    },
    HarnessVerdict {
        allow: bool,
        reason: String,
    },
}

impl StreamEventWire {
    /// Project a live [`StreamEvent`] into its wire mirror.
    ///
    /// Returns `None` for the client-local UI events (endpoint list + catalogue)
    /// that never cross the daemon boundary — see the type docs.
    #[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
    pub fn from_event(ev: &StreamEvent) -> Option<Self> {
        Some(match ev {
            StreamEvent::Token(s) => StreamEventWire::Token(s.clone()),
            StreamEvent::Reasoning(s) => StreamEventWire::Reasoning(s.clone()),
            StreamEvent::Usage {
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                cost,
            } => StreamEventWire::Usage {
                prompt_tokens: *prompt_tokens,
                completion_tokens: *completion_tokens,
                cached_tokens: *cached_tokens,
                cost: *cost,
            },
            StreamEvent::ToolCalls(calls) => StreamEventWire::ToolCalls(calls.clone()),
            StreamEvent::Done => StreamEventWire::Done,
            StreamEvent::Error(s) => StreamEventWire::Error(s.clone()),
            StreamEvent::Compacted { summary, kept_tail } => StreamEventWire::Compacted {
                summary: summary.clone(),
                kept_tail: kept_tail.clone(),
            },
            StreamEvent::HarnessVerdict { allow, reason } => StreamEventWire::HarnessVerdict {
                allow: *allow,
                reason: reason.clone(),
            },
            // Client-local UI events — never sent over the wire.
            StreamEvent::EndpointsLoaded { .. } | StreamEvent::EndpointsError { .. } => return None,
        })
    }
}

impl From<&StreamEvent> for Option<StreamEventWire> {
    fn from(ev: &StreamEvent) -> Self {
        StreamEventWire::from_event(ev)
    }
}
