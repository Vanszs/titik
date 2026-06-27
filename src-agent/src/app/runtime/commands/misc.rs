//! Miscellaneous simple commands: `/mode`, `/settings`, `/agents`, `/select`,
//! `/help`, `/quit`, `/usage`.

use anyhow::Result;

use crate::app::mode::{AgentsState, Mode, SettingsState};
use crate::app::state::AppState;

use super::super::stream::abort_current;

/// Handle the `/mode` command: toggle chat ↔ agentic mode.
pub(super) fn handle_mode(state: &mut AppState) -> Result<()> {
    state.rest.agent_mode = state.rest.agent_mode.toggled();
    state.rest.status = format!("mode: {}", state.rest.agent_mode.label());
    Ok(())
}

/// Handle the `/settings` command: open the settings modal.
///
/// Needs an active session (drafts seed from it); also blocked while a
/// request is in flight, mirroring the /compact guard.
pub(super) fn handle_settings(state: &mut AppState) -> Result<()> {
    if state.rest.waiting {
        state.rest.status = "busy — wait for response".into();
        return Ok(());
    }
    // No catalogue prefetch here anymore: the Models Select modal's
    // omnisearch fetches the EDITED provider's `/models` on demand
    // (debounced) the first time it opens / a key is typed — see
    // `controller::input::handle_settings`. A boot prefetch keyed to the
    // Main endpoint was wrong for editing a model on a DIFFERENT provider.
    let Some(session) = state.rest.session.as_ref() else {
        state.rest.status = "no active session".into();
        return Ok(());
    };
    let st = SettingsState::from(session, &state.rest.config);
    state.mode = Mode::Settings(Box::new(st));
    Ok(())
}

/// Handle the `/agents` command: open the agents modal.
///
/// Needs an active session (the registry loads from it); also blocked
/// while a request is in flight, mirroring the /settings + /compact guards.
pub(super) fn handle_agents(state: &mut AppState) -> Result<()> {
    if state.rest.waiting {
        state.rest.status = "busy — wait for response".into();
        return Ok(());
    }
    let Some(session) = state.rest.session.as_ref() else {
        state.rest.status = "no active session".into();
        return Ok(());
    };
    let st = AgentsState::from(session);
    state.mode = Mode::Agents(Box::new(st));
    Ok(())
}

/// Handle the `/select` command: arm text-selection mode.
pub(super) fn handle_select(state: &mut AppState) -> Result<()> {
    if state.rest.waiting {
        state.rest.status = "busy — wait for response".into();
        return Ok(());
    }
    if state.rest.session.is_none() {
        state.rest.status = "no active session".into();
        return Ok(());
    }
    state.rest.select_pending = true;
    Ok(())
}

/// Handle the `/help` command: open the help overlay.
pub(super) fn handle_help(state: &mut AppState) -> Result<()> {
    state.rest.help_open = true;
    Ok(())
}

/// Handle the `/quit` command: abort any in-flight request and exit.
pub(super) fn handle_quit(state: &mut AppState) -> Result<()> {
    if state.rest.waiting {
        abort_current(&mut state.rest);
    }
    state.rest.should_quit = true;
    Ok(())
}

/// Handle the `/usage` command: open the cost/token usage dashboard.
///
/// Read-only; no waiting guard needed (the dashboard never writes).
pub(super) fn handle_usage(state: &mut AppState) -> Result<()> {
    state.mode = Mode::Usage(Box::default());
    Ok(())
}
