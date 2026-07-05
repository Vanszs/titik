//! `/model` slash command: change the Main model.
//!
//! - `/model <model-id>` sets the Main model directly.
//! - Bare `/model` opens the provider's live model catalogue picker, prefilled
//!   from the currently resolved Main route.

use std::sync::Arc;

use anyhow::Result;

use crate::app::mode::{KeyInputForm, Mode};
use crate::app::runtime::actions::settings_creds::handle_save_model;
use crate::app::state::AppState;
use crate::config::{DEFAULT_BASE_URL, DEFAULT_MODEL};
use crate::model::app_config::ModelRole;
use crate::service::openrouter::OpenRouterClient;

/// Apply `/model`.
pub(super) fn handle_model(
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
    arg: Option<String>,
) -> Result<()> {
    if let Some(model_id) = arg {
        let model_id = model_id.trim().to_string();
        if model_id.is_empty() {
            state.rest.status = "usage: /model <model-id>".into();
            return Ok(());
        }
        return handle_save_model(model_id, state, client, handle);
    }

    // Bare `/model`: open the model picker prefilled from the active Main route.
    let resolved = state
        .rest
        .fg()
        .session
        .as_ref()
        .and_then(|sess| {
            crate::app::resolve::resolve_role(
                &state.rest.config,
                &sess.settings,
                ModelRole::Main,
            )
        });

    let endpoint = resolved
        .as_ref()
        .map(|r| r.endpoint.clone())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let api_key = resolved.as_ref().map(|r| r.api_key.clone()).unwrap_or_default();
    let current_model = resolved
        .as_ref()
        .map(|r| r.model_id.clone())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    state.mode = Mode::KeyInput(KeyInputForm::for_model_select(endpoint, api_key, current_model));
    state.rest.status = "select a model".into();
    Ok(())
}
