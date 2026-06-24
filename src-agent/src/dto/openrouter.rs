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

/// Request-side usage accounting directive.
///
/// Serialises to `{"include": true}`. When present on the request body,
/// OpenRouter returns token counts AND the total generation `cost` in the
/// response — including the final streaming chunk (which carries `usage` with
/// an empty `choices` array).
#[derive(Debug, Clone, Serialize)]
pub struct UsageRequest {
    pub include: bool,
}

/// Reasoning/thinking control for the request, serialised as the top-level
/// `"reasoning"` object.
///
/// Only the two fields this app uses are modelled; both are skipped when
/// `None` so the on-wire object carries exactly what was set:
/// - `{"reasoning":{"effort":"high"}}` selects a thinking effort level.
/// - `{"reasoning":{"enabled":false}}` turns thinking off entirely.
///
/// Omitting the whole struct (via `skip_serializing_if` on the `ChatRequest`
/// field) lets the model use its own default reasoning behaviour. `effort` and
/// `enabled` are never set together — see `reasoning_config` in the service.
#[derive(Debug, Serialize)]
pub struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// JSON-Schema description of one tool's `function`, as required by the
/// OpenAI/OpenRouter `tools` request field. `parameters` is the tool's raw
/// JSON-Schema object (taken verbatim from `Tool::parameters`).
#[derive(Debug, Clone, Serialize)]
pub struct ToolFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// One entry in the request `tools` array. `kind` is always `"function"`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunctionDef,
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
    /// Usage accounting directive — always sent as `{"include": true}` so the
    /// response (and final streaming chunk) reports token counts + total cost.
    pub usage: UsageRequest,
    /// Function-calling tool definitions exposed to the model. `Some` on the
    /// streaming chat path (so the model can request tool calls); omitted via
    /// `skip_serializing_if` on the `/compact` summary call, which uses no tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    /// Reasoning/thinking directive. `Some` only on the interactive chat path
    /// (set from the session's `effort`); `None` everywhere else (compaction +
    /// secondary-model calls don't think) and omitted from the body via
    /// `skip_serializing_if`, so the model falls back to its own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    /// Structured-output directive, serialised as the top-level `response_format`
    /// object. `Some` only on the classifier path, where it pins a strict
    /// `json_schema` so the verdict comes back as machine-parseable JSON; `None`
    /// everywhere else (and omitted from the body via `skip_serializing_if`), so
    /// the interactive/compaction calls emit free-form text as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Models list (inbound, GET /models — drives the /effort capability menu)
// ---------------------------------------------------------------------------

/// The `reasoning` sub-object of a model entry in `GET /models`.
///
/// Both fields default so a model that omits one (or omits `reasoning`
/// entirely) still deserialises. `supported_efforts` is the list of effort
/// tokens the model accepts (e.g. `["high","low"]`); empty means the model
/// either takes no discrete efforts (on/off only) or none were reported.
/// `mandatory` is true when reasoning can't be turned off.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ModelReasoning {
    #[serde(default)]
    pub mandatory: bool,
    #[serde(default)]
    pub supported_efforts: Vec<String>,
}

/// One model entry from `GET /models`. Only the fields the effort-capability
/// derivation needs are modelled; the rest of OpenRouter's rich model record is
/// ignored. `reasoning` is absent for models with no thinking support.
#[derive(Debug, Deserialize, Clone)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub reasoning: Option<ModelReasoning>,
}

/// Top-level envelope of `GET /models`: `{ "data": [ ModelInfo, ... ] }`.
#[derive(Debug, Deserialize)]
pub struct ModelsResponse {
    pub data: Vec<ModelInfo>,
}

// ---------------------------------------------------------------------------
// Usage (inbound, shared by streaming + non-streaming responses)
// ---------------------------------------------------------------------------

/// Token + cost accounting returned by OpenRouter when the request body sets
/// `usage: {"include": true}`. On a streaming response this rides the final
/// chunk (the one with an empty `choices` array). All fields default to zero so
/// a partial/absent `usage` object never fails to deserialise.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    /// OpenRouter total cost (USD) for this generation.
    #[serde(default)]
    pub cost: f64,
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
    /// Token/cost accounting (present when the request asked for it). Unused by
    /// the compaction caller today, kept for completeness with the streaming path.
    #[allow(dead_code)]
    #[serde(default)]
    pub usage: Option<Usage>,
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
///
/// `reasoning` carries a reasoning model's thinking text: some models (e.g. the
/// safeguard classifier) leave `content` empty and return their answer in this
/// field instead, so the classifier path falls back to it. Defaults to `None`
/// for models that don't emit it.
#[derive(Debug, Deserialize)]
pub struct ResponseMessage {
    #[allow(dead_code)]
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub reasoning: Option<String>,
}

// ---------------------------------------------------------------------------
// Streaming chunk (one SSE data: line)
// ---------------------------------------------------------------------------

/// One parsed SSE `data:` frame received during a streaming chat turn.
///
/// Most frames carry a one-element `choices` array whose `Delta::content` is
/// appended to the assistant bubble. The FINAL frame instead carries `usage`
/// (token counts + cost) with an empty `choices` array, so both fields are
/// handled independently.
#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    pub choices: Vec<StreamChoice>,
    /// Present only on the terminal chunk: token/cost accounting for the whole
    /// generation. Absent (`None`) on every content-bearing chunk.
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// The single choice inside a streaming chunk.
#[derive(Debug, Deserialize)]
pub struct StreamChoice {
    pub delta: Delta,
    /// Set on the final content frame for this choice: `"stop"`, `"tool_calls"`,
    /// `"length"`, etc. Absent on intermediate frames. Defaults to `None`.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Incremental content fragment for the current assistant turn.
///
/// `content` is `None` on the first and last frames (role-only / finish-reason
/// frames); callers should skip `None` values and append `Some(text)` to the
/// growing assistant bubble. `tool_calls` carries incremental function-call
/// deltas (id / name / argument fragments) that the caller accumulates by index.
#[derive(Debug, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    /// Incremental tool-call fragments. The model streams a tool call across
    /// several frames: the first carries the `id` + function `name`, subsequent
    /// frames append `arguments` text. Each entry's `index` selects the slot to
    /// merge into.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// One streamed fragment of a tool call. `index` identifies which tool call in
/// the assistant turn this fragment belongs to (the model may stream several in
/// parallel). `id` and `function.name` arrive once; `function.arguments` is the
/// fragment to append to that call's growing argument string.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    pub id: Option<String>,
    pub function: Option<FunctionDelta>,
}

/// The `function` slice of a [`ToolCallDelta`]: either the call's `name` (sent
/// on the first fragment) or a chunk of its JSON-encoded `arguments` string.
#[derive(Debug, Clone, Deserialize)]
pub struct FunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}
