//! Service layer: the async side of the app.
//!
//! Owns the network client (`openrouter`) and defines [`StreamEvent`], the only
//! message type that crosses the async->UI boundary. A spawned request task
//! sends `StreamEvent`s down a per-request channel; the runtime drains the
//! matching receiver each tick and folds the events into `AppState`.
//!
//! Lifecycle of one request: runtime opens a fresh channel, stashes the
//! receiver in `state.rest.active_rx`, spawns a task with the sender. Dropping
//! the receiver (on interrupt / `/new` / a new request) silently discards any
//! events the old task still emits — no generation tagging required.

pub mod openrouter;

use crate::dto::chat::ChatMessage;

/// A single event on the async->UI channel. One channel exists per in-flight
/// request; the runtime folds each event into `AppState`.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of assistant text to append to the streaming buffer.
    Token(String),
    /// A chunk of the model's reasoning/thinking text (the `delta.reasoning`
    /// channel, separate from `content`). Appended to a parallel reasoning buffer
    /// and committed onto the assistant message as a display-only block — never
    /// sent back to the API or persisted to disk.
    Reasoning(String),
    /// Token/cost accounting for the in-flight generation. Arrives on the final
    /// streaming chunk, just before [`StreamEvent::Done`]; stashed and committed
    /// with the assistant message. `cached_tokens` is the share of `prompt_tokens`
    /// served from the prompt cache (a cache hit at the discounted rate); 0 on a
    /// cold prefix or a provider that doesn't report cache stats.
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: u64,
        cost: f64,
    },
    /// The model requested one or more tool calls. Emitted just before
    /// [`StreamEvent::Done`] (after any `Token`/`Usage` events) so the runtime
    /// can stash them and run the tools once the stream finalises.
    ToolCalls(Vec<crate::dto::chat::ToolCall>),
    /// The stream finished cleanly; commit the buffered assistant message.
    Done,
    /// The stream failed; `String` is the error to surface in the status line.
    Error(String),
    /// `/compact` result: the `summary` plus the `kept_tail` snapshot captured
    /// at dispatch time (so compaction is applied against a stable tail).
    Compacted {
        summary: String,
        kept_tail: Vec<ChatMessage>,
    },
    /// Advisory prompt-classifier (PC) verdict for the current turn. Produced by
    /// a background task spawned at turn start and delivered on the harness
    /// channel. `allow = false` surfaces a toast; the turn is NEVER blocked by it
    /// (the stream already proceeded). `allow = true` is silent.
    HarnessVerdict { allow: bool, reason: String },
    /// A model's provider-endpoint list finished loading. Delivered on the
    /// dedicated `endpoints_rx` channel (drained in `run_loop`, independent of
    /// streaming) by a background task spawned when the model modal selects /
    /// opens an OpenRouter model. `model_id` is the model the endpoints belong to
    /// (used as a stale-guard against the modal's `endpoints_for`).
    EndpointsLoaded {
        model_id: String,
        endpoints: Vec<crate::dto::openrouter::ModelEndpoint>,
    },
    /// The provider-endpoint fetch for `model_id` failed; `error` is the cause.
    /// Folded into the modal as an empty endpoint list (rendered as "no
    /// providers found"), so a failed fetch resolves the loading state instead
    /// of spinning forever. `error` is carried for diagnostics (Debug) but the
    /// drain renders the failure as the empty list rather than surfacing the
    /// text, so it is not otherwise read.
    EndpointsError {
        model_id: String,
        #[allow(dead_code)]
        error: String,
    },
}

/// A single event on the startup-warming channel. The non-blocking `warm_session`
/// refactor spawns the catalogue + awareness fetches as background tasks and
/// shows an animated [`crate::app::mode::Mode::Loading`] splash; each task sends
/// one of these when it resolves, and the event loop folds it into both
/// `AppStateRest` (the cache / summary — always) and the live `LoadingState`
/// (the step marker — only while still Loading). Lives on its own channel
/// (`AppStateRest::warm_rx`), independent of streaming, mirroring `endpoints_rx`.
// The shared `Warm` prefix is intentional: it reads clearly at the call/drain
// site (`WarmEvent::WarmCatalogue`) and matches the dedicated-channel naming.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub enum WarmEvent {
    /// The model catalogue fetch succeeded; carries the fetched models. Folded
    /// into `models_cache` so the short-send threshold gate has a context window.
    WarmCatalogue(Vec<crate::dto::openrouter::ModelInfo>),
    /// The model catalogue fetch failed (network / non-routable Main). The cache
    /// stays `None` (treated as "window unknown") and the step marker shows failed.
    WarmCatalogueFailed,
    /// The project-awareness summary resolved: `Some(text)` on success, `None`
    /// when there were no docs / the call failed. Folded into `awareness_summary`.
    WarmAwareness(Option<String>),
}
