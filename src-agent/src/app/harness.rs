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
//!   call in Normal mode. A "safe" verdict lets a normally-risky tool auto-run; a
//!   "block" verdict (or any error) forces the human approval prompt.
//!
//! Both classifier calls reuse [`OpenRouterClient::complete_with`] against the
//! configured `classifier_model` / `classifier_provider` — the same secondary-
//! model path the awareness summary uses. They build a two-message conversation
//! (System = the embedded policy text, User = the prompt / tool call) and parse a
//! single `VERDICT:` line out of the reply.

use std::path::Path;

use crate::dto::chat::{ChatMessage, Role};
use crate::model::settings::Settings;
use crate::service::openrouter::OpenRouterClient;

/// A classifier decision. `allow = true` means proceed; `reason` is a short
/// human-readable note (empty on a clean allow, the block reason otherwise).
#[derive(Debug, Clone)]
pub struct Verdict {
    pub allow: bool,
    pub reason: String,
}

impl Verdict {
    fn allow() -> Self {
        Self {
            allow: true,
            reason: String::new(),
        }
    }
    fn block(reason: impl Into<String>) -> Self {
        Self {
            allow: false,
            reason: reason.into(),
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
/// Scans for the first line containing `VERDICT:`. `ALLOW` → allow; `BLOCK` →
/// block with the text after `BLOCK` as the reason. If no `VERDICT:` line is
/// found the reply is malformed; the caller decides how to treat that via
/// `fallback` (PC fails open, TAC fails closed-to-approval).
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

/// Run the classifier model over `messages` and parse its verdict.
///
/// On any error (no key, network failure, empty reply) the `fallback` verdict is
/// returned — the call never propagates an error or panics. `block_on`-friendly:
/// it is a plain async fn the caller can drive from the sync loop.
async fn classify(
    client: &OpenRouterClient,
    settings: &Settings,
    messages: Vec<ChatMessage>,
    fallback: Verdict,
) -> Verdict {
    match client
        .complete_with(&settings.classifier_model, &settings.classifier_provider, messages)
        .await
    {
        Ok(reply) => parse_verdict(&reply, fallback),
        Err(_) => fallback,
    }
}

/// Prompt classifier (PC). Classify a user prompt; ADVISORY only.
///
/// FAIL-OPEN: a malformed reply or a failed call returns `allow` (with the note
/// "classifier unavailable") so the turn is never blocked by classifier trouble.
/// The caller surfaces a block verdict as a toast and otherwise proceeds.
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
        Verdict {
            allow: true,
            reason: "classifier unavailable".to_string(),
        },
    )
    .await
}

/// Tool-call classifier (TAC). Classify a single tool call for auto-run safety.
///
/// FAIL-TO-APPROVAL: on a malformed reply or a failed call this returns
/// `allow = false` with the reason "classifier unavailable". A false verdict
/// routes the call into the normal human-approval prompt — which is the SAFE
/// default for this cut: the user simply approves manually, and a classifier
/// outage can never silently auto-run a risky call. (Fully failing the turn
/// closed would be too aggressive, so we degrade to "ask the human" instead.)
pub async fn classify_toolcall(
    client: &OpenRouterClient,
    settings: &Settings,
    tool_name: &str,
    args_json: &str,
) -> Verdict {
    let call = format!("tool: {tool_name}\narguments: {args_json}");
    let messages = vec![
        ChatMessage::new(Role::System, crate::resources::classifier_toolcall()),
        ChatMessage::new(Role::User, call),
    ];
    classify(
        client,
        settings,
        messages,
        Verdict {
            allow: false,
            reason: "classifier unavailable".to_string(),
        },
    )
    .await
}
