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
}
