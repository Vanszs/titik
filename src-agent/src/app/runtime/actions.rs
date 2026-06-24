//! Action dispatcher: apply a decoded keystroke action to app state.

use std::sync::Arc;

use anyhow::Result;

use crate::app::mode::{KeyInputForm, Mode, PickerState};
use crate::app::state::AppState;
use crate::config::DEFAULT_MODEL;
use crate::controller::input::Action;
use crate::dto::chat::Role;
use crate::model::{msglog, session::Session, store};
use crate::service::openrouter::OpenRouterClient;

use super::build_client;
use super::commands::apply_slash;
use super::stream::{abort_current, process_tools, run_tool, start_stream_task};

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
            if state.rest.waiting {
                abort_current(&mut state.rest);
            }
            state.rest.should_quit = true;
        }

        Action::Submit(text) => {
            if client.is_none() || state.rest.session.is_none() {
                state.rest.status = "no active session".into();
                return Ok(());
            }
            if state.rest.waiting {
                state.rest.status = "busy — wait for response".into();
                return Ok(());
            }
            // Prompt-classifier (PC): keep a copy of the user's prompt to
            // classify in the background once the turn is kicked off.
            let pc_prompt = text.clone();
            let history = {
                let sess = state.rest.session.as_mut().unwrap();
                let _ = msglog::append(&sess.path, Role::User, &text, None);
                sess.conversation.push_user(text);
                if let Err(e) = sess.save() {
                    state.rest.status = format!("error: {e}");
                    return Ok(());
                }
                sess.conversation.history()
            };
            state.rest.reset_scroll();
            state.rest.begin_stream();
            state.rest.waiting = true;
            // A new user turn starts fresh: no carried-over tool-call rounds or
            // a half-finished approval machine.
            state.rest.agent_steps = 0;
            state.rest.pending_tool_calls.clear();
            state.rest.awaiting_approval = false;
            state.rest.tool_idx = 0;
            state.rest.tool_results.clear();
            // Every new user turn must plan before running tools.
            state.rest.needs_plan = true;
            state.rest.status = "thinking...".into();
            start_stream_task(history, state, client, handle);

            // Prompt-classifier (PC), advisory + non-blocking: once per turn, if
            // the harness is enabled, classify the user prompt on a background
            // task. It sends one HarnessVerdict on a dedicated channel (drained
            // in run_loop) — it NEVER gates the stream that just started. Drop
            // any stale receiver from a prior turn first.
            state.rest.harness_rx = None;
            let pc_inputs = match (client.as_ref(), state.rest.session.as_ref()) {
                (Some(c), Some(sess)) if sess.settings.classifier_enabled => {
                    Some((Arc::clone(c), sess.settings.clone()))
                }
                _ => None,
            };
            if let Some((c, settings)) = pc_inputs {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                state.rest.harness_rx = Some(rx);
                handle.spawn(async move {
                    let v = crate::app::harness::classify_prompt(&c, &settings, &pc_prompt).await;
                    // A dropped receiver (turn superseded / app closing) makes
                    // this a no-op — same contract as the streaming channel.
                    let _ = tx.send(crate::service::StreamEvent::HarnessVerdict {
                        allow: v.allow,
                        reason: v.reason,
                    });
                });
            }
        }

        Action::Slash(cmd) => {
            apply_slash(cmd, state, client, handle)?;
        }

        Action::Interrupt => {
            // Custom finalization (not finish_stream): the partial buffer is
            // committed with an "  [interrupted]" marker. abort_current drops
            // active_rx, so the aborted task's late events are ignored.
            if state.rest.waiting {
                abort_current(&mut state.rest);
                // Halt the agentic loop: drop any stashed tool calls, reset the
                // step counter, and clear the approval machine so a halt mid-
                // approval doesn't leave the turn wedged.
                state.rest.pending_tool_calls.clear();
                state.rest.agent_steps = 0;
                state.rest.awaiting_approval = false;
                state.rest.approval_reason = None;
                state.rest.tool_idx = 0;
                state.rest.tool_results.clear();
                // Take any captured usage unconditionally so a partial turn's
                // usage can't leak into the next response.
                let usage = state.rest.pending_usage.take();
                let buf = state.rest.take_stream();
                if let Some(b) = buf {
                    if !b.is_empty() {
                        if let Some(sess) = state.rest.session.as_mut() {
                            let content = format!("{b}  [interrupted]");
                            let _ = msglog::append(&sess.path, Role::Assistant, &content, usage);
                            sess.conversation.push_assistant(content);
                            let _ = sess.save();
                            if let Some((pt, ct, cost)) = usage {
                                state.rest.tokens_in = pt;        // current context size, not a sum
                                state.rest.tokens_out += ct;
                                state.rest.cost += cost;
                            }
                        }
                    }
                }
            }
            state.rest.status = "interrupted".into();
        }

        Action::Resend => {
            if state.rest.waiting {
                state.rest.status = "busy — wait for response".into();
                return Ok(());
            }
            if client.is_none() || state.rest.session.is_none() {
                state.rest.status = "no active session".into();
                return Ok(());
            }
            let history = {
                let sess = state.rest.session.as_mut().unwrap();
                if sess.conversation.last_user_content().is_none() {
                    state.rest.status = "nothing to resend".into();
                    return Ok(());
                }
                sess.conversation.pop_trailing_assistants();
                let _ = sess.save();
                sess.conversation.history()
            };
            state.rest.reset_scroll();
            state.rest.begin_stream();
            state.rest.waiting = true;
            state.rest.status = "thinking...".into();
            start_stream_task(history, state, client, handle);
        }

        Action::ApproveTool => {
            // Run the paused risky call, record its result, advance past it, then
            // resume the machine (which may pause again on the next risky call or
            // finish the round). Clone the call out first so `run_tool`'s mutable
            // borrow of `state` doesn't overlap the `pending_tool_calls` read.
            state.rest.awaiting_approval = false;
            state.rest.approval_reason = None;
            if let Some(call) = state.rest.pending_tool_calls.get(state.rest.tool_idx).cloned() {
                let result = run_tool(state, &call);
                state.rest.tool_results.push((call.id.clone(), result));
                state.rest.tool_idx += 1;
            }
            process_tools(state, client, handle);
        }

        Action::DenyTool => {
            state.rest.awaiting_approval = false;
            state.rest.approval_reason = None;
            // Denial halts the turn. Answer the denied call AND every remaining
            // pending call with "denied by user" (so the conversation stays
            // API-valid: every tool_call gets a result), commit any results
            // already collected this round, then STOP — do not re-stream.
            let results = state.rest.tool_results.clone();
            let denied_ids: Vec<String> = state
                .rest
                .pending_tool_calls
                .iter()
                .skip(state.rest.tool_idx)
                .map(|c| c.id.clone())
                .collect();
            if let Some(sess) = state.rest.session.as_mut() {
                for (id, result) in &results {
                    let _ = msglog::append(
                        &sess.path,
                        Role::Tool,
                        result,
                        None,
                    );
                    sess.conversation.push_tool(id.clone(), result.clone());
                }
                for id in &denied_ids {
                    let _ = msglog::append(
                        &sess.path,
                        Role::Tool,
                        "denied by user",
                        None,
                    );
                    sess.conversation.push_tool(id.clone(), "denied by user".to_string());
                }
                let _ = sess.save();
            }
            // Reset the agentic-loop state and end the turn.
            state.rest.pending_tool_calls.clear();
            state.rest.tool_idx = 0;
            state.rest.tool_results.clear();
            state.rest.agent_steps = 0;
            state.rest.waiting = false;
            state.rest.current_task = None;
            state.rest.status = "denied — stopped".into();
        }

        Action::SaveCreds { api_key, model, provider } => {
            let model = if model.is_empty() {
                DEFAULT_MODEL.to_string()
            } else {
                model
            };
            // Lazy creation: first-run path has no session yet. Create it now,
            // then apply the entered credentials.
            if state.rest.session.is_none() {
                match store::create_session() {
                    Ok(s) => state.rest.session = Some(s),
                    Err(e) => {
                        state.rest.status = format!("error: {e}");
                        return Ok(());
                    }
                }
            }
            if let Some(sess) = state.rest.session.as_mut() {
                sess.settings.api_key = api_key.clone();
                sess.settings.model = model.clone();
                sess.settings.provider = provider.clone();
                let _ = sess.save();
            }
            state.rest.remember_creds(&api_key, &model, &provider);
            *client = state.rest.session.as_ref().map(build_client);
            // Warm the confirmed session: reindex its workspace and compute the
            // awareness summary so a creds-confirmed session is fully primed like
            // a cold boot.
            super::warm_session(state, client, handle);
            // Seed totals from the (new or picker-prefilled) session's log.
            if let Some(p) = state.rest.session.as_ref().map(|s| s.path.clone()) {
                state.rest.load_token_totals(&p);
            }
            state.rest.prev_session = None; // committed; discard fallback
            state.rest.reset_scroll();
            state.mode = Mode::Chat;
            state.rest.status = "ready".into();
        }

        Action::CancelKeyInput => {
            if let Some(prev) = state.rest.prev_session.take() {
                *client = if prev.settings.api_key.is_empty() {
                    None
                } else {
                    Some(build_client(&prev))
                };
                state.rest.session = Some(prev);
            } else if let Some(sess) = state.rest.session.as_ref() {
                // Defensive: no stashed prev; rebuild from current session.
                *client = if sess.settings.api_key.is_empty() {
                    None
                } else {
                    Some(build_client(sess))
                };
            }
            state.rest.reset_scroll();
            state.mode = Mode::Chat;
            if client.is_none() {
                state.rest.status = "no active session".into();
            } else {
                state.rest.status = "ready".into();
            }
        }

        Action::CancelKeyInputToPicker => {
            // Esc out of a picker-launched KeyInput: drop the partially-set
            // session, clear any client, and return to the session picker
            // instead of pinning a no-client Chat.
            state.rest.session = None;
            state.rest.prev_session = None;
            *client = None;
            state.rest.reset_scroll();
            state.mode = Mode::SessionPicker(PickerState::new(store::list_sessions()?));
            state.rest.status = "ready".into();
        }

        Action::PickerSelect => {
            // Extract selected path first (borrow of mode released before
            // mutating rest/mode below).
            let path = match &state.mode {
                Mode::SessionPicker(p) => p.selected_meta().map(|m| m.path.clone()),
                _ => None,
            };
            let Some(path) = path else {
                state.rest.status = "no session selected".into();
                return Ok(());
            };
            let sess = match Session::load(&path) {
                Ok(s) => s,
                Err(e) => {
                    state.rest.status = format!("error: {e}");
                    return Ok(());
                }
            };
            if sess.settings.api_key.is_empty() {
                // Prefill from remembered creds; do NOT overwrite them.
                let lk = state.rest.last_key.clone().unwrap_or_default();
                let lm = state
                    .rest
                    .last_model
                    .clone()
                    .unwrap_or_else(|| DEFAULT_MODEL.to_string());
                let lp = state.rest.last_provider.clone().unwrap_or_default();
                state.rest.session = Some(sess);
                state.rest.reset_scroll();
                state.mode = Mode::KeyInput(KeyInputForm::prefilled(lk, lm, lp, false, true));
            } else {
                state
                    .rest
                    .remember_creds(&sess.settings.api_key, &sess.settings.model, &sess.settings.provider);
                *client = Some(build_client(&sess));
                let sess_path = sess.path.clone();
                state.rest.session = Some(sess);
                // Warm the selected session: reindex its workspace and compute
                // the awareness summary so picker-resume is fully primed like a
                // cold boot.
                super::warm_session(state, client, handle);
                // Existing session: seed the running totals from its full sqlite
                // log so the readout reflects prior usage.
                state.rest.load_token_totals(&sess_path);
                state.rest.reset_scroll();
                state.mode = Mode::Chat;
                state.rest.status = "ready".into();
            }
        }

        Action::SaveSettings => {
            // 1. Pull drafts out of the mode first so the borrow of `state.mode`
            //    is released before we mutate `state.rest` / `state.mode` below.
            let drafts = match &state.mode {
                Mode::Settings(s) => Some((
                    s.api_key.clone(),
                    s.model.clone(),
                    s.provider.clone(),
                    s.name.clone(),
                    s.theme.clone(),
                    s.accent.clone(),
                    s.workdir.clone(),
                    s.awareness_enabled,
                    s.awareness_inherit,
                    s.awareness_model.clone(),
                    s.awareness_provider.clone(),
                    s.classifier_enabled,
                    s.classifier_model.clone(),
                    s.classifier_provider.clone(),
                    s.allowed_folders.clone(),
                )),
                _ => None,
            };
            if let Some((
                api_key,
                model,
                provider,
                name,
                theme,
                accent,
                workdir,
                awareness_enabled,
                awareness_inherit,
                awareness_model,
                awareness_provider,
                classifier_enabled,
                classifier_model,
                classifier_provider,
                allowed_folders,
            )) = drafts
            {
                // Detect whether the OpenRouter-relevant creds changed so we only
                // rebuild the client when necessary.
                let creds_changed = match state.rest.session.as_ref() {
                    Some(s) => {
                        s.settings.api_key != api_key
                            || s.settings.model != model
                            || s.settings.provider != provider
                    }
                    None => false,
                };
                // a) Apply the text drafts to the session settings. The
                //    awareness settings ride along here too; they don't affect
                //    the chat client (the awareness call uses `complete_with`
                //    per invocation), so no client rebuild is keyed off them.
                // Normalise both path-list drafts: trim each entry, drop empties.
                // (They're already `Vec<String>` from the managed list editor — no
                // comma-splitting anymore.)
                let allowed_folders_vec: Vec<String> = allowed_folders
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                // Workdir must keep at least one entry; if the draft normalises to
                // nothing, fall back to the launch cwd so `Session::workdir` still
                // resolves and the reindex below has a real directory.
                let mut workdir_vec: Vec<String> = workdir
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if workdir_vec.is_empty() {
                    workdir_vec = std::env::current_dir()
                        .map(|p| vec![p.display().to_string()])
                        .unwrap_or_default();
                }
                if let Some(sess) = state.rest.session.as_mut() {
                    sess.settings.api_key = api_key;
                    sess.settings.model = model;
                    sess.settings.provider = provider;
                    sess.settings.workdir = workdir_vec;
                    sess.settings.awareness_enabled = awareness_enabled;
                    sess.settings.awareness_inherit = awareness_inherit;
                    sess.settings.awareness_model = awareness_model;
                    sess.settings.awareness_provider = awareness_provider;
                    // Harness settings ride along here too; like awareness they
                    // don't affect the chat client (the classifier uses
                    // `complete_with` per invocation), so no client rebuild is
                    // keyed off them.
                    sess.settings.classifier_enabled = classifier_enabled;
                    sess.settings.classifier_model = classifier_model;
                    sess.settings.classifier_provider = classifier_provider;
                    sess.settings.allowed_folders = allowed_folders_vec;
                }
                // b) Apply global theme/accent and persist config.json. Best-effort:
                //    a write failure surfaces to the status line but does not abort
                //    the rest of the save.
                state.rest.config.theme = theme;
                state.rest.config.accent = accent;
                if let Err(e) = state.rest.config.save() {
                    state.rest.status = format!("config save failed: {e}");
                }
                // c) Persist the session's settings.json.
                if let Some(sess) = state.rest.session.as_mut() {
                    if let Err(e) = sess.save() {
                        state.rest.status = format!("error: {e}");
                    }
                }
                // c2) Reindex the dir cache against the (possibly changed) workdir.
                //     Spawns a background thread; non-blocking.
                let workdir = state.rest.session.as_ref().map(|s| s.workdir());
                let dir_cache = state.rest.dir_cache.clone();
                if let Some(wd) = workdir {
                    crate::tool::dircache::reindex(wd, dir_cache);
                }
                // d) Rename LAST, and only when the name actually changed and is
                //    non-empty. Doing it last means a rename failure can't lose the
                //    other drafts (they're already saved above).
                let needs_rename = state
                    .rest
                    .session
                    .as_ref()
                    .map(|s| !name.trim().is_empty() && name.trim() != s.name)
                    .unwrap_or(false);
                if needs_rename {
                    if let Some(sess) = state.rest.session.as_mut() {
                        if let Err(e) = store::rename_session(sess, name.trim()) {
                            state.rest.status = format!("rename failed: {e}");
                        }
                    }
                }
                // e) Rebuild the OpenRouter client if the creds changed.
                if creds_changed {
                    *client = state.rest.session.as_ref().map(build_client);
                }
            }
            state.mode = Mode::Chat;
        }

        Action::SaveEffort(choice) => {
            // Store the chosen effort ("default" → empty = model default),
            // persist, then REBUILD the client so the new `reasoning` directive
            // is applied to the next request (effort is baked into the client).
            let effort = if choice == "default" { String::new() } else { choice };
            if let Some(sess) = state.rest.session.as_mut() {
                sess.settings.effort = effort.clone();
                if let Err(e) = sess.save() {
                    state.rest.status = format!("error: {e}");
                }
            }
            *client = state.rest.session.as_ref().map(build_client);
            let label = if effort.is_empty() { "default" } else { &effort };
            state.rest.status = format!("effort: {label}");
            state.mode = Mode::Chat;
        }

        Action::EffortCancel => {
            state.mode = Mode::Chat;
        }
    }
    Ok(())
}
