//! Token and cost accounting types shared by streaming and non-streaming responses.

use serde::Deserialize;

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
