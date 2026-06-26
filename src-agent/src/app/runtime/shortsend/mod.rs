//! Short-send rolling summary: the incremental "fold" step (Phase 2).
//!
//! Keeping the full chat history in every request is expensive. The short-send
//! architecture instead maintains ONE dense rolling summary of the older history
//! (in `messages.sqlite`'s `summary` row) plus a verbatim tail of the newest N
//! messages. This module owns the fold: it reads the messages that have grown
//! past the verbatim tail but aren't yet summarised, asks a secondary model to
//! merge them into the running summary, and persists the result.
//!
//! ## Bleed guard (critical)
//!
//! The fold is a SECOND LLM call, and its output is PERSISTED and replayed into
//! every future send-payload. A leaked chain-of-thought here is therefore strictly
//! worse than a transient bleed — it poisons the conversation permanently. The
//! request runs with reasoning explicitly OFF (see
//! [`OpenRouterClient::summarize_fold`]) and the fold prompt instructs the model
//! to emit ONLY the summary. We only ever read already-clean message `content`
//! from sqlite (the reasoning channel is never stored there), so nothing on this
//! path can introduce model thinking into the archive.
//!
//! Everything is best-effort and append-only: we read messages/blobs and upsert
//! the single summary row via the Phase-1 helpers; the `messages` table is never
//! mutated.
//!
//! ## Send-path reshaper (Phase 3)
//!
//! [`shape`] is the payoff: a PURE transform over the API-bound history that drops
//! the older turns in favour of the rolling summary + a verbatim tail, rehydrating
//! only the archived blobs a strict-JSON router (reasoning OFF) judges relevant to
//! the current question. It reads sqlite and builds a NEW `Vec<ChatMessage>` — it
//! never touches the live `Conversation`, `messages.json`, or the rendered
//! transcript (dual rail: only the wire payload is compressed). It folds first
//! (via [`update_summary`]) and fails open at every step, so a turn is never
//! broken. The send path applies it inside the spawned stream task, just before
//! the request is POSTed.

mod fold;
mod recall;

// --- Cache-warmth-adaptive, hysteresis-driven summarization rail ---------------
//
// All of these are token budgets expressed as a PERCENTAGE of `usable`, where
// `usable = context_window - BASE_OVERHEAD`. The engage decision (cold/warm +
// sticky hysteresis) is made upstream in `start_stream_task`; the fold boundary
// (token-band step-advance) is made in `update_summary`. Both consume `usable`.

/// Fixed system+tools+memory token cost that never appears in `history` but DOES
/// count against the model's context window. Subtracted off the window so every
/// percentage below is taken against the budget actually available to the chat.
pub(super) const BASE_OVERHEAD: u64 = 10_000;
/// Engage threshold (conversation size as % of `usable`) when the prompt cache is
/// cold or absent: summarize sooner, since there's no warm cache to ride.
pub(super) const ENGAGE_COLD_PCT: u64 = 20;
/// Engage threshold when the cache is warm: let the conversation grow far larger
/// before summarizing, since a warm cache makes the big prefix cheap.
pub(super) const ENGAGE_WARM_PCT: u64 = 80;
/// Sticky disengage floor: once engaged, KEEP summarizing until the conversation
/// shrinks below this % of `usable`. The gap between this and the engage
/// thresholds is the hysteresis band that prevents flapping on/off each turn.
pub(super) const DISENGAGE_PCT: u64 = 15;

use crate::dto::chat::{ChatMessage, Role};

/// Estimate the conversation's token cost from an API-bound history slice:
/// `~4 chars/token` over each message's content plus its tool-call arguments
/// (often the bulk of a turn). This is the SAME estimate shape the engage gate
/// uses; `start_stream_task` calls it to size the conversation against `usable`
/// before deciding whether to summarize. No tokenizer needed — fast + cheap.
pub(super) fn estimate_conv_tokens(history: &[ChatMessage]) -> u64 {
    history
        .iter()
        // Skip the System message: the ~10k base it carries is already accounted
        // for as BASE_OVERHEAD (the engage math subtracts it from the window), so
        // summing it here would double-count and trip the engage gate too early.
        .filter(|m| m.role != Role::System)
        .map(|m| {
            let base = m.content.chars().count() as u64 / 4;
            let args: u64 = m
                .tool_calls
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|tc| tc.function.arguments.chars().count() as u64 / 4)
                .sum();
            base + args
        })
        .sum()
}

// Re-export the public API so callers outside this module use the same paths
// as before the split.
pub use recall::shape;
