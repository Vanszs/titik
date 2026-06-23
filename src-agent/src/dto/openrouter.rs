//! Wire-format DTOs for the OpenRouter chat-completions API.
//!
//! Three distinct shapes live here, corresponding to the three interaction
//! modes this app uses:
//!
//! 1. **Request** (`ChatRequest`) — sent for every user turn, both streaming
//!    and non-streaming.
//! 2. **Non-streaming response** (`ChatResponse` / `Choice` / `ResponseMessage`)
//!    — used only by the `/compact` summarisation call, which needs the full
//!    reply in one shot before it can rewrite the conversation.
//! 3. **Streaming chunk** (`StreamChunk` / `StreamChoice` / `Delta`) — each
//!    SSE `data:` line from the model during a normal chat turn is parsed into
//!    one of these; `Delta::content` is appended to the in-progress assistant
//!    bubble.
//!
//! All types are serde-only; no business logic lives here.

use serde::{Deserialize, Serialize};
use crate::dto::chat::ChatMessage;

// ---------------------------------------------------------------------------
// Request (outbound)
// ---------------------------------------------------------------------------

/// OpenRouter provider-routing directive for strict provider pinning.
///
/// When set, the request is routed exclusively through the listed provider with
/// no fallback. Omitting this struct entirely (via `skip_serializing_if`) lets
/// OpenRouter use its default routing logic.
#[derive(Debug, Serialize)]
pub struct ProviderRouting {
    pub only: Vec<String>,
    pub allow_fallbacks: bool,
}

/// POST body for `POST /api/v1/chat/completions`.
///
/// `stream: true` triggers SSE delivery; `stream: false` waits for the full
/// response (used only by the compaction summary request).
#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    /// Optional provider-routing directive. When `Some`, the request is strictly
    /// pinned to the specified provider. When `None`, the field is omitted and
    /// OpenRouter uses its default routing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderRouting>,
}

// ---------------------------------------------------------------------------
// Non-streaming response (used by /compact summary)
// ---------------------------------------------------------------------------

/// Top-level non-streaming response envelope.
///
/// OpenRouter returns an array of `choices`; we always take `choices[0]`.
#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
}

/// One completion alternative inside a non-streaming response.
#[derive(Debug, Deserialize)]
pub struct Choice {
    pub message: ResponseMessage,
}

/// The finished message inside a non-streaming `Choice`.
///
/// `role` is received as a string (e.g. `"assistant"`) but is unused by the
/// caller; only `content` is extracted for the compaction summary.
#[derive(Debug, Deserialize)]
pub struct ResponseMessage {
    #[allow(dead_code)]
    pub role: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Streaming chunk (one SSE data: line)
// ---------------------------------------------------------------------------

/// One parsed SSE `data:` frame received during a streaming chat turn.
///
/// Each frame carries a `choices` array; in practice there is always exactly
/// one element. The frame is discarded once `Delta::content` is extracted.
#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    pub choices: Vec<StreamChoice>,
}

/// The single choice inside a streaming chunk.
#[derive(Debug, Deserialize)]
pub struct StreamChoice {
    pub delta: Delta,
}

/// Incremental content fragment for the current assistant turn.
///
/// `content` is `None` on the first and last frames (role-only / finish-reason
/// frames); callers should skip `None` values and append `Some(text)` to the
/// growing assistant bubble.
#[derive(Debug, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
}
