//! Global application configuration persisted to `~/.simple-coder/config.json`.
//!
//! Unlike per-session `settings.json`, this file stores user-wide preferences
//! that apply across all sessions: visual theme, accent colour, and any future
//! global knobs. It is loaded once at startup (after `ensure_dirs`) and never
//! written automatically — the user (or a future `/settings` command) calls
//! `save()` explicitly.
//!
//! On-disk format (pretty-printed JSON):
//! ```json
//! {
//!   "theme": "dark",
//!   "accent": "green"
//! }
//! ```
//!
//! Unknown keys are silently ignored (forward-compat); missing keys fall back
//! to defaults (back-compat). Any read error — file absent, parse failure,
//! permission denied — returns `AppConfig::default()` instead of propagating,
//! so a corrupt or missing config never prevents startup.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use crate::model::store::base_dir;

/// Visual colour scheme.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

fn default_accent() -> String {
    "green".to_string()
}

/// Mint a fresh random UUID (v4) as a `String`. Used as the serde default for
/// the `uuid` field of [`ProviderConn`] / [`ModelEntry`] so entries read from an
/// old config file without a uuid get a stable identity on load, and so new
/// entries can be minted in Rust without a hand-rolled scheme.
fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Wire protocol an API provider connection speaks. Mirrors the UI-side
/// `ApiType`; this is the persisted form (serde snake_case).
///
/// `OpenAiCompatible` is the default — the OpenRouter/OpenAI chat-completions
/// wire is what the runtime currently speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiType {
    #[default]
    OpenAiCompatible,
    AnthropicCompatible,
}

/// Runtime role slot a model is assigned to. Exclusive (1:1 role→model) by
/// convention; persisted in lowercase (`"main"`, `"awareness"`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelRole {
    Main,
    Awareness,
    Safeguard,
    Compactor,
}

/// One API provider connection: a base URL + auth + wire type, keyed by `uuid`.
///
/// Every field carries `#[serde(default)]` so a partially-written or
/// older-schema config loads cleanly; `uuid` defaults to a freshly minted v4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConn {
    #[serde(default = "new_uuid")]
    pub uuid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub api_type: ApiType,
    /// Base URL, e.g. `https://openrouter.ai/api/v1`.
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub api_key: String,
}

/// One model entry in the global catalogue. References its serving provider by
/// `provider_uuid`. `route` pins an OpenRouter upstream provider name; `role`
/// assigns the runtime slot (`None` = unassigned).
///
/// Every field carries `#[serde(default)]`; `uuid` defaults to a freshly minted
/// v4 and the two `Option` fields are omitted from the JSON when `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    #[serde(default = "new_uuid")]
    pub uuid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub provider_uuid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ModelRole>,
}

/// Global user-facing configuration (theme + accent + provider/model catalogue).
///
/// All fields carry `#[serde(default)]` so the struct round-trips cleanly
/// when the on-disk file was written by an older version that lacked a field,
/// or when the file is absent entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub theme: ThemeMode,
    #[serde(default = "default_accent")]
    pub accent: String,
    /// Global catalogue of API provider connections, keyed by uuid.
    #[serde(default)]
    pub providers: Vec<ProviderConn>,
    /// Global catalogue of named models; each references a provider by uuid.
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: ThemeMode::default(),
            accent: default_accent(),
            providers: Vec::new(),
            models: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Load from `~/.simple-coder/config.json`.
    ///
    /// Returns `AppConfig::default()` on ANY error (file absent, parse failure,
    /// etc.) so startup is never blocked by a missing or corrupt config file.
    pub fn load() -> Self {
        let path = match base_dir() {
            Ok(d) => d.join("config.json"),
            Err(_) => return AppConfig::default(),
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => return AppConfig::default(),
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Index of the provider whose `uuid` matches, if any. Used by the
    /// `/settings` load/save mapping to resolve a [`ModelEntry::provider_uuid`]
    /// back to the UI draft's positional `provider_idx`.
    pub fn provider_index_by_uuid(&self, uuid: &str) -> Option<usize> {
        self.providers.iter().position(|p| p.uuid == uuid)
    }

    /// Serialise (pretty-printed) to `~/.simple-coder/config.json`.
    ///
    /// Called by the `/settings` dashboard when the user saves theme/accent
    /// changes.
    pub fn save(&self) -> Result<()> {
        let path = base_dir()?.join("config.json");
        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}
