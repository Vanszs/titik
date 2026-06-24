//! Safety harness ("Pass B"): wraps the agentic tool loop with an LLM safety
//! classifier, plus a deterministic workspace check. All of it is gated behind
//! the master `classifier_enabled` setting (default off); when disabled the loop
//! behaves exactly as it did before this module existed.
//!
//! Three checks, per the locked design:
//!
//! - **WC (workspace check)** — [`workspace_allowed`]. Deterministic, no network:
//!   is the session workdir the launch directory, or in the allow-list? Tools run
//!   only against an allowed workspace.
//! - **PC (prompt classifier)** — [`classify_prompt`]. Runs ONCE per turn,
//!   ADVISORY only: classify the user's prompt and surface a toast if flagged.
//!   It never blocks the turn (fail-open).
//! - **TAC (tool-call classifier)** — [`classify_toolcall`]. Runs PER risky tool
//!   call in BOTH agent modes, INTENT-AWARE (it sees the user's latest request
//!   plus the proposed call). An "allow" verdict auto-runs the call; a "block"
//!   verdict is acted on by the caller per mode (Auto records a "blocked by
//!   harness" result and continues; Normal prompts the human). If the classifier
//!   is unavailable (error/timeout) the verdict's `available` flag is false and
//!   the caller degrades to a human prompt in both modes.
//!
//! Both classifier calls run against the configured `classifier_model` /
//! `classifier_provider`. PC uses the plain secondary-model path; TAC uses the
//! dedicated [`OpenRouterClient::classify_with`] (low reasoning effort, with a
//! content-or-reasoning fallback) so the safeguard reasoning model stays fast and
//! its verdict is still read when it lands in `message.reasoning`. They build a
//! two-message conversation (System = the embedded policy text, User = the prompt
//! / the request + tool call) and parse a single `VERDICT:` line out of the
//! reply, all bounded by an 8s timeout so the sync loop can't freeze.

use std::path::Path;

use crate::dto::chat::{ChatMessage, Role};
use crate::model::settings::Settings;
use crate::service::openrouter::OpenRouterClient;

/// A classifier decision.
///
/// - `allow`: proceed when true.
/// - `reason`: short human-readable note (empty on a clean allow, the block
///   reason otherwise).
/// - `available`: true when the classifier actually produced this verdict;
///   false when it couldn't be reached (network error, timeout, empty or
///   unparseable reply). The caller treats `available = false` specially —
///   TAC degrades to a human prompt in BOTH agent modes rather than trusting
///   `allow`, so a classifier outage never silently runs or blocks a call.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub allow: bool,
    pub reason: String,
    pub available: bool,
}

impl Verdict {
    /// A successfully-parsed ALLOW.
    fn allow() -> Self {
        Self {
            allow: true,
            reason: String::new(),
            available: true,
        }
    }
    /// A successfully-parsed BLOCK with its reason.
    fn block(reason: impl Into<String>) -> Self {
        Self {
            allow: false,
            reason: reason.into(),
            available: true,
        }
    }
    /// The classifier could not be reached / its reply was unusable. `allow`
    /// here is meaningless to the caller — `available = false` is the signal to
    /// degrade to a human prompt.
    fn unavailable() -> Self {
        Self {
            allow: false,
            reason: "classifier unavailable".to_string(),
            available: false,
        }
    }
}

/// Normalise a path to a comparable string. Prefer the canonical form (resolves
/// symlinks, `.`/`..`, and relative paths against the cwd); fall back to the
/// path as-given when it can't be canonicalised (e.g. it doesn't exist yet).
fn norm(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy().into_owned()
}

/// Deterministic workspace check (WC). Returns true when `workdir` is the
/// process launch directory OR appears in the session's allow-set: every
/// `settings.workdir` entry plus every `settings.allowed_folders` entry.
///
/// `workdir` (the dir actually being checked) is the session's *effective*
/// workdir — the first `settings.workdir` entry — but the path list may name
/// several directories the session is allowed to touch, so ALL of them count,
/// alongside the extra `allowed_folders`. The launch directory is ALWAYS allowed
/// regardless of the lists, so the common case (running the agent in the folder
/// you want to work in) just works.
///
/// Comparison is on canonicalised path strings so equivalent spellings (relative
/// vs absolute, trailing slash, symlink) of the same directory match.
pub fn workspace_allowed(settings: &Settings, workdir: &Path, launch_dir: &Path) -> bool {
    let wd = norm(workdir);
    if wd == norm(launch_dir) {
        return true;
    }
    // The allow-set is the union of the workdir path list and the extra allowed
    // folders; a blank entry can't match a real directory after canonicalisation.
    settings
        .workdir
        .iter()
        .chain(settings.allowed_folders.iter())
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .map(|f| norm(Path::new(f)))
        .any(|allowed| allowed == wd)
}

/// Parse a classifier reply into a [`Verdict`].
///
/// Scans for the first line containing `VERDICT:`. `ALLOW` → a parsed allow
/// (`available = true`); `BLOCK` → a parsed block (`available = true`) with the
/// text after `BLOCK` as the reason. If no `VERDICT:` line is found the reply is
/// malformed and the `fallback` is returned verbatim — callers pass a fallback
/// carrying `available = false` (PC keeps `allow = true` since it's advisory,
/// TAC keeps `allow = false`), so an unparseable reply is treated as "classifier
/// unavailable", never as a trusted verdict.
fn parse_verdict(reply: &str, fallback: Verdict) -> Verdict {
    for line in reply.lines() {
        let Some((_, rest)) = line.split_once("VERDICT:") else {
            continue;
        };
        let rest = rest.trim();
        let upper = rest.to_ascii_uppercase();
        if upper.starts_with("ALLOW") {
            return Verdict::allow();
        }
        if let Some(after) = upper.strip_prefix("BLOCK") {
            // Re-slice the ORIGINAL (non-uppercased) text for the reason so its
            // casing is preserved; `after` only told us where it starts.
            let reason = rest[rest.len() - after.len()..].trim();
            let reason = if reason.is_empty() {
                "flagged".to_string()
            } else {
                reason.to_string()
            };
            return Verdict::block(reason);
        }
        // A VERDICT: line that's neither ALLOW nor BLOCK is malformed — stop
        // scanning and use the fallback rather than guessing.
        break;
    }
    fallback
}

/// How long to wait for a classifier verdict before giving up. The safeguard
/// model is a reasoning model, so a slow generation could otherwise stall the
/// sync loop that drives this via `block_on`. On timeout the call degrades to
/// the `fallback` (TAC → human prompt), so the UI can never freeze waiting on it.
const CLASSIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Run the classifier model over `messages` and parse its verdict.
///
/// On any error (no key, network failure, empty reply) OR a timeout the
/// `fallback` verdict is returned — the call never propagates an error or
/// panics. The whole request is bounded by [`CLASSIFY_TIMEOUT`] so a slow
/// reasoning model can't hang the loop. `block_on`-friendly: it is a plain async
/// fn the caller can drive from the sync loop.
async fn classify(
    client: &OpenRouterClient,
    settings: &Settings,
    messages: Vec<ChatMessage>,
    fallback: Verdict,
) -> Verdict {
    match tokio::time::timeout(
        CLASSIFY_TIMEOUT,
        client.classify_with(
            &settings.classifier_model,
            &settings.classifier_provider,
            messages,
        ),
    )
    .await
    {
        Ok(Ok(reply)) => parse_verdict(&reply, fallback),
        // Inner error (network / empty reply) or outer timeout → unavailable.
        _ => fallback,
    }
}

/// Prompt classifier (PC). Classify a user prompt; ADVISORY only.
///
/// FAIL-OPEN: a malformed reply or a failed call returns `allow = true` (with
/// the note "classifier unavailable" and `available = false`) so the turn is
/// never blocked by classifier trouble. The caller surfaces a block verdict as a
/// toast and otherwise proceeds; it ignores `available` because PC is advisory.
pub async fn classify_prompt(
    client: &OpenRouterClient,
    settings: &Settings,
    user_prompt: &str,
) -> Verdict {
    let messages = vec![
        ChatMessage::new(Role::System, crate::resources::classifier_prompt()),
        ChatMessage::new(Role::User, user_prompt),
    ];
    classify(
        client,
        settings,
        messages,
        // Advisory fail-open: allow the turn, but mark it unavailable so the
        // note is accurate.
        Verdict {
            allow: true,
            reason: "classifier unavailable".to_string(),
            available: false,
        },
    )
    .await
}

/// Tool-call classifier (TAC). Classify a single tool call for auto-run safety,
/// INTENT-AWARE: it sees the user's latest request alongside the proposed call,
/// so it can block a mutation the user never asked for (e.g. the user asked a
/// question but the model tried to edit a file).
///
/// On a malformed reply, a failed call, or a timeout this returns a verdict with
/// `available = false` — the caller degrades that to a human approval prompt in
/// BOTH agent modes, so a classifier outage can never silently auto-run a risky
/// call nor silently block it. A successfully-parsed verdict carries
/// `available = true` (allow → auto-run; block → the caller acts on the mode).
pub async fn classify_toolcall(
    client: &OpenRouterClient,
    settings: &Settings,
    user_intent: &str,
    tool_name: &str,
    args_json: &str,
) -> Verdict {
    let call = format!(
        "User request: {user_intent}\n\nProposed tool call:\ntool: {tool_name}\narguments: {args_json}"
    );
    let messages = vec![
        ChatMessage::new(Role::System, crate::resources::classifier_toolcall()),
        ChatMessage::new(Role::User, call),
    ];
    classify(client, settings, messages, Verdict::unavailable()).await
}
