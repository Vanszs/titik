//! Action dispatcher: apply a decoded keystroke action to app state.
//!
//! The module is split into focused submodules by concern:
//! - [`chat`]     — Submit, Interrupt, Resend, ApproveTool, DenyTool
//! - [`settings`] — SaveCreds, SaveSettings, SaveEffort, EffortCancel, FetchModelEndpoints
//! - [`session`]  — CancelKeyInput, CancelKeyInputToPicker, CancelPickerToChat, PickerSelect, SkipLoading
//! - [`agents`]   — CreateAgent, SaveAgent, DeleteAgent, CloseAgents

use std::sync::Arc;

use anyhow::Result;

use crate::app::state::AppState;
use crate::controller::input::Action;
use crate::service::openrouter::OpenRouterClient;

mod agents;
mod chat;
mod rewind;
mod session;
mod settings;
mod settings_creds;

/// Apply one `Action` (the decoded result of a keystroke) by mutating state and,
/// where needed, spawning/aborting the request task.
pub(super) fn apply_action(
    action: Action,
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    match action {
        Action::None => {}

        Action::Quit => {
            if state.rest.fg().waiting {
                crate::app::runtime::stream::abort_current(&mut state.rest);
            }
            state.rest.should_quit = true;
        }

        Action::Submit(text) => {
            chat::handle_submit(text, state, client, handle)?;
        }

        Action::Slash(cmd) => {
            super::commands::apply_slash(cmd, state, client, handle)?;
        }

        Action::Interrupt => {
            chat::handle_interrupt(state)?;
        }

        Action::Resend => {
            chat::handle_resend(state, client, handle)?;
        }

        Action::ApproveTool => {
            chat::handle_approve_tool(state, client, handle)?;
        }

        Action::DenyTool => {
            chat::handle_deny_tool(state)?;
        }

        Action::SaveCreds { endpoint, api_key, model } => {
            settings_creds::handle_save_creds(endpoint, api_key, model, state, client, handle)?;
        }

        Action::CancelKeyInput => {
            session::handle_cancel_key_input(state, client)?;
        }

        Action::CancelKeyInputToPicker => {
            session::handle_cancel_key_input_to_picker(state, client)?;
        }

        Action::CancelPickerToChat => {
            session::handle_cancel_picker_to_chat(state)?;
        }

        Action::PickerSelect => {
            session::handle_picker_select(state, client, handle)?;
        }

        Action::SaveSettings => {
            settings::handle_save_settings(state)?;
        }

        Action::SaveEffort(choice) => {
            settings::handle_save_effort(choice, state)?;
        }

        Action::EffortCancel => {
            state.mode = crate::app::mode::Mode::Chat;
        }

        Action::CreateAgent => {
            agents::handle_create_agent(state)?;
        }

        Action::SaveAgent => {
            agents::handle_save_agent(state)?;
        }

        Action::DeleteAgent => {
            agents::handle_delete_agent(state)?;
        }

        Action::CloseAgents => {
            agents::handle_close_agents(state)?;
        }

        Action::FetchModelEndpoints(model_id) => {
            settings::handle_fetch_model_endpoints(model_id, state, client, handle)?;
        }

        Action::SkipLoading => {
            session::handle_skip_loading(state)?;
        }

        Action::CloseUsage => {
            state.mode = crate::app::mode::Mode::Chat;
            state.rest.status = "ready".into();
        }

        Action::OpenRewind => {
            rewind::handle_open_rewind(state)?;
        }

        Action::RewindCancel => {
            rewind::handle_rewind_cancel(state)?;
        }

        Action::RewindToMessage(idx) => {
            rewind::handle_rewind_to_message(idx, state)?;
        }
    }
    Ok(())
}
