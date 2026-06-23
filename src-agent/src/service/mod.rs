pub mod openrouter;

use crate::dto::chat::ChatMessage;

/// Monotonic id distinguishing task generations.
pub type Generation = u64;

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Token(String),
    Done,
    Error(String),
    /// summary + the kept_tail snapshot captured at /compact dispatch time.
    Compacted {
        summary: String,
        kept_tail: Vec<ChatMessage>,
    },
}

/// Every event carried on the channel is tagged with the generation of the
/// task that produced it. The runtime discards events whose generation does
/// not match state.generation.
#[derive(Debug, Clone)]
pub struct TaggedEvent {
    pub generation: Generation,
    pub event: StreamEvent,
}
