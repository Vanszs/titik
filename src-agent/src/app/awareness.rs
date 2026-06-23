//! Project self-awareness (Phase 2): summarise the project's docs once per
//! session so the agent knows what it's working on without burning a full chat
//! round on it.
//!
//! On startup (and after `/compact`) the runtime reads the depth-1 project
//! docs (AGENT.md / AGENTS.md / README.md / CLAUDE.md) and asks a cheap
//! secondary model — via [`OpenRouterClient::complete_with`] — for a few-
//! sentence summary. That summary is stashed in `AppStateRest::awareness_summary`
//! and appended to the first System message on every request (see
//! `runtime::stream::start_stream_task`), so it survives compaction the same way
//! the top-level file listing does.
//!
//! Everything here is best-effort: a disabled flag, missing docs, no API key, or
//! a failed network call all degrade to `None`. The summary is never persisted —
//! it is recomputed per session so it always reflects the current docs.

use std::path::Path;

use crate::dto::chat::{ChatMessage, Role};
use crate::model::settings::Settings;
use crate::service::openrouter::OpenRouterClient;

/// Project doc filenames probed at depth 1 of the workspace, in priority order.
/// Case-sensitive (matched verbatim) — the same names the rest of the app uses.
const DOC_FILES: &[&str] = &["AGENT.md", "AGENTS.md", "README.md", "CLAUDE.md"];

/// Max characters kept from any single doc file.
const PER_FILE_CAP: usize = 6000;

/// Max characters of combined doc corpus sent to the model.
const CORPUS_CAP: usize = 16000;

/// System instruction for the summary call. Kept terse and concrete so a small
/// model stays on task and returns prose the agent can use directly.
const SUMMARY_SYSTEM: &str = "You summarize a software project for an AI coding assistant. In 4-6 sentences, state what the project is, its stack, structure, and any conventions the agent must follow. Be concrete. No preamble.";

/// Take at most `cap` chars from `s` (char-boundary safe).
fn cap_chars(s: &str, cap: usize) -> String {
    s.chars().take(cap).collect()
}

/// Read depth-1 project docs and summarize them via the secondary model.
///
/// Returns `None` if awareness is disabled, no docs are found, or the model
/// call fails — the caller treats `None` as "no summary" and carries on.
pub async fn summarize(
    client: &OpenRouterClient,
    settings: &Settings,
    workdir: &Path,
) -> Option<String> {
    if !settings.awareness_enabled {
        return None;
    }

    // Gather the docs that exist at depth 1, labelling each with its filename so
    // the model can tell them apart. Each file is capped, and the running corpus
    // is bounded so a giant README can't blow the secondary call's context.
    let mut corpus = String::new();
    for name in DOC_FILES {
        if corpus.len() >= CORPUS_CAP {
            break;
        }
        let path = workdir.join(name);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue; // missing / unreadable / non-UTF8 → skip
        };
        let body = cap_chars(raw.trim(), PER_FILE_CAP);
        if body.trim().is_empty() {
            continue;
        }
        let section = format!("## {name}\n{body}\n\n");
        // Trim the final section to fit the corpus cap rather than overshoot.
        let remaining = CORPUS_CAP - corpus.len();
        corpus.push_str(&cap_chars(&section, remaining));
    }

    let corpus = corpus.trim();
    if corpus.is_empty() {
        return None; // no docs to summarise
    }

    // Pick the model/provider: inherit the session's own, or use the dedicated
    // awareness model. `complete_with` treats an empty provider as default
    // routing, so an inherited empty provider behaves the same as the chat path.
    let (model, provider) = if settings.awareness_inherit {
        (settings.model.as_str(), settings.provider.as_str())
    } else {
        (
            settings.awareness_model.as_str(),
            settings.awareness_provider.as_str(),
        )
    };

    let messages = vec![
        ChatMessage::new(Role::System, SUMMARY_SYSTEM),
        ChatMessage::new(Role::User, corpus),
    ];

    // Best-effort: any error (no key, network, bad provider) → no summary.
    match client.complete_with(model, provider, messages).await {
        Ok(s) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        }
        Err(_) => None,
    }
}
