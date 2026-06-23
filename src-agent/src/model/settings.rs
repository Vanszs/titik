//! Per-session configuration persisted to `settings.json`.
//!
//! Each session directory contains exactly one `settings.json` that is read
//! on `Session::load` and written on every `Session::save`. The file is
//! human-editable — users can change the model or tweak compaction settings
//! without touching the TUI.
//!
//! **API key storage:** the key is stored per-session by design. This lets
//! users run separate sessions against different OpenRouter accounts (e.g. a
//! personal key vs. a work key) without a global config file. An empty string
//! means "not configured"; the UI will prompt before the first send.
//!
//! On-disk format (pretty-printed JSON):
//! ```json
//! {
//!   "api_key": "sk-or-...",
//!   "model": "openai/gpt-4o",
//!   "name": "my-session",
//!   "compaction": { "preserve_n": 20 }
//! }
//! ```

use std::path::Path;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use crate::config::{
    DEFAULT_AWARENESS_MODEL, DEFAULT_AWARENESS_PROVIDER, DEFAULT_CLASSIFIER_MODEL,
    DEFAULT_CLASSIFIER_PROVIDER, DEFAULT_MODEL, DEFAULT_PRESERVE_N, DEFAULT_PROVIDER,
};

/// Controls conversation compaction behaviour (the `/compact` command).
///
/// When the history grows long, the oldest messages are summarised and
/// replaced. `preserve_n` determines how many of the most-recent messages are
/// kept verbatim after compaction (the "tail" that stays in full detail).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    #[serde(default = "default_preserve_n")]
    pub preserve_n: usize,
}

fn default_preserve_n() -> usize {
    DEFAULT_PRESERVE_N
}

impl Default for Compaction {
    fn default() -> Self {
        Self {
            preserve_n: DEFAULT_PRESERVE_N,
        }
    }
}

/// Per-session user-configurable settings.
///
/// Deserialized from (and serialized to) `<session_dir>/settings.json`.
/// All fields have serde defaults so a partially-written or newly-created
/// file deserialises without error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    // api_key is intentionally ALWAYS serialised (no skip_serializing_if),
    // even when empty, so the on-disk round-trip is unambiguous — an absent
    // key in JSON would deserialise to "" via the `default` attribute, but
    // re-serialising would then omit it, creating a surprising diff.
    #[serde(default)]
    pub api_key: String,
    /// OpenRouter model identifier, e.g. `"openai/gpt-4o"`.
    #[serde(default = "default_model")]
    pub model: String,
    /// Human-readable session name (also used as the directory slug after
    /// `rename_session` normalises it).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub compaction: Compaction,
    /// OpenRouter provider slug to strict-pin (e.g. `"anthropic"`, `"together"`).
    /// Empty string means use OpenRouter default routing; the `provider` field is
    /// then omitted from the request body entirely.
    #[serde(default)]
    pub provider: String,
    /// Working directory for this session. Defaults to the process's cwd at
    /// session creation time. Used to locate AGENT.md / AGENTS.md.
    #[serde(default)]
    pub workdir: String,
    /// Whether the project-awareness summary is generated and injected into the
    /// system prompt. When false, no secondary-model call is made.
    #[serde(default = "default_awareness_enabled")]
    pub awareness_enabled: bool,
    /// Awareness model source. `false` (default) uses the dedicated
    /// `awareness_model` / `awareness_provider` below; `true` reuses this
    /// session's own `model` / `provider` for the summary call.
    #[serde(default = "default_awareness_inherit")]
    pub awareness_inherit: bool,
    /// Dedicated model for the awareness summary when `awareness_inherit` is
    /// false. A small/cheap model is plenty for a few-sentence summary.
    #[serde(default = "default_awareness_model")]
    pub awareness_model: String,
    /// Dedicated provider slug (strict-pinned) for the awareness summary when
    /// `awareness_inherit` is false. Empty means OpenRouter default routing.
    #[serde(default = "default_awareness_provider")]
    pub awareness_provider: String,
    /// Master switch for the safety harness ("Pass B"). When false (the
    /// default), the agentic loop behaves EXACTLY as it did before the harness
    /// existed: no workspace check, no prompt/tool-call classification, no
    /// secondary-model calls. Opt-in only.
    #[serde(default = "default_classifier_enabled")]
    pub classifier_enabled: bool,
    /// Model used for the safety classifier (prompt + tool-call verdicts).
    /// A dedicated safeguard model judges whether a request/call is safe.
    #[serde(default = "default_classifier_model")]
    pub classifier_model: String,
    /// Provider slug (strict-pinned) for the classifier call. Empty means
    /// OpenRouter default routing.
    #[serde(default = "default_classifier_provider")]
    pub classifier_provider: String,
    /// Extra folders the session is allowed to operate in, beyond the launch
    /// directory (which is always allowed at runtime). The workspace check (WC)
    /// passes when the session workdir is the launch dir OR appears here. Empty
    /// by default; ignored entirely when `classifier_enabled` is false.
    #[serde(default)]
    pub allowed_folders: Vec<String>,
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

fn default_awareness_enabled() -> bool {
    true
}

fn default_awareness_inherit() -> bool {
    false
}

fn default_awareness_model() -> String {
    DEFAULT_AWARENESS_MODEL.to_string()
}

fn default_awareness_provider() -> String {
    DEFAULT_AWARENESS_PROVIDER.to_string()
}

fn default_classifier_enabled() -> bool {
    false
}

fn default_classifier_model() -> String {
    DEFAULT_CLASSIFIER_MODEL.to_string()
}

fn default_classifier_provider() -> String {
    DEFAULT_CLASSIFIER_PROVIDER.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: DEFAULT_MODEL.to_string(),
            name: String::new(),
            compaction: Compaction::default(),
            provider: DEFAULT_PROVIDER.to_string(),
            workdir: String::new(),
            awareness_enabled: default_awareness_enabled(),
            awareness_inherit: default_awareness_inherit(),
            awareness_model: DEFAULT_AWARENESS_MODEL.to_string(),
            awareness_provider: DEFAULT_AWARENESS_PROVIDER.to_string(),
            classifier_enabled: default_classifier_enabled(),
            classifier_model: DEFAULT_CLASSIFIER_MODEL.to_string(),
            classifier_provider: DEFAULT_CLASSIFIER_PROVIDER.to_string(),
            allowed_folders: Vec::new(),
        }
    }
}

impl Settings {
    /// Deserialise from a `settings.json` file at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Serialise (pretty-printed) to `path`, creating or overwriting the file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}
