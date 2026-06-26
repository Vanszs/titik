//! Action dispatcher: apply a decoded keystroke action to app state.

use std::sync::Arc;

use anyhow::Result;

use crate::app::mode::{KeyInputForm, Mode, PickerState};
use crate::app::state::AppState;
use crate::config::{
    DEFAULT_AWARENESS_MODEL, DEFAULT_AWARENESS_PROVIDER, DEFAULT_CLASSIFIER_MODEL,
    DEFAULT_CLASSIFIER_PROVIDER, DEFAULT_MODEL,
};
use crate::model::app_config::{ApiType, ModelEntry, ModelRole, ProviderConn};
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
            // Phase label for the comet: a single word the shimmer sweeps across
            // (the elapsed counter is appended by the renderer). No trailing dots —
            // the comet supplies the motion, `· Ns` supplies the elapsed.
            state.rest.status = "thinking".into();
            start_stream_task(history, state, client, handle);

            // Prompt-classifier (PC), advisory + non-blocking: once per turn, if
            // the harness is enabled, classify the user prompt on a background
            // task. It sends one HarnessVerdict on a dedicated channel (drained
            // in run_loop) — it NEVER gates the stream that just started. Drop
            // any stale receiver from a prior turn first.
            state.rest.harness_rx = None;
            let pc_inputs = match (client.as_ref(), state.rest.session.as_ref()) {
                (Some(c), Some(sess)) if sess.settings.classifier_enabled => Some((
                    Arc::clone(c),
                    state.rest.config.clone(),
                    sess.settings.clone(),
                )),
                _ => None,
            };
            if let Some((c, config, settings)) = pc_inputs {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                state.rest.harness_rx = Some(rx);
                handle.spawn(async move {
                    let v =
                        crate::app::harness::classify_prompt(&c, &config, &settings, &pc_prompt)
                            .await;
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
                // Abandon any round parked on deferred task-tool delegations so a
                // sub-agent that finishes AFTER this interrupt can't resume a turn
                // the user killed. The orphaned sub-agents keep running in the
                // background; their terminal delivery finds no matching pending id
                // and is dropped (no chat fold, no re-stream).
                state.rest.pending_subagent_calls.clear();
                state.rest.awaiting_subagents = false;
                // Take any captured usage unconditionally so a partial turn's
                // usage can't leak into the next response.
                let usage = state.rest.pending_usage.take();
                // Likewise drain the reasoning buffer unconditionally so a
                // half-streamed thinking block can't bleed into the next turn;
                // it's folded onto the interrupted message (display-only).
                let reasoning = state.rest.take_reasoning();
                let buf = state.rest.take_stream();
                if let Some(b) = buf {
                    if !b.is_empty() {
                        if let Some(sess) = state.rest.session.as_mut() {
                            let content = format!("{b}  [interrupted]");
                            let _ = msglog::append(&sess.path, Role::Assistant, &content, usage);
                            sess.conversation.push_assistant(content, reasoning);
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
            state.rest.status = "thinking".into();
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

        Action::SaveCreds { endpoint, api_key, model } => {
            let endpoint = if endpoint.is_empty() {
                crate::config::DEFAULT_BASE_URL.to_string()
            } else {
                endpoint
            };
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
            // Back-compat + startup none-gate + legacy resolver fallback: mirror
            // the entered creds onto the session settings. `provider` (OpenRouter
            // routing slug) stays EMPTY — the wizard pins routing via the config
            // ProviderConn/ModelEntry below, not the legacy slug.
            if let Some(sess) = state.rest.session.as_mut() {
                sess.settings.api_key = api_key.clone();
                sess.settings.model = model.clone();
                sess.settings.provider = String::new();
                let _ = sess.save();
            }
            // Provider-agnostic config write (first-run): build a real
            // ProviderConn from the ENTERED endpoint (NOT hardcoded OpenRouter)
            // plus a Main-role ModelEntry, and persist config.json. Only seed when
            // the catalogue is empty (first-run norm) so a re-entry over an
            // existing config doesn't duplicate entries — the session-settings
            // mirror above already covers the legacy fallback in that case.
            if state.rest.config.providers.is_empty() && state.rest.config.models.is_empty() {
                let provider_uuid = uuid::Uuid::new_v4().to_string();
                state.rest.config.providers.push(ProviderConn {
                    uuid: provider_uuid.clone(),
                    name: provider_name_from_endpoint(&endpoint),
                    api_type: ApiType::OpenAiCompatible,
                    endpoint: endpoint.clone(),
                    api_key: api_key.clone(),
                });
                state.rest.config.models.push(ModelEntry {
                    uuid: uuid::Uuid::new_v4().to_string(),
                    name: "main".to_string(),
                    model_id: model.clone(),
                    provider_uuid: provider_uuid.clone(),
                    // Omnisearch routing is a later pass; no upstream pin here.
                    route: None,
                    // First-run only ever assigns Main (unchanged behavior); the
                    // legacy single-role field is left None so it isn't written.
                    roles: vec![ModelRole::Main],
                    role: None,
                });
                // OpenRouter first-run: auto-register the cheap groq-pinned
                // Awareness and Safeguard model entries so the harness works
                // out-of-the-box without manual configuration.
                if endpoint.to_lowercase().contains("openrouter") {
                    state.rest.config.models.push(ModelEntry {
                        uuid: uuid::Uuid::new_v4().to_string(),
                        name: "awareness".to_string(),
                        model_id: DEFAULT_AWARENESS_MODEL.into(),
                        provider_uuid: provider_uuid.clone(),
                        route: Some(DEFAULT_AWARENESS_PROVIDER.into()),
                        roles: vec![ModelRole::Awareness],
                        role: None,
                    });
                    state.rest.config.models.push(ModelEntry {
                        uuid: uuid::Uuid::new_v4().to_string(),
                        name: "safeguard".to_string(),
                        model_id: DEFAULT_CLASSIFIER_MODEL.into(),
                        provider_uuid,
                        route: Some(DEFAULT_CLASSIFIER_PROVIDER.into()),
                        roles: vec![ModelRole::Safeguard],
                        role: None,
                    });
                }
                if let Err(e) = state.rest.config.save() {
                    state.rest.status = format!("config save failed: {e}");
                }
            }
            // Routing slug is empty for the wizard path (config drives routing).
            state.rest.remember_creds(&api_key, &model, "");
            // KEYLESS client → no creds baked in; just (re)build for a fresh
            // plan_word at this session boundary. Resolve gates whether there's a
            // usable Main route (non-empty key) so we don't pin a no-creds client.
            *client = state.rest.session.as_ref().and_then(|sess| {
                crate::app::resolve::resolve_role(
                    &state.rest.config,
                    &sess.settings,
                    crate::model::app_config::ModelRole::Main,
                )
                .filter(|r| !r.api_key.is_empty())
                .map(|_| build_client())
            });
            // Seed totals from the (new or picker-prefilled) session's log.
            if let Some(p) = state.rest.session.as_ref().map(|s| s.path.clone()) {
                state.rest.load_token_totals(&p);
            }
            state.rest.prev_session = None; // committed; discard fallback
            state.rest.reset_scroll();
            // Land in Chat first, THEN warm: `warm_session` is non-blocking and may
            // upgrade the mode to `Mode::Loading` (animated splash) when it has warm
            // work to spawn, so it must run LAST to get the final word. With no warm
            // work it leaves the mode as the Chat we just set.
            state.mode = Mode::Chat;
            state.rest.status = "ready".into();
            // Warm the confirmed session: reindex its workspace + (async) fetch the
            // catalogue and awareness summary so it's primed like a cold boot.
            super::warm_session(state, client, handle);
        }

        Action::CancelKeyInput => {
            // KEYLESS client → build for a fresh plan_word at this session boundary;
            // gate on whether the restored session's MAIN role resolves to a usable
            // route (non-empty key), preserving the no-client-no-send invariant.
            let usable = |state: &AppState, settings: &crate::model::settings::Settings| {
                crate::app::resolve::resolve_role(
                    &state.rest.config,
                    settings,
                    crate::model::app_config::ModelRole::Main,
                )
                .is_some_and(|r| !r.api_key.is_empty())
            };
            if let Some(prev) = state.rest.prev_session.take() {
                *client = if usable(state, &prev.settings) {
                    Some(build_client())
                } else {
                    None
                };
                state.rest.session = Some(prev);
            } else if let Some(settings) = state.rest.session.as_ref().map(|s| s.settings.clone()) {
                // Defensive: no stashed prev; rebuild from current session.
                *client = if usable(state, &settings) {
                    Some(build_client())
                } else {
                    None
                };
            }
            // Restoring prev_session here bypasses warm_session, so reconcile the
            // lock directly: release the lock for the session we were configuring
            // and re-acquire the restored one's.
            super::reconcile_session_lock(state);
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

        Action::CancelPickerToChat => {
            // Esc/Ctrl+C in the /resume-opened session picker: the active
            // session is still in state.rest.session (untouched), so just
            // swap the mode back to Chat without disturbing anything else.
            state.mode = Mode::Chat;
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
            // Re-check the lock live (don't trust the cached row flag) so a race
            // — the session getting opened elsewhere after the list was built —
            // can't slip through. If it's locked by a live process, refuse to
            // enter and stay in the picker; the row already shows the marker.
            if store::is_locked(&path) {
                state.rest.status = "session is open — can't enter".into();
                return Ok(());
            }
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
                state.rest.session = Some(sess);
                state.rest.reset_scroll();
                state.mode = Mode::KeyInput(KeyInputForm::prefilled(lk, lm, false, true));
            } else {
                state
                    .rest
                    .remember_creds(&sess.settings.api_key, &sess.settings.model, &sess.settings.provider);
                // KEYLESS client → fresh plan_word at this session boundary. This
                // branch already gated on a non-empty key above, so build directly.
                *client = Some(build_client());
                let sess_path = sess.path.clone();
                state.rest.session = Some(sess);
                // Existing session: seed the running totals from its full sqlite
                // log so the readout reflects prior usage.
                state.rest.load_token_totals(&sess_path);
                state.rest.reset_scroll();
                // Land in Chat first, THEN warm: `warm_session` is non-blocking and
                // may upgrade the mode to `Mode::Loading` (animated splash) when it
                // has warm work to spawn, so it must run LAST to get the final word.
                // With no warm work it leaves the mode as the Chat we just set.
                state.mode = Mode::Chat;
                state.rest.status = "ready".into();
                // Warm the selected session: reindex its workspace + (async) fetch
                // the catalogue and awareness summary so picker-resume is primed
                // like a cold boot.
                super::warm_session(state, client, handle);
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
                    s.short_send_enabled,
                    s.sliding_cache,
                    s.providers.clone(),
                    s.models.clone(),
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
                short_send_enabled,
                sliding_cache,
                provider_drafts,
                model_drafts,
            )) = drafts
            {
                // No client rebuild is keyed off creds/model/provider changes
                // anymore: the client is KEYLESS and every request resolves its
                // connection/model/effort per-call via `resolve_role`, so the
                // existing Arc keeps serving (and keeps its cache-stable plan_word).
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
                // Map provider drafts -> persisted ProviderConn (preserve uuid;
                // mint one only if a draft somehow arrived without it).
                let provider_conns: Vec<ProviderConn> = provider_drafts
                    .iter()
                    .map(|d| ProviderConn {
                        uuid: if d.uuid.is_empty() {
                            uuid::Uuid::new_v4().to_string()
                        } else {
                            d.uuid.clone()
                        },
                        name: d.name.clone(),
                        api_type: d.api_type,
                        endpoint: d.endpoint.clone(),
                        api_key: d.api_key.clone(),
                    })
                    .collect();
                // Map model drafts -> persisted ModelEntry, resolving the draft's
                // positional `provider_idx` back to a `provider_uuid` against the
                // FRESHLY built provider_conns (so a model added in this same edit
                // session that points at a brand-new provider still resolves). A
                // dangling idx yields an empty provider_uuid (surfaces for re-pick).
                let to_entry = |d: &crate::app::mode::settings::ModelDraft| ModelEntry {
                    uuid: if d.uuid.is_empty() {
                        uuid::Uuid::new_v4().to_string()
                    } else {
                        d.uuid.clone()
                    },
                    name: d.name.clone(),
                    model_id: d.model_id.clone(),
                    provider_uuid: provider_conns
                        .get(d.provider_idx)
                        .map(|p| p.uuid.clone())
                        .unwrap_or_default(),
                    route: d.route.clone(),
                    // Persist the multi-role list; leave the legacy single-role
                    // field None so it stops being serialized (migration on save).
                    roles: d.roles.clone(),
                    role: None,
                };
                // Global catalogue: session_only == false. Session override layer:
                // session_only == true (persisted to settings.json, never config).
                let model_entries: Vec<ModelEntry> = model_drafts
                    .iter()
                    .filter(|d| !d.session_only)
                    .map(&to_entry)
                    .collect();
                let session_model_entries: Vec<ModelEntry> = model_drafts
                    .iter()
                    .filter(|d| d.session_only)
                    .map(&to_entry)
                    .collect();
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
                    // Short-send kill switch: no client rebuild needed; the
                    // shape() call reads this flag per-send.
                    sess.settings.short_send_enabled = short_send_enabled;
                    // Sliding-cache toggle: no client rebuild needed; a later
                    // wave's summarization logic reads this flag per-send.
                    sess.settings.sliding_cache = sliding_cache;
                    // Session-only models live in the per-session override layer,
                    // never in the global config. Persisted via sess.save() below.
                    sess.settings.session_models = session_model_entries;
                }
                // b) Apply global theme/accent + the provider/model catalogue and
                //    persist config.json in one write. Best-effort: a write failure
                //    surfaces to the status line but does not abort the rest of the
                //    save.
                state.rest.config.theme = theme;
                state.rest.config.accent = accent;
                state.rest.config.providers = provider_conns;
                state.rest.config.models = model_entries;
                if let Err(e) = state.rest.config.save() {
                    state.rest.status = format!("config save failed: {e}");
                }
                // c) Persist the session's settings.json.
                if let Some(sess) = state.rest.session.as_mut() {
                    if let Err(e) = sess.save() {
                        state.rest.status = format!("error: {e}");
                    }
                }
                // c2) Reindex the dir cache against the (possibly changed) workdirs.
                //     Spawns a background thread; non-blocking.
                let roots = state.rest.session.as_ref().map(|s| s.workdirs());
                let dir_cache = state.rest.dir_cache.clone();
                if let Some(r) = roots {
                    crate::tool::dircache::reindex(r, dir_cache);
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
                // e) No client rebuild: creds/model/provider are read per-call via
                //    `resolve_role`, so the existing keyless Arc serves the new
                //    settings on the next request. (This also keeps the cache-stable
                //    plan_word intact across a settings save.)
            }
            state.mode = Mode::Chat;
        }

        Action::SaveEffort(choice) => {
            // Store the chosen effort ("default" → empty = model default) and
            // persist. No client rebuild: effort is now resolved per-call (it flows
            // only into the streaming path via the Main route's `effort`), so the
            // existing keyless client applies the new directive on the next request
            // WITHOUT busting its cache-stable plan_word.
            let effort = if choice == "default" { String::new() } else { choice };
            if let Some(sess) = state.rest.session.as_mut() {
                sess.settings.effort = effort.clone();
                if let Err(e) = sess.save() {
                    state.rest.status = format!("error: {e}");
                }
            }
            let label = if effort.is_empty() { "default" } else { &effort };
            state.rest.status = format!("effort: {label}");
            state.mode = Mode::Chat;
        }

        Action::EffortCancel => {
            state.mode = Mode::Chat;
        }

        Action::CreateAgent => {
            apply_agent_create(state);
        }

        Action::SaveAgent => {
            apply_agent_save(state);
        }

        Action::DeleteAgent => {
            apply_agent_delete(state);
        }

        Action::CloseAgents => {
            // Discard any in-flight drafts; the dashboard never wrote them.
            state.mode = Mode::Chat;
            state.rest.status = "ready".into();
        }

        Action::FetchModelEndpoints(model_id) => {
            // Spawn the per-model provider-endpoints fetch on a background task
            // (mirrors the advisory prompt-classifier spawn in `Action::Submit`):
            // open a fresh channel, stash its receiver (replacing any in-flight
            // older fetch — dropping that receiver is the desired stale-cancel),
            // and send one EndpointsLoaded / EndpointsError when the request
            // resolves. The drain in `run_loop` folds it into the modal.
            //
            // Endpoints-API gate: `list_model_endpoints` is an OpenRouter-only GET,
            // and only an OpenAI-compatible provider has it (an Anthropic-typed
            // provider has no equivalent catalogue endpoint). When the modal's
            // SELECTED provider isn't an OpenRouter OpenAI-compatible one, don't
            // fire a doomed request: resolve the modal to an EMPTY endpoints list
            // (the view renders "no providers found" / Auto-only routing) and clear
            // loading. This keeps non-OpenRouter + Anthropic providers from
            // spinning on a request that would 404/400.
            if !matches!(&state.mode, Mode::Settings(s) if s.mm_provider_has_endpoints_api()) {
                if let Mode::Settings(s) = &mut state.mode {
                    if let Some(m) = s.model_modal.as_mut() {
                        m.endpoints = Some(Vec::new());
                        m.endpoints_loading = false;
                    }
                }
                return Ok(());
            }
            // No client, or no connection for the modal's OWN provider → nothing
            // to fetch against; clear the loading flag so the UI doesn't spin.
            // The endpoints GET must go against the EDITED MODEL's provider
            // connection (OpenRouter), NOT the Main role's connection (which may
            // be on a completely different provider). Pull (endpoint, api_key)
            // from `mm_provider_conn` and MOVE the owned Strings into the task
            // (no borrow of `state` crosses the spawn boundary).
            let provider_conn = if let Mode::Settings(s) = &state.mode {
                s.mm_provider_conn()
            } else {
                None
            };
            let (Some(c), Some((endpoint, api_key))) = (client.as_ref(), provider_conn) else {
                if let Mode::Settings(s) = &mut state.mode {
                    if let Some(m) = s.model_modal.as_mut() {
                        m.endpoints_loading = false;
                    }
                }
                return Ok(());
            };
            if endpoint.trim().is_empty() {
                if let Mode::Settings(s) = &mut state.mode {
                    if let Some(m) = s.model_modal.as_mut() {
                        m.endpoints_loading = false;
                    }
                }
                return Ok(());
            }
            let c = Arc::clone(c);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            state.rest.endpoints_rx = Some(rx);
            handle.spawn(async move {
                // A dropped receiver (modal closed / a newer fetch superseded
                // this one) makes the send a no-op — same contract as the
                // streaming + harness channels.
                let conn = crate::service::openrouter::Conn {
                    endpoint: &endpoint,
                    api_key: &api_key,
                };
                let _ = match c.list_model_endpoints(conn, &model_id).await {
                    Ok(eps) => tx.send(crate::service::StreamEvent::EndpointsLoaded {
                        model_id,
                        endpoints: eps,
                    }),
                    Err(e) => tx.send(crate::service::StreamEvent::EndpointsError {
                        model_id,
                        error: e.to_string(),
                    }),
                };
            });
        }

        Action::GeneratePrompt => {
            // One-shot agent-prompt GENERATOR (Ctrl+G in the `/agents` full-screen
            // prompt editor): a SINGLE non-streaming Main-model `complete` call (no
            // tools, no loop) runs on a background task and fills the editor buffer
            // with the result. Never blocks the UI — same spawn + channel + drain
            // pattern as the endpoints fetch / prompt classifier.

            // Guard: need a client + an active session, and we must currently be in
            // the agents prompt editor. Otherwise this Action is a no-op.
            if client.is_none() || state.rest.session.is_none() {
                return Ok(());
            }
            let editor_open =
                matches!(&state.mode, Mode::Agents(a) if a.prompt_editor.is_some());
            if !editor_open {
                return Ok(());
            }

            // Pull the draft name + description and the live editor buffer (owned)
            // out of the mode so no borrow of `state.mode` crosses into the resolve
            // / spawn below.
            let (name, desc, buffer) = match &state.mode {
                Mode::Agents(a) => (
                    a.draft_name.trim().to_string(),
                    a.draft_description.trim().to_string(),
                    a.prompt_editor.as_ref().map(|ed| ed.text()).unwrap_or_default(),
                ),
                _ => return Ok(()),
            };

            // Need at least a name or a description to have anything to generate from.
            if name.is_empty() && desc.is_empty() {
                state.rest.status = "fill description first to generate".into();
                return Ok(());
            }

            // Resolve the Main route (endpoint + key + model + upstream slug). Clone
            // the session settings out first so the resolve doesn't hold a borrow of
            // `state.rest.session` while we read the sibling `config`.
            let settings = state.rest.session.as_ref().unwrap().settings.clone();
            let resolved = match crate::app::resolve::resolve_role(
                &state.rest.config,
                &settings,
                ModelRole::Main,
            ) {
                Some(r) => r,
                None => {
                    state.rest.status = "no main model configured".into();
                    return Ok(());
                }
            };

            // Build the two-message prompt: a directive SYSTEM message + a USER
            // message carrying the agent's name + purpose, optionally seeding the
            // model with the current buffer to improve on.
            let system = "You write the SYSTEM PROMPT for a specialized sub-agent in a \
terminal coding assistant. Output ONLY the prompt text — second person ('You are...'), \
concise and directive: state the agent's role, how it should work (read before acting, \
use its tools, stay scoped), what to focus on, and that it must finish with a clear, \
complete report. No preamble, no markdown code fences, no surrounding quotes.";
            let mut user = format!(
                "Agent name: {name}\nPurpose / when to use: {desc}\n\nWrite the system prompt."
            );
            if !buffer.trim().is_empty() {
                user.push_str("\n\nImprove on this existing draft:\n");
                user.push_str(&buffer);
            }
            let messages = vec![
                crate::dto::chat::ChatMessage::new(Role::System, system),
                crate::dto::chat::ChatMessage::new(Role::User, user),
            ];

            // Own copies of the route pieces so the spawned task builds its own
            // `Conn` (which borrows the endpoint/key strings) entirely inside the
            // task — mirrors the sub-agent engine's `stream_step` clone-before-spawn.
            let endpoint = resolved.endpoint.clone();
            let api_key = resolved.api_key.clone();
            let model_id = resolved.model_id.clone();
            let provider = resolved.provider().to_string();
            let c = Arc::clone(client.as_ref().unwrap());

            // Open a fresh channel (dropping any prior in-flight generation's
            // receiver — the desired stale-cancel), mark generating, and kick off
            // the single call. The drain in `run_loop` folds the result back in.
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            state.rest.prompt_gen_rx = Some(rx);
            state.rest.prompt_generating = true;
            state.rest.status = "generating prompt...".into();
            handle.spawn(async move {
                let conn = crate::service::openrouter::Conn {
                    endpoint: &endpoint,
                    api_key: &api_key,
                };
                let r = c.complete(conn, &model_id, &provider, messages).await;
                // A dropped receiver (editor closed / app closing) makes this a
                // no-op — same contract as the streaming + endpoints channels.
                let _ = tx.send(r.map(|s| s.trim().to_string()).map_err(|e| e.to_string()));
            });
        }

        Action::SkipLoading => {
            // Esc on the loading splash: drop straight into Chat. The warm tasks
            // keep running in the background and their results still populate
            // `state.rest.*` via the `warm_rx` drain (the receiver is untouched
            // here). The session/chat state was already set up by the activation
            // path that opened the splash, so we only swap the mode.
            state.mode = Mode::Chat;
            state.rest.status = "ready".into();
        }
    }
    Ok(())
}

/// Derive a human-readable provider name from a base-URL `endpoint`, used to
/// label the [`ProviderConn`] the first-run wizard writes.
///
/// - An OpenRouter URL (case-insensitive) → `"OpenRouter"`.
/// - Otherwise the URL host (scheme, userinfo, port, and path stripped), e.g.
///   `https://api.example.com/v1` → `"api.example.com"`.
/// - Anything we can't parse a host out of → `"Provider"`.
fn provider_name_from_endpoint(endpoint: &str) -> String {
    if endpoint.to_lowercase().contains("openrouter") {
        return "OpenRouter".to_string();
    }
    // Strip the scheme (`https://`, `http://`, or any `scheme://`).
    let after_scheme = endpoint
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(endpoint);
    // Drop any userinfo (`user:pass@host`), then cut at the first path/port/query
    // delimiter to isolate the host.
    let authority = after_scheme.rsplit_once('@').map(|(_, h)| h).unwrap_or(after_scheme);
    let host = authority
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        "Provider".to_string()
    } else {
        host.to_string()
    }
}

/// Apply `Action::CreateAgent`: build an [`AgentDef`] from the Create drafts,
/// write it to the chosen scope, then reload the in-mode registry snapshot.
///
/// The name is re-validated by the data layer ([`agent_def::save_agent`]) so a
/// path-traversal name can never reach the filesystem. On success the dashboard
/// returns to Browse with the new agent in the list; on error the status line
/// reports it and the editor stays open so the draft isn't lost.
fn apply_agent_create(state: &mut AppState) {
    use crate::model::agent_def::{save_agent, AgentScope as DefScope};

    let Mode::Agents(a) = &state.mode else {
        return;
    };
    let scope_session = matches!(a.create_scope, crate::app::mode::AgentScope::Session);
    let def = a.to_agent_def();
    let session_dir = a.session_dir.clone();

    let scope = if scope_session {
        DefScope::Session(&session_dir)
    } else {
        DefScope::Global
    };
    let result = save_agent(scope, &def);

    match result {
        Ok(_) => {
            // Reload from disk so the new agent appears with its real source.
            if let Some(sess) = state.rest.session.as_ref() {
                if let Mode::Agents(a) = &mut state.mode {
                    a.reload(sess);
                    // Select the freshly-created agent if we can find it.
                    if let Some(i) = a.agents.iter().position(|x| x.name == def.name) {
                        a.list_sel = i;
                    }
                    a.cancel();
                }
            }
            // Rebuild the system prompt so the sub-agent roster reflects the new agent.
            if let Some(sess) = state.rest.session.as_mut() {
                sess.rebuild_system();
            }
            state.rest.status = format!("agent created: {}", def.name);
        }
        Err(e) => {
            state.rest.status = format!("create failed: {e}");
        }
    }
}

/// Apply `Action::SaveAgent`: overwrite the selected file-backed agent with the
/// Edit drafts, writing to the agent's own scope, then reload.
///
/// The target scope is the selected agent's [`AgentSource`] — a session agent is
/// re-saved into the session dir, a global agent into the global dir. A built-in
/// can never reach this path (the input handler blocks Edit on built-ins), so an
/// unexpected built-in source is treated as a no-op error.
fn apply_agent_save(state: &mut AppState) {
    use crate::model::agent_def::{save_agent, AgentScope as DefScope, AgentSource};

    let Mode::Agents(a) = &state.mode else {
        return;
    };
    let Some(agent) = a.current_agent() else {
        return;
    };
    let source = agent.source;
    let def = a.to_agent_def();
    let session_dir = a.session_dir.clone();

    let scope = match source {
        AgentSource::Global => DefScope::Global,
        AgentSource::Session => DefScope::Session(&session_dir),
        AgentSource::Builtin => {
            // Defensive: the UI never offers Edit on a built-in.
            state.rest.status = "built-in agents are read-only".into();
            return;
        }
    };
    let result = save_agent(scope, &def);

    match result {
        Ok(_) => {
            if let Some(sess) = state.rest.session.as_ref() {
                if let Mode::Agents(a) = &mut state.mode {
                    a.reload(sess);
                    if let Some(i) = a.agents.iter().position(|x| x.name == def.name) {
                        a.list_sel = i;
                    }
                    a.cancel();
                }
            }
            // Rebuild the system prompt so the sub-agent roster reflects the change.
            if let Some(sess) = state.rest.session.as_mut() {
                sess.rebuild_system();
            }
            state.rest.status = format!("agent updated: {}", def.name);
        }
        Err(e) => {
            state.rest.status = format!("save failed: {e}");
        }
    }
}

/// Apply `Action::DeleteAgent`: remove the selected file-backed agent from its
/// own scope's directory, then reload.
///
/// Built-ins are never deletable: they have no `file_path` and the input handler
/// blocks the delete prompt for them, so this only ever sees Global/Session
/// file agents. Deleting a session/global override that shadowed a built-in
/// simply re-exposes the built-in on the next reload.
fn apply_agent_delete(state: &mut AppState) {
    use crate::model::agent_def::{delete_agent, AgentScope as DefScope, AgentSource};

    let Mode::Agents(a) = &state.mode else {
        return;
    };
    let Some(agent) = a.current_agent() else {
        return;
    };
    let name = agent.name.clone();
    let source = agent.source;
    let session_dir = a.session_dir.clone();

    let scope = match source {
        AgentSource::Global => DefScope::Global,
        AgentSource::Session => DefScope::Session(&session_dir),
        AgentSource::Builtin => {
            state.rest.status = "cannot delete a built-in agent".into();
            if let Mode::Agents(a) = &mut state.mode {
                a.cancel();
            }
            return;
        }
    };
    let result = delete_agent(scope, &name);

    match result {
        Ok(()) => {
            if let Some(sess) = state.rest.session.as_ref() {
                if let Mode::Agents(a) = &mut state.mode {
                    a.reload(sess);
                    a.cancel();
                }
            }
            // Rebuild the system prompt so the sub-agent roster reflects the deletion.
            if let Some(sess) = state.rest.session.as_mut() {
                sess.rebuild_system();
            }
            state.rest.status = format!("agent deleted: {name}");
        }
        Err(e) => {
            state.rest.status = format!("delete failed: {e}");
            if let Mode::Agents(a) = &mut state.mode {
                a.cancel();
            }
        }
    }
}
