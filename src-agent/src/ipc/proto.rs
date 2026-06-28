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
    ///
    /// `cwd` is the CLIENT's launch working directory (its own `pwd`, NOT the
    /// daemon's). When present on the CONTROLLER's attach it drives pwd-aware
    /// session selection (stage 3): the daemon foregrounds a LIVE session for that
    /// pwd if one exists, else loads the most-recent ON-DISK session for that pwd,
    /// else CREATES a fresh session targeting that pwd. This is what makes relaunching
    /// `koma` from a NEW directory land on a session for THAT directory instead of the
    /// daemon's unrelated last session. `None` (e.g. an observer attach, or a client
    /// that can't resolve its cwd) keeps the daemon's current foreground untouched.
    Attach {
        foreground_id: Option<String>,
        cwd: Option<String>,
    },
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
    /// Run a `!`-prefixed shell command directly in the FOREGROUND session's CURRENT
    /// working directory, with NO model round-trip. `cmd` is the command WITHOUT the
    /// leading `!`. The daemon runs it (captured stdout+stderr, same output cap /
    /// ANSI strip / timeout as the `bash` tool), appends a distinct shell entry to
    /// that session's conversation, and the result is projected to the client via the
    /// normal snapshot/delta. NOT gated by the workspace allow-list (the user is
    /// trusted). In the daemon-default world keys are forwarded, so the leading-`!`
    /// detection normally happens daemon-side (the composer Enter handler emits the
    /// shell action); this variant is the explicit-request equivalent for a client
    /// that interprets the composer itself.
    Shell { cmd: String },
    /// Forward a single key event to the daemon (routed to whatever modal/handler
    /// the foreground session's mode dictates), as a serde-safe [`KeyWire`].
    SendKey(KeyWire),
    /// Forward a bracketed-PASTE event verbatim (the whole pasted text, which may
    /// be a file PATH or multi-line content). The daemon routes it through the SAME
    /// [`crate::controller::input::handle_paste`] the local TUI uses, so a pasted
    /// image-file path becomes an `[Image #N]` attachment (copied into the session's
    /// `images/` dir, daemon-side) and ordinary text lands in the active field with
    /// the same line-ending normalisation. Forwarded as a distinct request (NOT
    /// char-by-char `SendKey`s) precisely so the daemon's `handle_paste` runs — the
    /// per-char path can't detect an image-path paste and mangles CRLF newlines.
    Paste { text: String },
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
    /// Client publishes the on-screen editor wrap width so the daemon's editor
    /// navigation wraps by the same visual rows.
    EditorWrapW(usize),
    /// Sent by the client on startup when launched with `--resume` / `koma agents`:
    /// asks the daemon to open the session hub (same as the /resume slash command).
    OpenSessionHub,
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
    /// Build-skew handshake (task #142): the VERY FIRST frame the daemon sends a
    /// newly-attached client, carrying the daemon's [`crate::model::store::build_fingerprint`]
    /// AS COMPUTED AT DAEMON STARTUP (not re-read at attach time — the on-disk file may
    /// already be the new build). The client computes its OWN fingerprint fresh on
    /// connect and compares: equal → proceed; different → the binary changed since the
    /// daemon started (a rebuild), so the client restarts the stale daemon and reconnects
    /// rather than silently rendering its stale frames. Emitted right BEFORE the initial
    /// `Snapshot` in the attach handler, so it always arrives first (lowest seq) and the
    /// client can verify it before painting anything. A daemon predating this variant
    /// simply never sends it; the client then can't confirm a skew and proceeds (it never
    /// restarts on a mere absence — see the client handshake).
    Hello { version: String },
    /// A full state projection — sent on attach and on resync. The client rebuilds
    /// its entire shadow from this. Boxed because a `StateSnapshot` is by far the
    /// largest event payload (it carries every session + the full mode projection,
    /// including the settings dashboard draft), so an unboxed variant would bloat
    /// every `DaemonEvent` — including the high-frequency `Delta`/`Ack` frames — to
    /// the snapshot's size.
    Snapshot(Box<StateSnapshot>),
    /// An incremental update folded onto the existing shadow.
    Delta(StateDelta),
    /// Acknowledgement of a request that produces no other reply.
    Ack,
    /// A request failed; the `String` is a human-readable reason.
    Error(String),
    /// One-shot: the CONTROLLER asked for the `/select` transcript dump (the daemon's
    /// `/select` set `select_pending`). The dump MUST run on the CLIENT — it leaves the
    /// alt-screen and writes to the controlling TTY, which the headless daemon does NOT
    /// own. Payload-free: the client already holds the full conversation in its shadow
    /// state, so it renders the dump from `rest.fg().session` without any data riding the
    /// wire. Sent ONLY to the controller (the controlling client).
    EnterSelect,
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
    /// The session's EFFECTIVE working directory as a display string (the live
    /// `cd` override when set, else the configured workdir — see
    /// [`crate::app::state::SessionRuntime::effective_cwd`]). Carried so a client
    /// reflects where the session currently is (e.g. a future cwd indicator) and so
    /// its reconstructed `SessionRuntime` reports the same `effective_cwd()`. Empty
    /// only when there is no session. The configured allow-list roots are NOT
    /// projected here (cd moves only the cwd).
    pub cwd: String,
    /// Committed conversation messages (already serde via `ChatMessage`).
    pub messages: Vec<ChatMessage>,
    /// Per-message display-only reasoning, index-aligned with `messages`.
    ///
    /// `ChatMessage::reasoning` is `#[serde(skip)]` (it must NEVER ride the API
    /// request body nor `messages.json`), so the committed thinking block would
    /// vanish over the wire — the live `stream_reasoning` buffer clears at turn
    /// finalize and the client has nothing to fall back to, so the thinking block
    /// disappears the instant the turn completes. This parallel side-channel
    /// carries that reasoning to the client WITHOUT un-skipping the field: the
    /// daemon copies each message's `reasoning` here at projection time, and the
    /// client folds it back onto its reconstructed messages after deserialise.
    /// Each entry is `None` for a message that had no reasoning (the common case),
    /// keeping the wire cost negligible. Empty (or shorter than `messages`) is
    /// tolerated by the client as "no committed reasoning".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committed_reasoning: Vec<Option<String>>,
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
    /// The tool calls emitted by the in-flight stream that are pending execution /
    /// approval. Carried so the client's tool-approval overlay (rendered in Chat
    /// mode when `awaiting_approval`) shows the SAME pending call — name, args,
    /// payload preview — the local TUI does. `ToolCall` is already serde-clean.
    /// Empty when no calls are pending. Only the call at `tool_idx` is shown, but
    /// the whole round is carried so a future per-call preview has the full set.
    pub pending_tool_calls: Vec<crate::dto::chat::ToolCall>,
    /// Index of the call in `pending_tool_calls` the approval overlay is asking
    /// about (the `SessionRuntime::tool_idx`). The overlay reads
    /// `pending_tool_calls[tool_idx]`, so it must ride alongside the calls.
    pub tool_idx: usize,
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
    /// The daemon-side resolved Main model id for this session, computed via
    /// `resolve_role(config, settings, ModelRole::Main)` with fallback to
    /// `settings.model`. Projected here so the thin client can display the exact
    /// model name in the chat header without needing the full keyed config or
    /// model catalogue (both of which are stripped from the client's shadow).
    pub resolved_model_id: String,
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
    /// The GLOBAL UI theme as a wire token (`"dark"` / `"light"`), projected from
    /// [`crate::model::app_config::AppConfig::theme`]. `view::draw` frames EVERY
    /// screen with `theme::palette(&state.rest.config)` BEFORE dispatching to a mode
    /// renderer, so without this the thin client's `rest.config` stays at
    /// `AppConfig::default()` (Dark) and a Light-theme daemon would render every
    /// label/border/highlight in the wrong palette. (The Settings DRAFT `theme` is a
    /// separate value-preview field; the outer palette comes from `rest.config`, not
    /// the mode payload.)
    pub theme: String,
    /// The GLOBAL accent colour token, projected from
    /// [`crate::model::app_config::AppConfig::accent`] (an arbitrary palette key like
    /// `"green"`, carried verbatim). Drives every accent in `theme::palette`; without
    /// it a non-green daemon's selections/status bars render green on the client.
    pub accent: String,
    /// A PURE-DATA projection of the current [`crate::app::mode::Mode`] carrying
    /// each mode's render-relevant payload, so the thin client can reconstruct +
    /// draw every screen (not just Chat). See [`ModeSnapshot`].
    pub mode: ModeSnapshot,
    /// Active toast as `(kind, text)`, or `None`. `kind` is "error" / "info".
    pub toast: Option<(String, String)>,
    /// The on-demand model catalogue ([`crate::app::state::AppStateRest::models_cache`]):
    /// `None` = never fetched, `Some(vec)` = fetched for `models_cache_endpoint`
    /// (an empty vec means "fetched, none found" — terminal). The Settings model
    /// modal's omnisearch AND the KeyInput step-1 search render their result
    /// dropdowns from this; without it a remote client's omnisearch can never show
    /// catalogue rows (it would sit on `searching models…` forever). `ModelInfo`
    /// is serde-clean, so the cache crosses the wire verbatim.
    pub models_cache: Option<Vec<crate::dto::openrouter::ModelInfo>>,
    /// Which endpoint `models_cache` was fetched for
    /// ([`crate::app::state::AppStateRest::models_cache_endpoint`]). The omnisearch
    /// views only trust the cache when this matches the EDITED provider's endpoint;
    /// otherwise they show `searching models…`. Projected alongside the cache so the
    /// client makes the SAME match decision the daemon would.
    pub models_cache_endpoint: Option<String>,
    /// The full-screen sub-agent VIEWER state, mirroring
    /// [`crate::app::state::AppStateRest`]'s `agent_viewer` (`Some(idx)` =
    /// the viewer is open on the foreground session's `subagents[idx]`, short-
    /// circuiting the chat draw), `agent_viewer_scroll`, and `agent_viewer_follow`.
    /// The viewer is rendered FROM Chat mode (not its own `Mode`), so it rides here on
    /// the global projection rather than in [`ModeSnapshot`]; the client reconstructs
    /// the same `rest.agent_viewer*` so the unmodified chat renderer takes the
    /// full-screen viewer branch and draws the reconstructed sub-agent's transcript.
    pub agent_viewer: Option<usize>,
    pub agent_viewer_scroll: u16,
    pub agent_viewer_follow: bool,
    /// Sub-agents `$` panel open-state + selection
    /// ([`crate::app::state::AppStateRest`]'s `subagents_open` / `subagent_sel`),
    /// projected so the client renders the same overlay (it floats over the chat in
    /// the input controller's modal, reading the foreground session's reconstructed
    /// `subagents` + `pending_subagents`).
    pub subagents_open: bool,
    pub subagent_sel: usize,
    /// The `@` file-picker + `/` command-picker highlighted-row index
    /// ([`crate::app::state::AppStateRest::palette_sel`]). Like `subagent_sel` this
    /// is rendered FROM Chat mode and rides no incremental delta, so it is projected
    /// here so Up/Down on either picker moves the highlight on the thin client (its
    /// shadow `palette_sel` would otherwise stay stuck at 0).
    pub palette_sel: usize,
    /// The composer's staged-but-unsubmitted image attachments
    /// ([`crate::app::state::AppStateRest::pending_attachments`]). Each was ingested
    /// DAEMON-SIDE (its bytes already under `<session>/images/`) by a path-paste,
    /// clipboard-image paste, or `@`-picker pick, and matches an `[Image #N]` marker
    /// in `input`. The marker text rides in `input` already, but projecting the
    /// records keeps the client's shadow faithful (so its reconstructed composer
    /// state mirrors the daemon's exactly) and lets a future composer-side card read
    /// them. `Attachment` is serde-clean. Empty when nothing is staged.
    pub pending_attachments: Vec<crate::dto::chat::Attachment>,
    /// The `@`-file palette matches, precomputed DAEMON-SIDE, or `None` when the
    /// composer's last token is not an `@partial`. The palette is normally rendered
    /// by `view::chat` calling `dir_cache.search(partial, …)`, but a thin client's
    /// reconstructed session has an EMPTY `dir_cache` (the workspace index never
    /// crosses the wire), so its `@` dropdown would always be blank. The daemon runs
    /// the SAME `search` against ITS index and ships the result here; the client
    /// seeds `rest.file_palette` from it and the unmodified file-palette view renders
    /// the projected list (mirrors how `usage_data` / `models_cache` feed a DB-less /
    /// fetch-less client). `Some(vec![])` = an `@partial` with no matches.
    pub file_palette: Option<Vec<String>>,
    /// "auto" | "normal" — the current agent mode, matches AgentMode::label().
    pub agent_mode: String,
}

// ─── mode payload projections (stage 2: core interactive modes) ──────────────

/// A serde-safe projection of the first-run setup wizard ([`crate::app::mode::KeyInputForm`]).
///
/// Mirrors every field the wizard view (`view::key_input`) reads: the step/field
/// cursor, the three text drafts, and the step-1 omnisearch `query`/`result_sel`.
/// The `first_run`/`from_picker` flags only steer Esc behaviour (handled on the
/// daemon via forwarded keys), but are carried so the reconstructed form is an
/// exact copy rather than a render-only shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct KeyInputSnapshot {
    pub step: usize,
    pub field: usize,
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    pub query: String,
    pub result_sel: usize,
    pub first_run: bool,
    pub from_picker: bool,
}

/// A serde-safe projection of the startup warming splash ([`crate::app::mode::LoadingState`]).
///
/// The live `started` is an [`std::time::Instant`] (not serialisable); it is
/// projected as `elapsed_ms` so the client re-anchors its OWN `Instant` (`now -
/// elapsed`) and the footer's elapsed-seconds counter continues from the daemon's
/// phase instead of restarting at 0 each snapshot. `frame` drives the spinner; the
/// two `WarmStatus` steps are projected as [`WarmStatusWire`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct LoadingSnapshot {
    /// Milliseconds since the splash opened (the live `Instant`'s elapsed).
    pub elapsed_ms: u64,
    /// Spinner frame counter.
    pub frame: u64,
    /// The "indexing workspace" step status.
    pub workspace: WarmStatusWire,
    /// The "reading project docs" awareness step status.
    pub awareness: WarmStatusWire,
}

/// A serde-safe mirror of [`crate::app::mode::WarmStatus`] (which is not serde).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub enum WarmStatusWire {
    Pending,
    Running,
    /// Carries the short detail string the renderer shows dim next to the marker.
    Done(String),
    Skipped,
    Failed,
}

/// A serde-safe projection of one COOKING row in the session hub
/// ([`crate::app::mode::CookingEntry`]). The live `idx` is a daemon-side
/// `sessions` index used only on Enter (forwarded as a key), so it is NOT rendered
/// and not projected; the row's display fields are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct CookingEntrySnapshot {
    pub name: String,
    pub working: bool,
    pub is_foreground: bool,
}

/// A serde-safe projection of one HISTORY row in the session hub
/// ([`crate::app::mode::HistoryEntry`]). The live `path` is the daemon-side load
/// target (used on Enter, forwarded as a key) and is not rendered, so only the
/// display fields cross: the name and the last-active time as seconds-since-epoch
/// (the live `SystemTime` is rebuilt as `UNIX_EPOCH + secs` on the client, since
/// the view formats it as a relative age).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct HistoryEntrySnapshot {
    pub name: String,
    /// Last-active time as whole seconds since the Unix epoch (`0` if the live
    /// time predates the epoch / clock skew — the view degrades that to `"?"`).
    pub last_active_secs: u64,
}

/// A serde-safe projection of the two-pane session hub ([`crate::app::mode::SessionHub`]).
/// Carries both pane lists + the focus + each pane's cursor — everything
/// `view::session_hub` reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct SessionHubSnapshot {
    pub cooking: Vec<CookingEntrySnapshot>,
    pub history: Vec<HistoryEntrySnapshot>,
    /// `true` = the COOKING pane has focus; `false` = HISTORY.
    pub focus_cooking: bool,
    pub cooking_selected: usize,
    pub history_selected: usize,
}

/// A serde-safe mirror of [`crate::dto::openrouter::ModelEndpoint`], which is
/// `Deserialize`-only (so it cannot ride directly on a serde wire DTO). Carries
/// only the fields the model modal's Route list renders (provider name, pricing,
/// uptime); the rest of `ModelEndpoint` is reconstructed at `Default` on the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct ModelEndpointWire {
    pub name: Option<String>,
    pub provider_name: Option<String>,
    /// Per-token USD price strings `(prompt, completion)` (as OpenRouter reports
    /// them); the view formats them per-million. `None` when the endpoint omits
    /// pricing.
    pub price_prompt: Option<String>,
    pub price_completion: Option<String>,
    pub uptime_last_30m: Option<f64>,
}

/// A serde-safe projection of one API-provider draft ([`crate::app::mode::settings::ProviderDraft`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct ProviderDraftSnapshot {
    pub uuid: String,
    pub name: String,
    pub endpoint: String,
    /// `api_type` as a wire token: `"openai"` / `"anthropic"`.
    pub api_type: String,
    pub api_key: String,
}

/// A serde-safe projection of one model draft ([`crate::app::mode::settings::ModelDraft`]).
/// Roles are projected as wire tokens (`"main"`/`"awareness"`/`"safeguard"`/`"compactor"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct ModelDraftSnapshot {
    pub uuid: String,
    pub name: String,
    pub model_id: String,
    pub provider_idx: usize,
    pub roles: Vec<String>,
    pub route: Option<String>,
    pub session_only: bool,
}

/// A serde-safe projection of the add-provider modal ([`crate::app::mode::settings::ProviderModal`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct ProviderModalSnapshot {
    pub name: String,
    pub endpoint: String,
    pub api_type: String,
    pub api_key: String,
    pub field: usize,
}

/// A serde-safe projection of the role multi-select picker overlay
/// ([`crate::app::mode::settings::RolePickerState`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct RolePickerSnapshot {
    /// Parallel to `ModelRole::ALL` — the 4 role checkboxes.
    pub checked: Vec<bool>,
    pub cursor: usize,
}

/// A serde-safe projection of the add/edit-model modal ([`crate::app::mode::settings::ModelModal`]).
/// Endpoints ride as [`ModelEndpointWire`] (the live type is Deserialize-only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct ModelModalSnapshot {
    pub editing_idx: Option<usize>,
    pub uuid: String,
    pub name: String,
    pub provider_idx: usize,
    pub model_id: String,
    pub field: usize,
    pub roles: Vec<String>,
    pub role_picker: Option<RolePickerSnapshot>,
    pub query: String,
    pub result_sel: usize,
    pub route: Option<String>,
    pub route_sel: usize,
    /// `None` until a fetch resolves; `Some(vec)` once loaded (empty = none found).
    pub endpoints: Option<Vec<ModelEndpointWire>>,
    pub endpoints_loading: bool,
    pub endpoints_for: Option<String>,
}

/// A serde-safe projection of the filesystem directory picker overlay
/// ([`crate::app::mode::settings::PathPicker`]). The matches are precomputed on the
/// daemon side (a `read_dir` walk), so the client renders the SAME list without
/// touching the daemon's filesystem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct PathPickerSnapshot {
    pub query: String,
    pub matches: Vec<String>,
    pub sel: usize,
    /// `None` = Add mode; `Some(i)` = Replace the entry at index `i`.
    pub replace_idx: Option<usize>,
}

/// A serde-safe projection of the `/settings` dashboard ([`crate::app::mode::SettingsState`]).
///
/// This is the BIGGEST mode projection: the settings view reads dozens of draft
/// fields plus the providers/models lists, the open sub-modals, the path-list and
/// FS-picker state, and several pure helper methods (`is_providers_category`,
/// `model_modal_fields`, `mm_provider_omnisearchable`, …) that all recompute from
/// these same fields. So the client reconstructs a REAL `SettingsState` from this
/// projection and renders it through the unmodified `view::settings::draw` — every
/// helper then yields the same answer the daemon's would. `theme`/`internet_mode`
/// ride as wire tokens; `cwd` is carried only so a reconstructed FS picker can
/// recompute correctly (it is otherwise not rendered).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct SettingsSnapshot {
    pub cat: usize,
    pub field: usize,
    pub in_detail: bool,
    pub editing: bool,
    pub api_key: String,
    pub model: String,
    pub provider: String,
    pub name: String,
    /// Theme as a wire token: `"dark"` / `"light"`.
    pub theme: String,
    pub accent: String,
    pub workdir: Vec<String>,
    pub awareness_enabled: bool,
    pub awareness_inherit: bool,
    pub awareness_model: String,
    pub awareness_provider: String,
    pub classifier_enabled: bool,
    pub classifier_model: String,
    pub classifier_provider: String,
    pub allowed_folders: Vec<String>,
    pub short_send_enabled: bool,
    pub sliding_cache: bool,
    /// Internet mode as a wire token: `"simple"` / `"full"`.
    pub internet_mode: String,
    /// The session's effective working directory (FS-picker base path).
    pub cwd: String,
    pub list_editing: bool,
    pub list_sel: usize,
    pub picker: Option<PathPickerSnapshot>,
    pub providers: Vec<ProviderDraftSnapshot>,
    pub prov_sel: usize,
    pub prov_delete_armed: bool,
    pub prov_modal: Option<ProviderModalSnapshot>,
    pub models: Vec<ModelDraftSnapshot>,
    pub model_sel: usize,
    pub model_delete_armed: bool,
    pub model_modal: Option<ModelModalSnapshot>,
}

// ─── mode payload projections (stage 3: secondary full-screen views) ─────────

/// A serde-safe projection of the `/usage` dashboard ([`crate::app::mode::UsageNavState`]
/// + its pre-fetched ledger data).
///
/// The dashboard's numbers come from the global sqlite usage ledger, which the
/// thin client has NO access to. So the daemon pre-computes the exact query
/// results the renderer reads ([`crate::model::usage::UsageData`]) and ships them
/// here alongside the nav state (active view / range / metric as wire tokens). The
/// client rebuilds a `UsageNavState` AND seeds `rest.usage_data` from `data`, so
/// the unmodified `view::usage::draw` renders the same dashboard without a DB. The
/// `data` is already serde-clean (its query rows are plain scalars/strings).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct UsageSnapshot {
    /// Active view: `"global"` / `"session"`.
    pub view: String,
    /// Active range: `"today"` / `"week"` / `"year"`.
    pub range: String,
    /// Heatmap/bar metric: `"cost"` / `"tokens"`.
    pub metric: String,
    /// The pre-fetched ledger projection for the active view+range (so the client
    /// renders the dashboard with zero DB access).
    pub data: crate::model::usage::UsageData,
}

/// A serde-safe projection of one message-rewind entry ([`crate::app::mode::RewindState`]'s
/// `RewindEntry`). `vec_index` is the daemon-side conversation cut position used
/// only on Enter (forwarded as a key), but is carried so a reconstructed entry is an
/// exact copy; `content` is the message text the list row previews.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct RewindEntrySnapshot {
    pub vec_index: usize,
    pub content: String,
}

/// A serde-safe projection of the message-rewind picker ([`crate::app::mode::RewindState`]).
/// Carries the newest-first entry list + the cursor — everything `view::message_rewind`
/// reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct RewindSnapshot {
    pub entries: Vec<RewindEntrySnapshot>,
    pub selected: usize,
}

/// A serde-safe projection of the `/effort` reasoning-effort picker
/// ([`crate::app::mode::EffortPickerState`]). All plain data the overlay reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct EffortSnapshot {
    pub options: Vec<String>,
    pub selected: usize,
    pub note: String,
}

/// A serde-safe projection of one `--resume` session-picker row
/// ([`crate::model::store::SessionMeta`]).
///
/// The live `SessionMeta` carries a `PathBuf` (the daemon-side load target, used on
/// Enter — forwarded as a key) and a `SystemTime`, neither serde-clean; the picker
/// view renders only the name + id + message count + lock marker + a relative age, so
/// the path is dropped and the modified time crosses as seconds-since-epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct SessionMetaSnapshot {
    pub id: String,
    pub name: String,
    /// Last-modified time as whole seconds since the Unix epoch (`0` if the live
    /// time predates the epoch / clock skew).
    pub modified_secs: u64,
    pub message_count: usize,
    pub locked: bool,
}

/// A serde-safe projection of the `--resume` session picker ([`crate::app::mode::PickerState`]).
/// Carries the full unfiltered metadata list + the live query + the filtered index
/// subset + the cursor — everything `view::session_picker` reads (it re-runs no
/// filtering itself; the daemon's `filtered_idx` is carried so the SAME rows show).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct PickerSnapshot {
    pub query: String,
    pub all: Vec<SessionMetaSnapshot>,
    pub filtered_idx: Vec<usize>,
    pub selected: usize,
}

/// A serde-safe projection of a registered model entry, KEYLESS — just the fields
/// the `/agents` model row + picker resolve a chosen `model_uuid` into a label
/// (`name @ provider`). Projected (instead of the whole `AppConfig`) so the client
/// resolves the same labels WITHOUT the daemon's API keys ever crossing the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct CatalogueModelSnapshot {
    pub uuid: String,
    pub name: String,
    pub model_id: String,
    pub provider_uuid: String,
}

/// A serde-safe projection of an API-provider connection, KEYLESS — only the
/// `uuid` + display name (or endpoint fallback) the `/agents` model label needs to
/// render `... @ <provider>`. The api key is deliberately NOT projected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct CatalogueProviderSnapshot {
    pub uuid: String,
    pub name: String,
    pub endpoint: String,
}

/// A serde-safe projection of the full-screen nano text editor
/// ([`crate::app::mode::editor::TextEditorState`]) — the `/agents` field editor.
/// The live `wrap_w` is a render-published `Cell` (re-seeded on the client), so only
/// the buffer + cursor + scroll cross.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct TextEditorSnapshot {
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize,
    pub scroll: usize,
}

/// A serde-safe projection of the `/agents` tool multi-select picker
/// ([`crate::app::mode::agents::ToolPickerState`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct ToolPickerSnapshot {
    pub options: Vec<String>,
    pub checked: Vec<bool>,
    pub cursor: usize,
    pub filter: String,
}

/// A serde-safe projection of the `/agents` single-select model picker
/// ([`crate::app::mode::agents::ModelPickerState`]). Each option is its
/// `(model_uuid_or_none, label)` pair, carried verbatim (row 0 is the
/// `(None, "(inherit main)")` sentinel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct AgentModelPickerSnapshot {
    pub options: Vec<(Option<String>, String)>,
    pub cursor: usize,
}

/// Lightweight agent entry for IPC display — carries the fields that AgentDef
/// marks #[serde(skip)] and would otherwise be lost over the socket.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AgentEntry {
    pub name: String,
    pub description: String,
    pub conditions: String,
    /// "session" | "project" | "global" | "builtin"
    pub source: String,
    pub model_uuid: Option<String>,
    pub model: Option<String>,
    pub tools: Vec<String>,
    pub prompt: String,
}

/// A serde-safe projection of the `/agents` dashboard ([`crate::app::mode::AgentsState`]).
///
/// The agent LIST rides as `Vec<AgentEntry>` (carrying the display fields that
/// `AgentDef` marks `#[serde(skip)]` and would otherwise vanish over the socket),
/// the working drafts + sub-mode + field cursor as plain data + wire tokens, and the
/// three overlays (tool picker / model picker / full-screen field editor) as their own
/// projections. A KEYLESS model+provider catalogue rides alongside so the client
/// resolves a chosen `model_uuid` to its `name @ provider` label exactly as the
/// daemon would, WITHOUT any API key crossing the wire (the client reconstructs a
/// minimal `AppConfig` from it just for that label lookup).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub struct AgentsSnapshot {
    pub agents: Vec<AgentEntry>,
    pub list_sel: usize,
    pub in_detail: bool,
    /// Sub-mode: `"browse"` / `"edit"` / `"create"` / `"delete_confirm"`.
    pub mode: String,
    /// Highlighted field: `"name"` / `"description"` / `"conditions"` / `"model"` /
    /// `"tools"` / `"prompt"`.
    pub field: String,
    pub editing: bool,
    /// Create scope: `"session"` / `"global"`.
    pub create_scope: String,
    pub draft_name: String,
    pub draft_description: String,
    pub draft_conditions: String,
    pub draft_model_uuid: Option<String>,
    pub draft_model_legacy: Option<String>,
    pub draft_tools: String,
    pub draft_body: String,
    pub tool_picker: Option<ToolPickerSnapshot>,
    pub model_picker: Option<AgentModelPickerSnapshot>,
    /// The full-screen field editor: `(field_token, editor)` when open.
    pub editor: Option<(String, TextEditorSnapshot)>,
    pub editor_clear_confirm: bool,
    /// Keyless registered-model catalogue (for the model-label lookup).
    pub catalogue_models: Vec<CatalogueModelSnapshot>,
    /// Keyless provider catalogue (for the `@ <provider>` part of the label).
    pub catalogue_providers: Vec<CatalogueProviderSnapshot>,
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
/// Stage 1 filled `Chat` (payload-free) and `QuitConfirm`. Stage 2 filled the CORE
/// interactive modes — `KeyInput`, `SessionHub`, `Loading`, `Settings`. Stage 3
/// fills the SECONDARY full-screen views — `Usage`, `MessageRewind`, plus the last
/// remaining stubs `Effort`, `SessionPicker`, `Agents` — so EVERY variant now
/// carries its render-relevant payload and NOTHING falls back to a blank Chat render.
/// (The full-screen sub-agent VIEWER + `$` panel are rendered FROM Chat mode, so
/// their state rides on [`GlobalSnapshot`], not a variant here.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(dead_code)] // wired in daemon stage 2+ (no callers in stage 1)
pub enum ModeSnapshot {
    /// First-run credentials wizard (`Mode::KeyInput`). Carries the full form state.
    KeyInput(KeyInputSnapshot),
    /// `--resume` disk session picker (`Mode::SessionPicker`). Carries the metadata
    /// list + query + filtered subset + cursor.
    SessionPicker(PickerSnapshot),
    /// Two-pane session hub (`Mode::SessionHub`). Carries both panes + focus/cursors.
    SessionHub(SessionHubSnapshot),
    /// Normal chat view (`Mode::Chat`). Payload-free: everything Chat renders lives
    /// in the session + global projections already, so the variant carries nothing.
    Chat,
    /// Startup warming splash (`Mode::Loading`). Carries the step statuses + spinner.
    Loading(LoadingSnapshot),
    /// `/settings` dashboard (`Mode::Settings`). Carries the full draft + modal state.
    Settings(Box<SettingsSnapshot>),
    /// `/agents` manager (`Mode::Agents`). Carries the full editor state + keyless
    /// model/provider catalogue. Boxed — it is the largest payload (the agent list +
    /// drafts + overlays + catalogue), so an unboxed variant would bloat every frame.
    Agents(Box<AgentsSnapshot>),
    /// `/effort` reasoning-effort picker (`Mode::Effort`). Carries the options + cursor + note.
    Effort(EffortSnapshot),
    /// `/usage` dashboard (`Mode::Usage`). Carries the nav state + pre-fetched ledger
    /// data so the client renders it with no DB. Boxed — the embedded `UsageData`
    /// (heatmap buckets + per-model rows) is a large payload.
    Usage(Box<UsageSnapshot>),
    /// Message-rewind picker (`Mode::MessageRewind`). Carries the entry list + cursor.
    MessageRewind(RewindSnapshot),
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
    /// A new session was added; carries its full initial projection. Boxed so this
    /// variant doesn't bloat the whole `StateDelta` enum (a `SessionSnapshot` is the
    /// largest payload here — it carries the full message + tool-call + sub-agent
    /// projections), keeping the common small deltas (token/reasoning appends) cheap
    /// to move.
    SessionAdded(Box<SessionSnapshot>),
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
