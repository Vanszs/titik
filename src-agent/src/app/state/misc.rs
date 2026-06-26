//! Miscellaneous methods on [`super::AppStateRest`]: credentials, model-catalogue
//! requests, and toast management.

use super::rest::AppStateRest;
use super::types::{CataloguePending, ToastKind};

impl AppStateRest {
    pub fn remember_creds(&mut self, key: &str, model: &str, provider: &str) {
        self.last_key = Some(key.to_string());
        self.last_model = Some(model.to_string());
        self.last_provider = Some(provider.to_string());
    }

    /// Request the model catalogue for `endpoint` (debounced).
    ///
    /// The model omnisearch calls this on every query keystroke / provider change
    /// / field focus for whatever provider is being edited. It is cheap and
    /// idempotent:
    /// - empty `endpoint` → no-op (nothing to fetch against);
    /// - the cache already holds this endpoint (`models_cache_endpoint` matches,
    ///   including the terminal empty-result state) → no-op (filter locally);
    /// - a fetch for this endpoint is already in flight → no-op (don't double-fire);
    /// - otherwise (re)arm a pending fetch ~300 ms out. Calling it again on the
    ///   next keystroke pushes `due` forward, collapsing a typing burst into one
    ///   request fired only once the user pauses.
    ///
    /// The event-loop tick fires the pending fetch once `due` passes; the result
    /// lands via the `warm_rx` drain, which sets `models_cache` +
    /// `models_cache_endpoint` and clears `catalogue_fetching`.
    pub fn request_catalogue(&mut self, endpoint: &str, api_key: &str) {
        if endpoint.is_empty() {
            return;
        }
        if self.models_cache_endpoint.as_deref() == Some(endpoint) {
            return; // already have this endpoint's catalogue (or terminal empty)
        }
        if self.catalogue_fetching.as_deref() == Some(endpoint) {
            return; // already fetching this endpoint
        }
        self.catalogue_pending = Some(CataloguePending {
            endpoint: endpoint.to_string(),
            api_key: api_key.to_string(),
            due: std::time::Instant::now() + std::time::Duration::from_millis(300),
        });
    }

    /// Show an error toast (red box) for ~6 seconds.
    pub fn set_toast(&mut self, msg: String) {
        self.toast = Some((
            msg,
            std::time::Instant::now() + std::time::Duration::from_secs(6),
            ToastKind::Error,
        ));
    }

    /// Show an informational toast (neutral box) for ~8 seconds. Used for
    /// non-failure notices like the post-compaction summary, which is multi-line
    /// and shouldn't read as an error.
    pub fn set_toast_info(&mut self, msg: String) {
        self.toast = Some((
            msg,
            std::time::Instant::now() + std::time::Duration::from_secs(8),
            ToastKind::Info,
        ));
    }

    /// Clear the toast if it has expired. Returns true if it was just cleared
    /// (so the caller can mark the frame dirty).
    pub fn tick_toast(&mut self) -> bool {
        if let Some((_, until, _)) = &self.toast {
            if std::time::Instant::now() >= *until {
                self.toast = None;
                return true;
            }
        }
        false
    }
}
