//! Persistent on-disk cache for the OpenRouter model catalogue.
//!
//! The catalogue (`GET /models`) is fetched on every launch today; this module
//! adds a disk-backed cache so a fresh cache makes startup instant and a stale
//! one is served immediately while a background refresh runs.
//!
//! Cache file location: `~/.simple-coder/catalogue/<sanitized_endpoint>.json`
//!
//! The JSON envelope is:
//! ```json
//! { "fetched_at": 1719000000, "models": [ ... ] }
//! ```
//!
//! TTL is [`CATALOGUE_TTL_SECS`] (24 h). Reads and writes are best-effort —
//! all errors are silently swallowed; the cache is advisory only.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::dto::openrouter::ModelInfo;
use crate::model::store::base_dir;

/// How long a cached catalogue is considered fresh (24 hours).
pub const CATALOGUE_TTL_SECS: u64 = 24 * 3600;

/// On-disk envelope written to / read from `catalogue/<endpoint>.json`.
#[derive(Serialize, Deserialize)]
struct CatalogueFile {
    /// Unix timestamp (seconds) when this snapshot was fetched.
    fetched_at: u64,
    models: Vec<ModelInfo>,
}

/// Current time as Unix seconds. Falls back to 0 on the rare clock error.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Map `endpoint` to a filename-safe stem by replacing every non-alphanumeric
/// ASCII character with `_`.
///
/// Example: `"https://openrouter.ai/api/v1"` → `"https___openrouter_ai_api_v1"`.
fn sanitize(endpoint: &str) -> String {
    endpoint
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

/// Absolute path to the cache file for `endpoint`, or `None` when the home
/// directory cannot be resolved.
fn cache_path(endpoint: &str) -> Option<PathBuf> {
    Some(base_dir().ok()?.join("catalogue").join(format!("{}.json", sanitize(endpoint))))
}

/// Whether `age_secs` is below the TTL threshold (i.e. the entry is still fresh).
pub fn is_fresh(age_secs: u64) -> bool {
    age_secs < CATALOGUE_TTL_SECS
}

/// Try to load the cached catalogue for `endpoint`.
///
/// Returns `Some((models, age_secs))` on success, where `age_secs` is how many
/// seconds have elapsed since the snapshot was fetched. Returns `None` on any
/// error (file missing, parse failure, clock error, etc.).
pub fn load(endpoint: &str) -> Option<(Vec<ModelInfo>, u64)> {
    let path = cache_path(endpoint)?;
    let bytes = std::fs::read(&path).ok()?;
    let file: CatalogueFile = serde_json::from_slice(&bytes).ok()?;
    let now = now_secs();
    let age = now.saturating_sub(file.fetched_at);
    Some((file.models, age))
}

/// Persist `models` to the cache file for `endpoint`.
///
/// Best-effort: creates the `catalogue/` directory if needed, then writes
/// pretty-printed JSON. All errors are silently ignored — never panics, never
/// blocks the caller meaningfully.
pub fn save(endpoint: &str, models: &[ModelInfo]) {
    let Some(path) = cache_path(endpoint) else { return };
    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let file = CatalogueFile {
        fetched_at: now_secs(),
        models: models.to_vec(),
    };
    if let Ok(json) = serde_json::to_vec_pretty(&file) {
        let _ = std::fs::write(&path, json);
    }
}
