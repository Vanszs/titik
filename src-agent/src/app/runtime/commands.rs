//! Slash command dispatcher: apply a parsed slash command to app state.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::app::mode::{KeyInputForm, Mode};
use crate::app::state::AppState;
use crate::config::DEFAULT_MODEL;
use crate::controller::command::Command;
use crate::dto::chat::{ChatMessage, Role};
use crate::model::store;
use crate::service::{openrouter::OpenRouterClient, StreamEvent};

use super::stream::abort_current;

/// Apply a parsed slash command. Like [`apply_action`], it mutates state and
/// may spawn/abort the request task.
pub(super) fn apply_slash(
    cmd: Command,
    state: &mut AppState,
    client: &mut Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) -> Result<()> {
    match cmd {
        Command::Compact => {
            if state.rest.waiting {
                state.rest.status = "busy — wait for response".into();
                return Ok(());
            }
            if client.is_none() || state.rest.session.is_none() {
                state.rest.status = "no active session".into();
                return Ok(());
            }
            let (to_sum, kept_tail) = {
                let sess = state.rest.session.as_ref().unwrap();
                let pn = sess.settings.compaction.preserve_n;
                sess.conversation.split_for_compaction(pn)
            };
            if to_sum.is_empty() {
                state.rest.status = "nothing to compact".into();
                return Ok(());
            }
            let mut req = vec![ChatMessage::new(
                Role::System,
                "Summarise the following conversation concisely, preserving key facts, decisions, and context.",
            )];
            req.extend(to_sum);
            state.rest.waiting = true;
            state.rest.status = "compacting...".into();
            // Fresh channel for this request; the receiver lives in state so an
            // interrupt/new just drops it and the task's result is ignored.
            let (tx, rx) = mpsc::unbounded_channel();
            state.rest.active_rx = Some(rx);
            let c = Arc::clone(client.as_ref().unwrap());
            let jh = handle.spawn(async move {
                let event = match c.complete(req).await {
                    Ok(s) => StreamEvent::Compacted {
                        summary: s,
                        kept_tail,
                    },
                    Err(e) => StreamEvent::Error(e.to_string()),
                };
                let _ = tx.send(event);
            });
            state.rest.current_task = Some(jh.abort_handle());
        }

        Command::New => {
            abort_current(&mut state.rest);
            let _ = state.rest.take_stream(); // discard partial; belongs to old session
            let sess = match store::create_session() {
                Ok(s) => s,
                Err(e) => {
                    state.rest.status = format!("error: {e}");
                    return Ok(());
                }
            };
            state.rest.prev_session = state.rest.session.take();
            state.rest.session = Some(sess);
            *client = None; // forces SaveCreds rebuild
            state.rest.reset_scroll();
            state.mode = Mode::KeyInput(KeyInputForm::prefilled(
                state.rest.last_key.clone().unwrap_or_default(),
                state
                    .rest
                    .last_model
                    .clone()
                    .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
                state.rest.last_provider.clone().unwrap_or_default(),
                false, // Esc -> CancelKeyInput restores prev_session
                false, // not from picker
            ));
        }

        Command::Rename(name) => {
            if name.trim().is_empty() {
                state.rest.status = "usage: /rename <name>".into();
                return Ok(());
            }
            if let Some(sess) = state.rest.session.as_mut() {
                match store::rename_session(sess, &name) {
                    Ok(()) => state.rest.status = format!("renamed to {}", sess.name),
                    Err(e) => state.rest.status = format!("error: {e}"),
                }
            }
        }

        Command::Help => {
            state.rest.status =
                "/compact /new /rename <name> /help /quit · Ctrl+R resend · Esc interrupt".into();
        }

        Command::Quit => {
            if state.rest.waiting {
                abort_current(&mut state.rest);
            }
            state.rest.should_quit = true;
        }

        Command::Unknown(s) => {
            state.rest.status = format!("unknown command: /{s}");
        }
    }
    Ok(())
}
