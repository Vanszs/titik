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

// --- Prompt-caching wire layer --------------------------------------------
//
// OpenRouter prompt caching wants ONE `cache_control: {"type":"ephemeral"}`
// breakpoint on the LAST content block of the stable prefix (here: the system
// message). `cache_control` can only ride a content block, so a cached message
// must serialise its `content` as an ARRAY of parts rather than a plain string.
//
// `ChatMessage.content` stays a `String` (it's used everywhere); this is a
// serialise-only mirror used solely when building the request body. Non-system
// messages serialise `content` as a plain string — byte-identical to the old
// wire format — so nothing about existing behaviour changes for them.

/// `cache_control` marker placed on a content part to open a cache breakpoint.
/// Serialises to `{"type":"ephemeral"}`. `pub(crate)` only to satisfy the
/// reachability of the wire types it's nested under; never named outside this
/// module.
#[derive(Serialize)]
pub(crate) struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

/// One content part of a multi-part message `content` array. Carries the text
/// plus an optional `cache_control` breakpoint (set only on the last part of
/// the stable prefix). `pub(crate)` only to satisfy the reachability of the wire
/// types it's nested under; never named outside this module.
#[derive(Serialize)]
pub(crate) struct ContentPart {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// Message `content` on the wire: either a plain string (no caching, identical
/// to the old format) or an array of parts (when a `cache_control` breakpoint
/// must be attached). `#[serde(untagged)]` makes the string serialise as a bare
/// JSON string and the parts as a JSON array, matching the API's accepted shapes.
///
/// `pub(crate)` only to match the reachability of `WireMessage::content` (which
/// is `pub` on a `pub(crate)` struct); it's never constructed outside this module
/// — `to_wire` is the only producer.
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum WireContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// Serialise-only mirror of [`ChatMessage`] for the request body. Field names /
/// serde attrs match `ChatMessage` exactly (`tool_calls` / `tool_call_id` with
/// `skip_serializing_if`) so the wire format is identical, except `content` can
/// now carry a `cache_control` breakpoint via [`WireContent::Parts`]. The
/// display-only `reasoning` field is intentionally absent (it's `#[serde(skip)]`
/// on `ChatMessage`, i.e. never sent).
#[derive(Serialize)]
pub struct WireMessage {
    pub role: crate::dto::chat::Role,
    pub content: WireContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<crate::dto::chat::ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Convert a conversation history into wire messages, placing the single prompt-
/// caching breakpoint on the system message (the stable prefix).
///
/// The System message gets `WireContent::Parts`; every other message serialises as
/// `WireContent::Text` — a plain `"content":"…"` string, byte-identical to the
/// pre-caching wire format. If there is no System message, no breakpoint is set
/// (all messages stay plain strings).
///
/// The System content may carry a [`CACHE_SPLIT_MARK`](crate::dto::chat::CACHE_SPLIT_MARK)
/// boundary that separates the STABLE cached head (base prompt + plan-word steer)
/// from the VOLATILE tail (project file listing + awareness summary, which change
/// across sessions/turns). When present, the content is split once at the mark
/// (which is stripped, never sent):
/// - the head becomes a `ContentPart` WITH the `cache_control: ephemeral`
///   breakpoint — only this byte-stable prefix is cached, so file changes in the
///   tail never bust the cache,
/// - the tail becomes a SECOND `ContentPart` WITHOUT `cache_control`, emitted only
///   when non-empty (an empty tail collapses to the single cached part).
///
/// When the System content has NO mark (e.g. secondary/utility calls that build a
/// fresh system message), the whole content is emitted as one cached part — the
/// original pre-split behaviour.
pub fn to_wire(messages: Vec<ChatMessage>) -> Vec<WireMessage> {
    messages
        .into_iter()
        .map(|m| {
            let content = if m.role == crate::dto::chat::Role::System {
                WireContent::Parts(system_parts(m.content))
            } else {
                WireContent::Text(m.content)
            };
            // Repair any tool-call argument string on the way OUT. A provider that
            // violated the streaming-delta contract may have persisted a malformed
            // `{...}{...}` arguments value into a stored assistant message; sending it
            // verbatim makes the provider's prefill/validation reject the whole
            // request ("unexpected content after document"), wedging the session.
            // `m.tool_calls` here is an owned clone of the stored message (the caller
            // passes `conversation.history()` clones), so cleaning it touches ONLY this
            // wire copy — the stored `ChatMessage` / `messages.json` is never mutated.
            // A single clean value is left semantically unchanged (no-op).
            let tool_calls = m.tool_calls.map(|mut calls| {
                for call in &mut calls {
                    call.function.arguments =
                        crate::dto::chat::sanitize_tool_arguments(&call.function.arguments);
                }
                calls
            });
            WireMessage {
                role: m.role,
                content,
                tool_calls,
                tool_call_id: m.tool_call_id,
            }
        })
        .collect()
}

/// Build the System message's wire content parts, splitting the stable cached head
/// from the volatile tail on [`CACHE_SPLIT_MARK`](crate::dto::chat::CACHE_SPLIT_MARK).
///
/// - mark present → `[head WITH cache_control, tail WITHOUT cache_control]`, the
///   tail part included only if non-empty; the mark itself is stripped.
/// - mark absent (or an empty tail) → a single cached part holding the whole
///   content — the original behaviour.
fn system_parts(content: String) -> Vec<ContentPart> {
    match content.split_once(crate::dto::chat::CACHE_SPLIT_MARK) {
        Some((head, tail)) if !tail.is_empty() => vec![
            ContentPart {
                kind: "text",
                text: head.to_string(),
                cache_control: Some(CacheControl { kind: "ephemeral" }),
            },
            ContentPart {
                kind: "text",
                text: tail.to_string(),
                cache_control: None,
            },
        ],
        // No mark, or an empty tail: one cached part holding the head (mark
        // stripped). `split_once` returns the head before the mark; with no mark
        // the whole string is the head.
        Some((head, _)) => vec![ContentPart {
            kind: "text",
            text: head.to_string(),
            cache_control: Some(CacheControl { kind: "ephemeral" }),
        }],
        None => vec![ContentPart {
            kind: "text",
            text: content,
            cache_control: Some(CacheControl { kind: "ephemeral" }),
        }],
    }
}

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
/// Only the fields this app uses are modelled; all are skipped when
/// `None` so the on-wire object carries exactly what was set:
/// - `{"reasoning":{"effort":"high"}}` selects a thinking effort level.
/// - `{"reasoning":{"enabled":false}}` turns thinking off entirely.
/// - `{"reasoning":{"exclude":true}}` keeps reasoning mandatory (satisfying
///   endpoints that require it) but strips the `reasoning` field from the
///   response — used by all secondary/utility model calls so their replies
///   are bleed-proof and the chain-of-thought is never persisted.
///
/// Omitting the whole struct (via `skip_serializing_if` on the `ChatRequest`
/// field) lets the model use its own default reasoning behaviour. `effort`,
/// `enabled`, and `exclude` are never set together — see `reasoning_config`
/// in the service.
#[derive(Debug, Serialize)]
pub struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<bool>,
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
#[derive(Serialize)]
pub struct ChatRequest {
    pub model: String,
    /// Wire-format messages. Built from a `Vec<ChatMessage>` via [`to_wire`],
    /// which attaches the single prompt-caching `cache_control` breakpoint to
    /// the system message; non-system messages serialise as plain strings.
    pub messages: Vec<WireMessage>,
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
    /// Hard cap on generated tokens. A generous limit (e.g. 32 000) on the
    /// interactive path guards against runaway generation; small caps (e.g. 2 000)
    /// on classifier/picker calls that return tiny JSON. `None` lets the provider
    /// use its own default. Omitted from the wire body when `None` via
    /// `skip_serializing_if`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
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

/// The `top_provider` sub-object of a model entry in `GET /models`.
///
/// OpenRouter exposes both a nominal `context_length` (the model's theoretical
/// maximum) and `top_provider.context_length` (the limit actually enforced by
/// the serving provider). The provider-served value is what matters for
/// summarisation thresholds; the nominal value is the fallback when this
/// object is absent.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct TopProvider {
    #[serde(default)]
    pub context_length: Option<u64>,
}

/// One model entry from `GET /models`. Only the fields the effort-capability
/// derivation needs are modelled; the rest of OpenRouter's rich model record is
/// ignored. `reasoning` is absent for models with no thinking support.
/// `context_length` is the model's maximum context window in tokens, taken from
/// the top-level field OpenRouter exposes on each model object.
/// `top_provider` carries the provider-served context limit, which takes
/// precedence over the nominal `context_length` when computing thresholds.
#[derive(Debug, Deserialize, Clone)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub reasoning: Option<ModelReasoning>,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub top_provider: Option<TopProvider>,
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
    /// Breakdown of the prompt tokens, including how many were served from the
    /// prompt cache. Present when the request set `usage: {"include": true}` and
    /// the provider reports cache stats; `None`/null otherwise (defaulted, so a
    /// missing object never fails to deserialise).
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// The `prompt_tokens_details` sub-object of [`Usage`]. `cached_tokens` is the
/// count of prompt tokens served from the prompt cache (a cache hit) at the
/// discounted rate — what prompt caching saves. Defaults to 0 so a partial /
/// absent object still deserialises.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
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
/// `content` is `Option<String>` because some models (e.g. deepseek-v4-flash)
/// return `"content": null` on a non-streaming response instead of an empty
/// string. `#[serde(default)]` additionally handles an absent field. Callers
/// use `.unwrap_or_default()` (or the reasoning fallback) to treat null/absent
/// as an empty string.
///
/// `reasoning` carries a reasoning model's thinking text: some models (e.g. the
/// safeguard classifier) leave `content` empty and return their answer in this
/// field instead, so the classifier path falls back to it. Defaults to `None`
/// for models that don't emit it.
#[derive(Debug, Deserialize)]
pub struct ResponseMessage {
    #[allow(dead_code)]
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
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
    /// Incremental reasoning/thinking fragment for the current assistant turn,
    /// streamed in a SEPARATE channel from `content` when reasoning is enabled
    /// (via `/effort`). Accumulated into the assistant message's display-only
    /// reasoning block; never echoed back to the API. `None` on frames the model
    /// doesn't think on (and absent entirely for non-reasoning models).
    #[serde(default)]
    pub reasoning: Option<String>,
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
