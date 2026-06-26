//! Slash command dispatcher: apply a parsed slash command to app state.

use std::sync::Arc;

use anyhow::Result;

use crate::app::state::AppState;
use crate::controller::command::Command;
use crate::service::openrouter::OpenRouterClient;

mod compact;
mod effort;
mod internet;
mod misc;
mod new_session;
mod task;

/// Apply a parsed slash command. Like [`apply_action`], it mutates state and
/// may spawn/abort the request task.
pub(super) fn apply_slash(
    cmd: Command,
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    match cmd {
        Command::Compact => compact::handle_compact(state, client, handle)?,
        Command::New => new_session::handle_new(state, client, handle)?,
        Command::Mode => misc::handle_mode(state)?,
        Command::Effort => effort::handle_effort(state, client)?,
        Command::Rename(name) => new_session::handle_rename(state, name)?,
        Command::Settings => misc::handle_settings(state)?,
        Command::Agents => misc::handle_agents(state)?,
        Command::Resume => new_session::handle_resume(state)?,
        Command::Select => misc::handle_select(state)?,
        Command::Help => misc::handle_help(state)?,
        Command::Quit => misc::handle_quit(state)?,
        Command::Task(args) => task::handle_task(args, state, client, handle)?,
        Command::Internet(target) => internet::handle_internet(target, state)?,
        Command::Unknown(s) => {
            state.rest.status = format!("unknown command: /{s}");
        }
    }
    Ok(())
}
