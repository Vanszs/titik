//! Action handlers for the message-rewind picker (double-Esc "edit a previous
//! message"): open the picker, cancel it, and select an entry.

use anyhow::Result;

use crate::app::mode::{Mode, RewindState};
use crate::app::state::AppState;

/// Handle `Action::OpenRewind`: build the picker from the active conversation's
/// user messages and swap into `Mode::MessageRewind`.
///
/// A no-op (status note only) when there is no active session or the
/// conversation has no prior user message — there is nothing to rewind to.
pub(super) fn handle_open_rewind(state: &mut AppState) -> Result<()> {
    let rewind = state
        .rest
        .session
        .as_ref()
        .and_then(|s| RewindState::from_messages(s.conversation.messages()));
    match rewind {
        Some(rw) => {
            state.mode = Mode::MessageRewind(Box::new(rw));
        }
        None => {
            state.rest.status = "nothing to edit".into();
        }
    }
    Ok(())
}

/// Handle `Action::RewindCancel`: discard the picker and return to Chat. The
/// conversation is untouched, so just swap the mode back.
pub(super) fn handle_rewind_cancel(state: &mut AppState) -> Result<()> {
    state.mode = Mode::Chat;
    Ok(())
}

/// Handle `Action::RewindSelect`: rewind the conversation to just before the
/// highlighted user message and load its text into the composer.
///
/// Stage 1 placeholder: the truncation + composer-load is wired in stage 2. For
/// now, return to Chat unchanged so the picker stays usable and compiles.
pub(super) fn handle_rewind_select(state: &mut AppState) -> Result<()> {
    state.mode = Mode::Chat;
    Ok(())
}
