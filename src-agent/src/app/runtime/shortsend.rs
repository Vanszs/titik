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

use std::path::Path;

use anyhow::Result;

use crate::dto::chat::{ChatMessage, Role};
use crate::model::msglog;
use crate::model::settings::Settings;
use crate::resources;
use crate::service::openrouter::OpenRouterClient;

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
/// After a fold, the verbatim tail is shrunk down to roughly this % of `usable`.
const TAIL_FLOOR_PCT: u64 = 5;
/// Once the verbatim tail grows past this % of `usable`, refold (advance the
/// watermark). Below it the fold is a no-op — the hysteresis dead-zone that
/// avoids a summarizer call every single turn.
const TAIL_HI_PCT: u64 = 15;
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

/// Estimate the conversation's token cost from an API-bound history slice:
/// `~4 chars/token` over each message's content plus its tool-call arguments
/// (often the bulk of a turn). This is the SAME estimate shape the engage gate
/// uses; `start_stream_task` calls it to size the conversation against `usable`
/// before deciding whether to summarize. No tokenizer needed — fast + cheap.
pub(super) fn estimate_conv_tokens(history: &[ChatMessage]) -> u64 {
    history
        .iter()
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

/// Most blobs to rehydrate into a single send payload. A hard cap so the router
/// can't undo the compression by recalling the whole archive — the summary
/// already carries the gist; recalls are for the few items the question needs.
const MAX_REHYDRATE: usize = 3;

/// Pick the secondary model + provider for short-send's own LLM calls (the fold
/// and the router). Mirrors [`update_summary`]'s selection EXACTLY: inherit the
/// session's own model/provider when `awareness_inherit`, else use the dedicated
/// awareness model/provider. An empty provider means default routing.
fn awareness_route(settings: &Settings) -> (&str, &str) {
    if settings.awareness_inherit {
        (settings.model.as_str(), settings.provider.as_str())
    } else {
        (
            settings.awareness_model.as_str(),
            settings.awareness_provider.as_str(),
        )
    }
}

/// Upper bound on how many delta messages to pull in one fold. Large enough that
/// a normal session's un-summarised backlog fits in a single pass; the verbatim
/// tail is excluded separately by the `fold_up_to` cap.
const DELTA_LIMIT: i64 = 10_000;

/// Max characters kept from any single message's content when building the fold
/// payload. Bounds the secondary call so one giant message can't blow its
/// context; heavy content is referenced via blobs anyway, not pasted in full.
const PER_MESSAGE_CAP: usize = 6000;

/// Take at most `cap` chars from `s` (char-boundary safe).
fn cap_chars(s: &str, cap: usize) -> String {
    s.chars().take(cap).collect()
}

/// Fold newly-archived messages into the session's rolling summary, advancing the
/// summary watermark by a TOKEN BAND rather than a fixed message count.
///
/// `usable = context_window - BASE_OVERHEAD` is the budget the percentages are
/// taken against. The verbatim tail (messages after the current watermark) is
/// allowed to grow up to [`TAIL_HI_PCT`] of `usable`; only when it crosses that
/// high-water mark do we fold, and we fold just enough that the REMAINING tail
/// drops back to ~[`TAIL_FLOOR_PCT`]. This hysteresis dead-zone means we do NOT
/// pay for a summarizer call on every turn — only when the tail has genuinely
/// grown past the band.
///
/// Returns `Ok(true)` when a fold happened (a new summary was written) and
/// `Ok(false)` when there was nothing to fold (tail still within band, or no
/// valid completed-exchange boundary) — so the caller can skip the write
/// entirely. Errors propagate from the secondary model call; the sqlite helpers
/// are best-effort and degrade to empty rather than erroring.
pub async fn update_summary(
    session_dir: &Path,
    client: &OpenRouterClient,
    settings: &Settings,
    usable: u64,
) -> Result<bool> {
    // Existing summary state. Absent row (first ever fold) → empty text, covers 0.
    let cur = msglog::read_summary(session_dir);
    let existing_text = cur.as_ref().map(|s| s.text.as_str()).unwrap_or("");
    let covers_up_to = cur.as_ref().map(|s| s.covers_up_to).unwrap_or(0);

    // The verbatim tail = every message after the current watermark. Measure its
    // token cost (~4 chars/token over content) to decide whether it has grown out
    // of band. (fetch returns id ASC, id > covers_up_to.)
    let tail: Vec<msglog::ArchivedMsg> =
        msglog::fetch_messages_since(session_dir, covers_up_to, DELTA_LIMIT);
    if tail.is_empty() {
        return Ok(false);
    }
    let tok = |s: &str| s.chars().count() as u64 / 4;
    let tail_tokens: u64 = tail.iter().map(|m| tok(&m.content)).sum();

    // Hysteresis dead-zone: the tail is still within band → no fold this turn.
    // This is the whole point of the token-band design: a fold (a secondary LLM
    // call) only fires when the tail has actually outgrown TAIL_HI_PCT.
    let tail_hi = TAIL_HI_PCT * usable / 100;
    if tail_tokens <= tail_hi {
        return Ok(false);
    }

    // Pick the cut so the REMAINING verbatim tail is ~TAIL_FLOOR_PCT of usable.
    // Walk the tail NEWEST→oldest accumulating tokens; the first message at which
    // the kept-newest total reaches `tail_floor` is the youngest message we still
    // keep. Everything OLDER than it is a fold candidate, so the walked cut point
    // is the id of the message just before that kept boundary.
    let tail_floor = (TAIL_FLOOR_PCT * usable / 100).max(1);
    let mut kept = 0u64;
    // Default the cut to "fold the whole tail" (cut at the newest id); the loop
    // below raises it to the boundary where the kept-newest tokens hit the floor.
    let mut cut_id = tail.last().map(|m| m.id).unwrap_or(covers_up_to);
    for m in tail.iter().rev() {
        kept += tok(&m.content);
        if kept >= tail_floor {
            // `m` is the oldest message we KEEP; fold everything strictly before it.
            cut_id = m.id - 1;
            break;
        }
    }

    // Snap the cut DOWN to a COMPLETED-exchange edge so a tool-call chain is never
    // cut and the current, in-progress exchange is never summarised. An exchange
    // runs from one `user` message up to (but not including) the next; the most
    // recent `user` message begins the live exchange, which must stay verbatim.
    // Valid boundaries are `(user_id) - 1`: pick the LARGEST that is <= the walked
    // cut, strictly > covers_up_to (so we actually advance), and < the last user
    // message id (so the live exchange is never folded).
    let user_ids = msglog::user_message_ids(session_dir); // ascending
    let last_user = user_ids.last().copied().unwrap_or(i64::MAX);
    let fold_up_to = match user_ids
        .iter()
        .copied()
        .filter(|&u| u < last_user) // never fold the in-progress (last) exchange
        .map(|u| u - 1)
        .filter(|&b| b <= cut_id && b > covers_up_to)
        .max()
    {
        Some(b) => b,
        None => return Ok(false), // no completed-exchange boundary in range → don't fold
    };

    // Pull everything after the last-covered id, then trim to the fold ceiling so
    // the verbatim tail stays out. (fetch returns id ASC, id > covers_up_to.)
    let delta: Vec<msglog::ArchivedMsg> =
        msglog::fetch_messages_since(session_dir, covers_up_to, DELTA_LIMIT)
            .into_iter()
            .filter(|m| m.id <= fold_up_to)
            .collect();
    if delta.is_empty() {
        return Ok(false);
    }

    // Blobs whose owning message falls in the delta range (covers_up_to, fold_up_to].
    // These are the heavy items the new messages may reference; older blobs were
    // already folded into the existing summary, newer ones belong to the tail.
    let blobs: Vec<msglog::BlobRef> = msglog::list_blobs(session_dir)
        .into_iter()
        .filter(|b| b.msg_id > covers_up_to && b.msg_id <= fold_up_to)
        .collect();

    let user_payload = build_payload(existing_text, &delta, &blobs);

    // Reuse the awareness model/provider selection — no new settings in this phase.
    // Inherit the session's own model/provider, or use the dedicated awareness
    // model. An empty provider means default routing.
    let (model, provider) = awareness_route(settings);

    let new_text = client
        .summarize_fold(
            model,
            Some(provider),
            resources::shortsend_summary_prompt(),
            &user_payload,
        )
        .await?;

    // Persist: the new summary now covers through `fold_up_to`; the live-send
    // start id is the first message past it.
    msglog::write_summary(session_dir, &new_text, fold_up_to, fold_up_to + 1)?;
    Ok(true)
}

/// Assemble the plain-text fold payload: three labeled sections the prompt
/// expects — the existing summary, the new messages to merge (each capped), and
/// the available blob references the summary may point at instead of inlining.
fn build_payload(
    existing_text: &str,
    delta: &[msglog::ArchivedMsg],
    blobs: &[msglog::BlobRef],
) -> String {
    let mut out = String::new();

    out.push_str("=== EXISTING SUMMARY ===\n");
    let existing = existing_text.trim();
    if existing.is_empty() {
        out.push_str("(none)\n");
    } else {
        out.push_str(existing);
        out.push('\n');
    }

    out.push_str("\n=== NEW MESSAGES ===\n");
    for m in delta {
        // `role: content`, with any single message bounded so the call stays small.
        let content = cap_chars(m.content.trim(), PER_MESSAGE_CAP);
        out.push_str(&m.role);
        out.push_str(": ");
        out.push_str(&content);
        out.push_str("\n\n");
    }

    out.push_str("=== AVAILABLE BLOBS ===\n");
    if blobs.is_empty() {
        out.push_str("(none)\n");
    } else {
        for b in blobs {
            // `#<id> [<kind>] <snippet>` — the id the summary references as
            // `[blob #<id>]` instead of pasting the heavy content.
            out.push('#');
            out.push_str(&b.id.to_string());
            out.push_str(" [");
            out.push_str(&b.kind);
            out.push_str("] ");
            out.push_str(b.snippet.trim());
            out.push('\n');
        }
    }

    out
}

/// Build the router's user payload: the user's latest message followed by the
/// candidate blob list, one per line as `#<id> [<kind>] <snippet>`. The router
/// reads this against [`shortsend_router_prompt`] and returns the ids whose full
/// content the answer needs.
fn build_router_payload(user_intent: &str, candidates: &[msglog::BlobRef]) -> String {
    let mut out = String::new();
    out.push_str("=== USER MESSAGE ===\n");
    out.push_str(user_intent.trim());
    out.push_str("\n\n=== AVAILABLE BLOBS ===\n");
    for b in candidates {
        out.push('#');
        out.push_str(&b.id.to_string());
        out.push_str(" [");
        out.push_str(&b.kind);
        out.push_str("] ");
        out.push_str(b.snippet.trim());
        out.push('\n');
    }
    out
}

/// Reshape the API-bound history into the short-send payload: drop the older
/// turns in favour of the rolling summary + a verbatim tail, rehydrating only the
/// archived blobs the router judges relevant to `user_intent`.
///
/// This is a PURE transform over the wire payload: it reads `messages.sqlite` and
/// builds a brand-new `Vec<ChatMessage>`, and it NEVER mutates stored or displayed
/// state. The caller passes the full API-bound history (system + body) and sends
/// the returned vec instead; the live `Conversation`, `messages.json`, and the
/// rendered transcript are untouched (dual rail — display is unaffected).
///
/// Fail-open at every step: a disabled kill switch, a too-short history, a missing
/// summary, or ANY internal failure returns the original `history` (or the
/// summary-less history) so a turn is never broken. The fold + router both run
/// with reasoning OFF and parse clean output only, so no chain-of-thought can
/// bleed into the payload.
///
/// `history[0]` (the system message, already carrying any project-files/awareness
/// injection from the caller) is preserved as index 0 of the output. The rolling
/// summary + any rehydrated blobs are APPENDED to its content (after the volatile
/// dir-listing/awareness tail), NOT emitted as a synthetic assistant turn — so the
/// caching wire layer's mark-split still lands on the real system message, and the
/// per-fold summary stays in the uncached tail (it must not bust the cached head).
///
/// `summarizing` is the engage decision, made UPSTREAM in `start_stream_task`
/// (cache-warmth + sticky hysteresis). When false, the full history is sent
/// verbatim (cheap via prompt caching) and this is a no-op. `usable =
/// context_window - BASE_OVERHEAD` is the token budget the fold's band sizing is
/// taken against.
pub async fn shape(
    history: Vec<ChatMessage>,
    session_dir: &Path,
    client: &OpenRouterClient,
    settings: &Settings,
    user_intent: &str,
    summarizing: bool,
    usable: u64,
) -> Vec<ChatMessage> {
    // 1. Kill switch: short-send disabled → send the full history unchanged.
    if !settings.short_send_enabled {
        return history;
    }

    // 2. Engage gate. The cache-warmth + sticky-hysteresis decision is made
    //    upstream (in `start_stream_task`); here we simply honour it. Not engaged
    //    → send the full history (cheap via prompt caching; full fidelity).
    if !summarizing {
        return history;
    }

    // 2b. Too short to be worth compressing: need a system message plus a couple
    //     of body messages, else dropping the old part saves nothing. Sized off
    //     `usable` would be overkill for such a tiny payload — a small structural
    //     floor is enough (we already know we're engaged).
    if history.len() <= 3 {
        return history;
    }

    // 2c. Post-compaction guard: if the conversation already carries a compaction
    //     summary turn (i.e. history[1] is an Assistant message whose content
    //     starts with "[summary of earlier conversation]"), stacking our own
    //     sqlite summary on top would produce a broken double-summary payload.
    //     Marker matches `Conversation::apply_compaction` in src/model/conversation.rs.
    const COMPACTION_MARKER: &str = "[summary of earlier conversation]";
    if history.len() >= 2 {
        if let Some(msg) = history.get(1) {
            if msg.role == Role::Assistant && msg.content.starts_with(COMPACTION_MARKER) {
                return history;
            }
        }
    }

    // 3. Best-effort fold so the summary reflects everything older than the tail.
    //    Token-band step-advance: this is a no-op unless the verbatim tail has
    //    grown past TAIL_HI_PCT of `usable`. Errors / "nothing to fold" are ignored
    //    — we use whatever summary already exists.
    let _ = update_summary(session_dir, client, settings, usable).await;

    // 4. No summary yet → nothing to compress against; send the full history.
    let Some(sum) = msglog::read_summary(session_dir) else {
        return history;
    };

    // Everything newer than the summary boundary must stay verbatim: that span is
    // the current (in-progress) exchange plus any completed tail the fold hasn't
    // advanced into yet. The fold only ever snaps `covers_up_to` to a
    // completed-exchange edge, so this auto-grows the verbatim window to cover the
    // whole live exchange and leaves NO gap between the summary and the tail.
    // Clamp to >= 1 so we always keep at least one message. Part 3 bounds this to
    // ~<= TAIL_HI_PCT of `usable`.
    let max_id = msglog::max_message_id(session_dir);
    let keep = (max_id.saturating_sub(sum.covers_up_to)).max(1) as usize;

    // 5. Split into [system, body...]. Defensive: a missing/empty history or a
    //    body shorter than the kept tail falls open to the original.
    if history.is_empty() {
        return history;
    }
    let body = &history[1..];
    if body.is_empty() {
        return history;
    }
    // Keep the LAST `keep` messages verbatim (clamped to the body length);
    // everything before them is the `old` region the summary stands in for
    // (dropped from the payload). `keep` covers everything after the summary
    // boundary, so no message is left both un-summarised and un-sent.
    let keep = keep.min(body.len());
    let tail: Vec<ChatMessage> = body[body.len() - keep..].to_vec();

    // 6. Router rehydrate. Candidate blobs are those in the SUMMARISED region
    //    (msg_id <= covers_up_to), i.e. NOT in the verbatim tail — the tail still
    //    carries its own heavy content in full. Ask the router which to inflate.
    let mut recalls: Vec<String> = Vec::new();
    let candidates: Vec<msglog::BlobRef> = msglog::list_blobs(session_dir)
        .into_iter()
        .filter(|b| b.msg_id <= sum.covers_up_to)
        .collect();
    if !candidates.is_empty() {
        let (model, provider) = awareness_route(settings);
        let payload = build_router_payload(user_intent, &candidates);
        // Best-effort: `pick_blobs` already returns an empty vec on any error.
        let picked = client
            .pick_blobs(model, provider, resources::shortsend_router_prompt(), &payload)
            .await
            .unwrap_or_default();
        for id in picked {
            if recalls.len() >= MAX_REHYDRATE {
                break; // cap rehydration so the router can't undo the compression
            }
            // The router returns `blobs.id` values (the ids shown in the payload).
            // Map back to the candidate to (a) reject any id we never offered
            // (guards a hallucinated id) and (b) resolve its `msg_id` — full blob
            // content lives in the `messages` row, so `fetch_blob_content` keys on
            // the message id, not the blob id. Skip any whose content is missing.
            let Some(cand) = candidates.iter().find(|c| c.id == id) else {
                continue;
            };
            if let Some(content) = msglog::fetch_blob_content(session_dir, cand.msg_id) {
                recalls.push(format!("\n\n[recalled blob #{id}]\n{content}"));
            }
        }
    }

    // 7. B-PLACEMENT: the summary goes into the SYSTEM message tail, NOT a
    //    synthetic assistant turn. `history[0]` already ends (after
    //    CACHE_SPLIT_MARK) with the volatile dir-listing/awareness block; append
    //    the condensed-history summary, then the rehydrated blob blocks, after it.
    //    Because all of this lands AFTER the mark, it rides in the UNCACHED tail —
    //    correct, since the summary changes per fold and must not bust the cached
    //    head. `shape` owns `history`, so we mutate index 0 in place.
    let mut system = history[0].clone();
    system.content.push_str(&format!(
        "\n\n# Conversation so far (reference — earlier turns, condensed)\n{}",
        sum.text
    ));
    for block in &recalls {
        system.content.push_str(block);
    }

    // 8. Output: [ modified system (index 0), verbatim tail... ]. No synthetic
    //    assistant summary turn — index 0 stays the system message so `to_wire`'s
    //    mark-split still applies.
    let mut out: Vec<ChatMessage> = Vec::with_capacity(1 + tail.len());
    out.push(system);
    out.extend(tail);
    out
}
