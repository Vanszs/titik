//! Private free-function helpers shared across the openrouter submodules.
//!
//! None of these are part of the public API; they exist here so the larger
//! submodules (stream, oneshot) can share them without duplication.

use tokio::sync::mpsc::UnboundedSender;

use crate::dto::chat::{sanitize_tool_arguments, ToolCall};
use crate::dto::openrouter::ReasoningConfig;
use crate::service::StreamEvent;

/// Send one event on the request channel, ignoring a closed receiver (the
/// request was interrupted/superseded, so the event is simply dropped).
pub(super) fn emit(tx: &UnboundedSender<StreamEvent>, event: StreamEvent) {
    let _ = tx.send(event);
}

/// Repair every accumulated tool call's `function.arguments` in place via
/// [`sanitize_tool_arguments`] before the assembled set leaves the client.
///
/// Streamed argument fragments are concatenated assuming pure deltas; providers
/// that re-send the FULL arguments per chunk (common on budget routes) make that
/// concatenation a malformed `{...}{...}` string. Collapsing it to one clean value
/// here keeps the bad string from entering the runtime/persistence pipeline — the
/// SOURCE-layer guard. A single clean value is left semantically unchanged.
pub(super) fn sanitize_tool_acc(tool_acc: &mut [ToolCall]) {
    for call in tool_acc.iter_mut() {
        call.function.arguments = sanitize_tool_arguments(&call.function.arguments);
    }
}

/// Build a provider-routing directive from a provider slug.
///
/// Returns `None` for an empty slug (OpenRouter default routing) and
/// `Some(ProviderRouting)` with `allow_fallbacks: false` otherwise, strictly
/// pinning the request to that single provider. Free helper so every request
/// path (streaming, `complete`, `complete_with`) shares one routing rule.
pub(super) fn provider_routing_for(
    provider: &str,
) -> Option<crate::dto::openrouter::ProviderRouting> {
    if provider.is_empty() {
        None
    } else {
        Some(crate::dto::openrouter::ProviderRouting {
            only: vec![provider.to_string()],
            allow_fallbacks: false,
        })
    }
}

/// Map a stored effort token to the request `reasoning` object.
///
/// - `""` / `"default"` → `None`: omit `reasoning` entirely so the model uses
///   its own default thinking behaviour.
/// - `"off"` / `"none"` → `Some(enabled: false)`: turn thinking off.
/// - any effort token (`minimal`/`low`/`medium`/`high`/`xhigh`/`max`/…) →
///   `Some(effort: <token>)`. `effort` and `enabled` are mutually exclusive, so
///   only `effort` is set here.
///
/// Free helper (not a method) so it has no hidden state — what you pass is what
/// you get. Applied only on the interactive chat path.
pub(super) fn reasoning_config(effort: &str) -> Option<ReasoningConfig> {
    match effort.trim() {
        "" | "default" => None,
        "off" | "none" => Some(ReasoningConfig {
            effort: None,
            enabled: Some(false),
            exclude: None,
        }),
        level => Some(ReasoningConfig {
            effort: Some(level.to_string()),
            enabled: None,
            exclude: None,
        }),
    }
}

/// Turn an OpenRouter error response body into a short human-readable message.
/// OpenRouter returns `{"error":{"message":..,"code":..,"metadata":{"raw":..}}}`;
/// the upstream provider's own text lives in `metadata.raw`, so prefer that, then
/// `message`, then a trimmed slice of the raw body. `status` renders as e.g.
/// "429 Too Many Requests".
pub(super) fn clean_error(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        let err = &v["error"];
        let raw = err["metadata"]["raw"].as_str().unwrap_or("");
        let msg = err["message"].as_str().unwrap_or("");
        let detail = if !raw.is_empty() { raw } else { msg };
        if !detail.is_empty() {
            let detail: String = detail.chars().take(200).collect();
            return format!("{status}: {detail}");
        }
    }
    let trimmed: String = body.chars().take(160).collect();
    if trimmed.trim().is_empty() {
        format!("{status}")
    } else {
        format!("{status}: {trimmed}")
    }
}
