//! Slash command dispatcher: apply a parsed slash command to app state.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::app::mode::{EffortPickerState, KeyInputForm, Mode, PickerState, SettingsState};
use crate::app::state::AppState;
use crate::config::DEFAULT_MODEL;
use crate::controller::command::Command;
use crate::dto::chat::{ChatMessage, Role};
use crate::model::store;
use crate::service::{openrouter::OpenRouterClient, StreamEvent};

use super::stream::abort_current;

/// Generic effort menu used when the model catalogue can't be fetched (network
/// failure). Covers the common tokens so the user can still set something; the
/// accompanying note tells them capabilities are unknown.
const GENERIC_EFFORTS: &[&str] = &["default", "off", "low", "medium", "high", "max"];

/// Append `opt` to `out` unless it's already present (case-sensitive). Keeps the
/// option list deduped while preserving the order options are added in.
fn push_unique(out: &mut Vec<String>, opt: &str) {
    if !out.iter().any(|o| o == opt) {
        out.push(opt.to_string());
    }
}

/// Build the `/effort` option list from a model's derived [`EffortCaps`].
///
/// Returns `None` when the model has no reasoning control at all (the caller
/// toasts and does NOT open the menu). Otherwise:
/// - discrete efforts reported → `["default","off"] + efforts` (deduped, model
///   order preserved); `"off"` dropped when reasoning is mandatory.
/// - supported but no discrete efforts (on/off only) → `["default","off","max"]`
///   (`"max"` == thinking on); `"off"` dropped when mandatory.
///
/// `"default"` is always first so the model-default choice is one keypress away.
fn build_effort_options(caps: &crate::service::openrouter::EffortCaps) -> Option<Vec<String>> {
    if !caps.supported {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    push_unique(&mut out, "default");
    if !caps.mandatory {
        push_unique(&mut out, "off");
    }
    if caps.efforts.is_empty() {
        // On/off-only model: "max" stands in for "thinking on".
        push_unique(&mut out, "max");
    } else {
        for e in &caps.efforts {
            push_unique(&mut out, e);
        }
    }
    Some(out)
}

/// Index of the option matching the session's stored `effort` (empty → the
/// `"default"` entry). Falls back to 0 when the stored value isn't offered.
fn preselect_effort(options: &[String], effort: &str) -> usize {
    let want = if effort.is_empty() { "default" } else { effort };
    options.iter().position(|o| o == want).unwrap_or(0)
}

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
            // Halt any in-flight agentic loop before swapping sessions, including
            // a half-finished approval machine.
            state.rest.pending_tool_calls.clear();
            state.rest.agent_steps = 0;
            state.rest.awaiting_approval = false;
            state.rest.tool_idx = 0;
            state.rest.tool_results.clear();
            let _ = state.rest.take_stream(); // discard partial; belongs to old session
            let mut sess = match store::create_session() {
                Ok(s) => s,
                Err(e) => {
                    state.rest.status = format!("error: {e}");
                    return Ok(());
                }
            };
            // Inherit the last-used creds so a new session drops straight into
            // chat — no credential prompt. (Change them per-session via /settings.)
            sess.settings.api_key = state.rest.last_key.clone().unwrap_or_default();
            sess.settings.model = state
                .rest
                .last_model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string());
            sess.settings.provider = state.rest.last_provider.clone().unwrap_or_default();
            let _ = sess.save();
            state.rest.prev_session = state.rest.session.take();
            state.rest.reset_scroll();
            if sess.settings.api_key.is_empty() {
                // No creds known yet — fall back to the credential prompt.
                state.rest.session = Some(sess);
                *client = None;
                state.mode = Mode::KeyInput(KeyInputForm::prefilled(
                    String::new(),
                    DEFAULT_MODEL.to_string(),
                    String::new(),
                    false, // Esc -> CancelKeyInput restores prev_session
                    false, // not from picker
                ));
            } else {
                *client = Some(super::build_client(&sess));
                let sess_path = sess.path.clone();
                state.rest.session = Some(sess);
                // Fresh session → totals are 0; calling is harmless and keeps the
                // readout reset when switching sessions.
                state.rest.load_token_totals(&sess_path);
                state.mode = Mode::Chat;
                state.rest.status = "ready".into();
            }
        }

        Command::Mode => {
            state.rest.agent_mode = state.rest.agent_mode.toggled();
            state.rest.status = format!("mode: {}", state.rest.agent_mode.label());
        }

        Command::Effort => {
            // Needs an active session + client (the menu is per-model and a fetch
            // uses the client). Blocked while a request is in flight, mirroring
            // the /settings + /compact guards.
            if state.rest.waiting {
                state.rest.status = "busy — wait for response".into();
                return Ok(());
            }
            let (Some(c), Some(model)) = (
                client.as_ref(),
                state.rest.session.as_ref().map(|s| s.settings.model.clone()),
            ) else {
                state.rest.status = "no active session".into();
                return Ok(());
            };

            // Fetch the model catalogue once and cache it. A network failure
            // leaves the cache `None`, which the option-build step below treats
            // as "capabilities unknown" and falls back to a generic menu.
            if state.rest.models_cache.is_none() {
                if let Ok(models) = handle.block_on(c.list_models()) {
                    state.rest.models_cache = Some(models);
                }
            }

            // Build the option list + capability note from the (cached) catalogue.
            let (options, note) = if let Some(models) = state.rest.models_cache.as_ref() {
                let caps = crate::service::openrouter::effort_caps(models, &model);
                match build_effort_options(&caps) {
                    Some(opts) => {
                        let note = if caps.efforts.is_empty() {
                            "thinking on/off only".to_string()
                        } else if caps.mandatory {
                            "reasoning is always on for this model".to_string()
                        } else {
                            "pick a thinking effort".to_string()
                        };
                        (opts, note)
                    }
                    None => {
                        // No reasoning control: don't open the menu, just say so.
                        state.rest.status = "model has no thinking control".into();
                        return Ok(());
                    }
                }
            } else {
                // Fetch failed (cache still None): generic fallback menu.
                (
                    GENERIC_EFFORTS.iter().map(|s| s.to_string()).collect(),
                    "couldn't fetch model capabilities".to_string(),
                )
            };

            let stored = state
                .rest
                .session
                .as_ref()
                .map(|s| s.settings.effort.clone())
                .unwrap_or_default();
            let selected = preselect_effort(&options, &stored);
            state.mode = Mode::Effort(Box::new(EffortPickerState {
                options,
                selected,
                note,
            }));
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

        Command::Settings => {
            // Needs an active session (drafts seed from it); also blocked while a
            // request is in flight, mirroring the /compact guard.
            if state.rest.waiting {
                state.rest.status = "busy — wait for response".into();
                return Ok(());
            }
            let Some(session) = state.rest.session.as_ref() else {
                state.rest.status = "no active session".into();
                return Ok(());
            };
            let st = SettingsState::from(session, &state.rest.config);
            state.mode = Mode::Settings(Box::new(st));
        }

        Command::Resume => {
            // Open the session picker so the user can switch to a different
            // session.  Unlike CancelKeyInputToPicker we do NOT clear the
            // current session/client — if the user Escapes the picker they
            // return to the active chat unchanged.
            if state.rest.waiting {
                state.rest.status = "busy — wait for response".into();
                return Ok(());
            }
            match store::list_sessions() {
                Ok(sessions) => {
                    state.mode = Mode::SessionPicker(PickerState::new(sessions));
                }
                Err(e) => {
                    state.rest.status = format!("error listing sessions: {e}");
                }
            }
        }

        Command::Select => {
            if state.rest.waiting {
                state.rest.status = "busy — wait for response".into();
                return Ok(());
            }
            if state.rest.session.is_none() {
                state.rest.status = "no active session".into();
                return Ok(());
            }
            state.rest.select_pending = true;
        }

        Command::Help => {
            state.rest.help_open = true;
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
