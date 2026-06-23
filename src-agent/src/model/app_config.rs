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

/// Global user-facing configuration (theme + accent).
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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: ThemeMode::default(),
            accent: default_accent(),
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
