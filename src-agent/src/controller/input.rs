//! Controller – keyboard input handler ("C" in MVC).
//!
//! Every raw [`ratatui::crossterm::event::KeyEvent`] that the event loop receives is
//! passed to [`handle_key`], which dispatches to one of three mode-specific
//! handlers depending on [`Mode`]:
//!
//! - [`handle_chat`]       – normal chat input (send messages, scroll, quit)
//! - [`handle_key_input`]  – credentials form (api key + model)
//! - [`handle_picker`]     – `--resume` session list with live search
//!
//! Each handler returns an [`Action`] that the runtime loop (see
//! `app::runtime`) acts on.  No state is mutated here beyond the fields
//! belonging to the active mode and `AppStateRest`.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crate::app::mode::{AgentsState, EffortPickerState, KeyInputForm, LoadingState, Mode, PickerState, SettingsState, WarmStatus};
use crate::app::state::{AppState, AppStateRest};
use crate::controller::command::{self, Command};

/// All observable effects a key-press can request from the runtime.
///
/// The runtime match on this value and performs actual state mutations and I/O.
pub enum Action {
    /// Key was recognised but requires no runtime response.
    None,
    /// Exit the application cleanly.
    Quit,
    // --- Chat actions ---
    /// User confirmed a non-slash message; inner string is the trimmed input.
    Submit(String),
    /// User entered a `/slash` command; inner value is the parsed [`Command`].
    Slash(Command),
    /// Abort an in-flight API request (Ctrl+C / Esc while `waiting = true`).
    Interrupt,
    /// Re-send the last user message (Ctrl+R while idle).
    Resend,
    /// Approve the paused risky tool call (`y` in the approval modal): run it
    /// and resume the tool-approval state machine.
    ApproveTool,
    /// Deny the paused risky tool call (`n`/Esc in the approval modal): feed
    /// `"denied by user"` back as its result and resume the machine.
    DenyTool,
    // --- KeyInput actions ---
    /// Setup wizard finished; carry the entered endpoint, api key, and model out
    /// so the runtime can build a provider-agnostic config from them.
    SaveCreds { endpoint: String, api_key: String, model: String },
    /// Setup wizard advanced from the connection step to the model step with an
    /// OpenRouter endpoint: prefetch the model catalogue so step 2's live search
    /// has results. The runtime serves a fresh disk cache directly, else spawns a
    /// network fetch on the shared `warm_rx` channel (mirroring `warm_session`).
    /// A no-op for a non-OpenRouter endpoint (never emitted there).
    FetchWizardCatalogue { endpoint: String, api_key: String },
    /// Esc on a credentials form that was NOT opened from the picker — return
    /// to the normal Chat view.
    CancelKeyInput,
    /// Esc from a KeyInput that was opened from the --resume picker: go back to
    /// the picker rather than pinning a no-client Chat.
    CancelKeyInputToPicker,
    // --- Picker actions ---
    /// Enter on the session picker — open the highlighted session.
    PickerSelect,
    // --- Settings actions ---
    /// Esc on the settings dashboard (while navigating) — apply every draft and
    /// return to Chat. The apply path reads the drafts back out of
    /// `state.mode`, mirroring [`Action::PickerSelect`].
    SaveSettings,
    // --- Effort picker actions ---
    /// Enter on the `/effort` picker — store the chosen effort, rebuild the
    /// client so it takes effect, and return to Chat. Inner string is the chosen
    /// option (`"default"` stores `""`).
    SaveEffort(String),
    /// Esc on the `/effort` picker — discard the selection and return to Chat.
    EffortCancel,
    // --- Agents dashboard actions ---
    /// Confirm CREATE: write a new agent from the drafts, reload, back to Browse.
    CreateAgent,
    /// Confirm EDIT: overwrite the selected agent from the drafts, reload, back
    /// to Browse.
    SaveAgent,
    /// Confirm DELETE: remove the selected file-backed agent, reload, back to
    /// Browse.
    DeleteAgent,
    /// Esc from the agents dashboard (Browse, LIST focused) — discard any drafts
    /// and return to Chat.
    CloseAgents,
    /// Fetch the provider-endpoint list for the given model id (the inner
    /// `String`) on a background task. Emitted by the model modal when an
    /// OpenRouter model is selected (search) or an existing model is opened for
    /// edit; the modal's loading flags are already set by the caller. The
    /// runtime opens a fresh `endpoints_rx` channel and spawns the fetch.
    FetchModelEndpoints(String),
    // --- Loading splash actions ---
    /// Esc on the startup loading splash — skip the remaining warm steps and drop
    /// straight into Chat. The background warm tasks keep running; their results
    /// still populate `state.rest.*` via the `warm_rx` drain. The handler already
    /// marked any non-terminal step `Skipped` for correctness; the runtime just
    /// swaps the mode to `Chat`.
    SkipLoading,
}

/// If the input's current (last, whitespace-delimited) token is a file
/// reference (`@...`), return the partial path after the `@`. The file palette
/// is shown while this is `Some`.
pub fn file_ref_partial(input: &str) -> Option<&str> {
    let start = input.rfind([' ', '\n']).map(|i| i + 1).unwrap_or(0);
    let token = &input[start..];
    token.strip_prefix('@')
}

/// Replace the current `@token` in `rest.input` with the selected entry.
/// Completing a FILE appends a trailing space (closes the palette).
/// Completing a FOLDER (trailing `/`) does NOT append a space so the palette
/// stays open and the user can browse into the subfolder.
fn complete_file_ref(rest: &mut AppStateRest, matches: &[String]) {
    let sel = rest.palette_sel.min(matches.len().saturating_sub(1));
    let entry = &matches[sel];
    let start = rest.input.rfind([' ', '\n']).map(|i| i + 1).unwrap_or(0);
    rest.input.truncate(start);
    rest.input.push('@');
    rest.input.push_str(entry);
    if !entry.ends_with('/') {
        rest.input.push(' '); // a FILE completion closes the palette
    }
    // a FOLDER (trailing '/') gets NO space → palette stays open at the new depth
    rest.palette_sel = 0;
    // The input was rewritten wholesale (truncate + push); park the caret at the
    // end so it doesn't dangle inside the old, now-replaced @token.
    rest.cursor_end();
}

/// Translate a raw key event into an [`Action`] based on the current [`Mode`].
///
/// # Borrow-checker note
/// `AppState` is split into `state.mode` and `state.rest`.  Both are mutably
/// borrowed at the same time here because they are *disjoint* fields — the
/// borrow checker can prove they occupy non-overlapping memory.  The handlers
/// therefore receive `&mut mode_specific_data` and `&mut state.rest` as
/// separate parameters.
pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Action {
    // Ignore key-release and key-repeat events; only act on physical presses.
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }
    match &mut state.mode {
        Mode::Chat => handle_chat(&mut state.rest, key),
        Mode::KeyInput(form) => handle_key_input(form, &mut state.rest, key),
        Mode::SessionPicker(p) => handle_picker(p, &mut state.rest, key),
        Mode::Settings(s) => handle_settings(s, &mut state.rest, key),
        Mode::Agents(a) => handle_agents(a, &mut state.rest, key),
        Mode::Effort(e) => handle_effort(e, &mut state.rest, key),
        Mode::Loading(l) => handle_loading(l, key),
    }
}

/// Feed `text` into a SINGLE-LINE field through `sink`, one char at a time,
/// dropping every `\r`/`\n` (and bare CR from CRLF) so a multi-line clipboard
/// can never corrupt a one-line field. Mirrors a normal `Char` keystroke per
/// surviving char, which is the only way to reuse each field's existing
/// active-target resolution (`push_char` step/field logic, `mm_push_char`
/// query-vs-id routing, …).
fn paste_single_line(text: &str, mut sink: impl FnMut(char)) {
    for c in text.chars() {
        if c != '\r' && c != '\n' {
            sink(c);
        }
    }
}

/// Insert pasted text into the active input of the current mode/sub-mode,
/// mirroring how a `Char(c)` keystroke is routed there (same deepest-modal
/// priority), but inserting the WHOLE pasted string at once.
///
/// Multiline fields (the Chat input, the agent prompt Body) keep newlines
/// verbatim; every single-line field (endpoints, keys, names, search queries,
/// filters) strips `\n`/`\r` so a multi-line clipboard can't corrupt it.
/// Contexts with no text field (the role/provider pickers, the session/effort
/// pickers, the loading splash) ignore the paste.
pub fn handle_paste(state: &mut AppState, text: &str) {
    match &mut state.mode {
        Mode::Chat => {
            // Multiline verbatim: '\n' is kept (newline in the input, never a
            // submit); only the bare CR of a CRLF pair is dropped.
            for c in text.chars() {
                if c != '\r' {
                    state.rest.push_char(c);
                }
            }
        }
        Mode::KeyInput(form) => {
            // Step 1 on an OpenRouter endpoint is the catalogue omnisearch:
            // paste feeds the live query and resets the result highlight, exactly
            // as a typed char does. Every other field is plain text on `model` /
            // `endpoint` / `api_key` via `push_char`.
            if form.step == 1 && form.is_openrouter() {
                paste_single_line(text, |c| {
                    form.query.push(c);
                    form.result_sel = 0;
                });
            } else {
                paste_single_line(text, |c| form.push_char(c));
            }
        }
        Mode::Settings(s) => {
            // Deepest-modal priority, mirroring `handle_settings`:
            //   role picker (no text field) > model modal > provider modal >
            //   FS path picker > plain text field.
            if s.mm_role_picker_open() {
                // Checkbox overlay — no text entry; swallow the paste.
            } else if s.model_modal.is_some() {
                // `mm_push_char` already routes to the active model-modal field:
                // Name → name, Model → omnisearch query (OpenRouter, resets the
                // result highlight) or raw model id, and ignores Route/Role/buttons.
                paste_single_line(text, |c| s.mm_push_char(c));
            } else if s.prov_modal.is_some() {
                // Add-API-provider modal: `modal_push_char` writes to the active
                // text field (name/endpoint/api_key) and no-ops on the buttons.
                paste_single_line(text, |c| s.modal_push_char(c));
            } else if s.picker.is_some() {
                paste_single_line(text, |c| s.picker_push_char(c));
            } else if s.editing {
                paste_single_line(text, |c| s.push_char(c));
            }
        }
        Mode::Agents(a) => {
            use crate::app::mode::AgentEditField;
            // Deepest-modal priority, mirroring `handle_agents`:
            //   provider picker (no text field) > tool picker (filter) > draft field.
            if a.provider_picker.is_some() {
                // Single-select list — no text entry; swallow the paste.
            } else if let Some(p) = a.tool_picker.as_mut() {
                // Tool picker live filter (single-line).
                paste_single_line(text, |c| p.push_filter(c));
            } else if a.editing {
                // Typing into a draft field. The Model field on an OpenRouter
                // provider is an omnisearch (paste → query, reset highlight); the
                // Body is the multiline prompt (newlines kept); every other field
                // is single-line plain text.
                let model_search = a.field == AgentEditField::Model
                    && a.selected_provider_is_openrouter(
                        &state.rest.config,
                        state.rest.models_cache.as_deref().unwrap_or(&[]),
                    );
                let body = a.field == AgentEditField::Body;
                for c in text.chars() {
                    if c == '\r' || c == '\n' {
                        if c == '\n' && body {
                            a.newline();
                        }
                        continue;
                    }
                    if model_search {
                        a.model_query.push(c);
                        a.model_result_sel = 0;
                    } else {
                        a.push_char(c);
                    }
                }
            }
        }
        Mode::SessionPicker(p) => {
            // The `--resume` picker has a live search field: paste feeds the
            // query and re-runs the filter, exactly as a typed char does.
            paste_single_line(text, |c| p.query.push(c));
            p.refilter();
        }
        // No text entry on the effort picker or the loading splash — paste is a
        // no-op.
        Mode::Effort(_) | Mode::Loading(_) => {}
    }
}

/// Returns `true` when `key` is the given ASCII `c` held with Ctrl.
fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(x) if x == c)
}

/// This session's sent user messages, oldest-first (for bash-style recall).
fn user_messages(rest: &AppStateRest) -> Vec<String> {
    rest.session
        .as_ref()
        .map(|s| {
            s.conversation
                .messages()
                .iter()
                .filter(|m| m.role == crate::dto::chat::Role::User)
                .map(|m| m.content.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Handle a key press while the app is in Chat mode.
///
/// Ctrl+C and Esc both interrupt an in-flight request when `waiting` is true;
/// when idle they quit the app.  Ctrl+R re-sends the last message (idle only).
fn handle_chat(rest: &mut AppStateRest, key: KeyEvent) -> Action {
    // The help overlay is modal: any key closes it and is otherwise swallowed.
    if rest.help_open {
        rest.help_open = false;
        return Action::None;
    }

    // Tool-approval modal: while a risky call is paused, only y/n/Esc matter.
    // `y` approves (run it), `n`/Esc deny (feed "denied by user"); every other
    // key is swallowed so the prompt stays up and input can't leak underneath.
    if rest.awaiting_approval {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Action::ApproveTool,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Action::DenyTool,
            _ => Action::None,
        };
    }

    // Ctrl+C: interrupt if waiting OR a compaction animation is in flight
    // (the animation keeps `compact_anim_start` set while the deferred apply
    // is pending, and `waiting` may have already cleared if the model replied
    // fast). Never quit mid-animation — that would leave the spinner stuck.
    if is_ctrl(&key, 'c') {
        return if rest.waiting || rest.compact_anim_start.is_some() {
            Action::Interrupt
        } else {
            Action::Quit
        };
    }
    // Ctrl+R: resend (only when idle).
    if is_ctrl(&key, 'r') {
        return if rest.waiting {
            Action::None
        } else {
            Action::Resend
        };
    }
    // Ctrl+J: insert a newline (reliable multiline trigger; unlike Shift+Enter
    // this works on every terminal since Ctrl+J is literally the LF control code).
    if is_ctrl(&key, 'j') {
        rest.push_char('\n');
        return Action::None;
    }

    // Max visible entries in the `@` file-reference palette (shared across all
    // key handlers in this function and kept in sync with the view constant).
    const FILE_PAL_MAX: usize = 10;

    match key.code {
        KeyCode::Esc => {
            // Interrupt if waiting OR a compaction animation is still running
            // (compact_anim_start remains set during the deferred-apply window).
            // Quitting mid-animation would leave the spinner permanently stuck.
            if rest.waiting || rest.compact_anim_start.is_some() {
                Action::Interrupt
            } else {
                Action::Quit
            }
        }
        KeyCode::Enter => {
            // Shift+Enter inserts a newline instead of submitting — but only when
            // the terminal actually reports the SHIFT modifier on Enter (many do
            // not). Ctrl+J above is the always-works fallback. Plain Enter falls
            // through to the palette/slash/submit logic unchanged.
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                rest.push_char('\n');
                return Action::None;
            }
            let cmd_matches = command::palette_matches(&rest.input);
            if !cmd_matches.is_empty() {
                // Command palette open: run the highlighted command, not the raw text.
                let sel = rest.palette_sel.min(cmd_matches.len() - 1);
                let name = cmd_matches[sel].0;
                rest.take_input();
                Action::Slash(command::parse(name))
            } else {
                // File palette: complete instead of submitting when a file match is selected.
                let fmatches: Vec<String> = file_ref_partial(&rest.input)
                    .map(|p| rest.dir_cache.read().map(|c| c.search(p, FILE_PAL_MAX)).unwrap_or_default())
                    .unwrap_or_default();
                if !fmatches.is_empty() {
                    complete_file_ref(rest, &fmatches);
                    Action::None
                } else if rest.input.trim().starts_with('/') {
                    let line = rest.take_input();
                    Action::Slash(command::parse(&line))
                } else if !rest.input.trim().is_empty() && !rest.waiting {
                    Action::Submit(rest.take_input())
                } else {
                    Action::None
                }
            }
        }
        KeyCode::Backspace => {
            rest.backspace();
            Action::None
        }
        // Caret movement within the input line (mid-text editing). Left/Right
        // step one char; Home jumps to the start. End is handled below (it also
        // doubles as "scroll to bottom" when the input is empty).
        KeyCode::Left => {
            rest.cursor_left();
            Action::None
        }
        KeyCode::Right => {
            rest.cursor_right();
            Action::None
        }
        KeyCode::Home => {
            rest.cursor_home();
            Action::None
        }
        KeyCode::Up => {
            // Command palette takes precedence; then file palette; then within-input
            // line movement; finally history recall (only when already on line 0).
            if !command::palette_matches(&rest.input).is_empty() {
                rest.palette_sel = rest.palette_sel.saturating_sub(1);
            } else {
                let fmatches: Vec<String> = file_ref_partial(&rest.input)
                    .map(|p| rest.dir_cache.read().map(|c| c.search(p, FILE_PAL_MAX)).unwrap_or_default())
                    .unwrap_or_default();
                if !fmatches.is_empty() {
                    rest.palette_sel = rest.palette_sel.saturating_sub(1);
                } else if !rest.cursor_up() {
                    let users = user_messages(rest);
                    rest.history_prev(&users);
                }
            }
            Action::None
        }
        KeyCode::Down => {
            let n = command::palette_matches(&rest.input).len();
            if n > 0 {
                rest.palette_sel = (rest.palette_sel + 1).min(n - 1);
            } else {
                let fmatches: Vec<String> = file_ref_partial(&rest.input)
                    .map(|p| rest.dir_cache.read().map(|c| c.search(p, FILE_PAL_MAX)).unwrap_or_default())
                    .unwrap_or_default();
                if !fmatches.is_empty() {
                    rest.palette_sel = (rest.palette_sel + 1).min(fmatches.len() - 1);
                } else if !rest.cursor_down() {
                    let users = user_messages(rest);
                    rest.history_next(&users);
                }
            }
            Action::None
        }
        KeyCode::Tab => {
            let cmd_matches = command::palette_matches(&rest.input);
            if !cmd_matches.is_empty() {
                let sel = rest.palette_sel.min(cmd_matches.len() - 1);
                rest.input = format!("{} ", cmd_matches[sel].0);
                rest.palette_sel = 0;
                rest.cursor_end(); // input replaced wholesale → caret to the end
            } else {
                let fmatches: Vec<String> = file_ref_partial(&rest.input)
                    .map(|p| rest.dir_cache.read().map(|c| c.search(p, FILE_PAL_MAX)).unwrap_or_default())
                    .unwrap_or_default();
                if !fmatches.is_empty() {
                    complete_file_ref(rest, &fmatches);
                }
            }
            Action::None
        }
        KeyCode::PageUp => {
            for _ in 0..10 {
                rest.scroll_up();
            }
            Action::None
        }
        KeyCode::PageDown => {
            for _ in 0..10 {
                rest.scroll_down();
            }
            Action::None
        }
        // End: with input present, move the caret to the end of the line (text
        // editing). With an EMPTY input it keeps its old meaning — jump the
        // transcript to the bottom and resume following.
        KeyCode::End => {
            if rest.input.is_empty() {
                rest.reset_scroll();
            } else {
                rest.cursor_end();
            }
            Action::None
        }
        // Shift+Tab toggles the tool-approval mode (Auto <-> Normal). Crossterm
        // reports Shift+Tab as BackTab, so it never collides with plain Tab.
        KeyCode::BackTab => {
            rest.agent_mode = rest.agent_mode.toggled();
            rest.status = format!("mode: {}", rest.agent_mode.label());
            Action::None
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            rest.push_char(c);
            Action::None
        }
        _ => Action::None,
    }
}

/// Handle a key press while the first-run setup wizard is active.
///
/// Two steps: step 0 = connection (endpoint + key), step 1 = model. Tab / ↑ / ↓
/// move between fields WITHIN the current step; Enter advances (field → step →
/// finish) and Esc walks back (step 1 → step 0 → cancel).
///
/// Step 1 (model) has TWO modes, keyed on [`KeyInputForm::is_openrouter`]:
/// - **OpenRouter** → a live omnisearch over `rest.models_cache`. Chars edit
///   `form.query`; ↑/↓ move `form.result_sel` over `filter_models(cache, query)`;
///   Enter picks the highlighted result into `form.model` (or, when there are no
///   results, falls back to the raw trimmed query so manual entry never traps)
///   and finishes. The catalogue is prefetched on the step-0→1 advance via
///   [`Action::FetchWizardCatalogue`].
/// - **Other endpoint** → a plain text Model box (chars edit `form.model`).
///
/// Esc on step 0 has three cases:
/// 1. `first_run = true` → no prior client exists, so Esc must quit rather than drop back to a broken Chat view.
/// 2. `from_picker = true` → form was opened from the `--resume` session picker, so Esc returns there (`CancelKeyInputToPicker`).
/// 3. Otherwise → Esc cancels back to the existing Chat view.
fn handle_key_input(form: &mut KeyInputForm, rest: &mut AppStateRest, key: KeyEvent) -> Action {
    use crate::app::mode::filter_models;

    if is_ctrl(&key, 'c') {
        return Action::Quit;
    }

    // --- Step 1 (model) on an OpenRouter endpoint: live catalogue omnisearch ---
    // This intercepts ALL keys for the model step so the search box + results list
    // own the input (chars → query, ↑/↓ → result_sel, Enter → pick/finish). Esc
    // still walks back to the connection step. The non-OpenRouter model step and
    // the whole connection step fall through to the generic handler below.
    if form.step == 1 && form.is_openrouter() {
        let cache = rest.models_cache.as_deref().unwrap_or(&[]);
        return match key.code {
            KeyCode::Esc => {
                // Non-destructive: back to the connection step (clears the query).
                form.back_step();
                Action::None
            }
            KeyCode::Up => {
                form.result_sel = form.result_sel.saturating_sub(1);
                Action::None
            }
            KeyCode::Down => {
                // Clamp to the last filtered result (0 when there are none).
                let n = filter_models(cache, &form.query).len();
                form.result_sel = (form.result_sel + 1).min(n.saturating_sub(1));
                Action::None
            }
            KeyCode::Enter => {
                let results = filter_models(cache, &form.query);
                if !results.is_empty() {
                    // Pick the highlighted catalogue model.
                    let sel = form.result_sel.min(results.len() - 1);
                    form.model = cache[results[sel]].id.clone();
                    Action::SaveCreds {
                        endpoint: form.endpoint.trim().to_string(),
                        api_key: form.api_key.trim().to_string(),
                        model: form.model.clone(),
                    }
                } else {
                    // No results (catalogue still loading, or the query matches
                    // nothing): fall back to the raw query as a manual model id so
                    // the wizard never traps. Finish only when it's non-empty.
                    let typed = form.query.trim();
                    if typed.is_empty() {
                        rest.status = "model required".into();
                        Action::None
                    } else {
                        form.model = typed.to_string();
                        Action::SaveCreds {
                            endpoint: form.endpoint.trim().to_string(),
                            api_key: form.api_key.trim().to_string(),
                            model: form.model.clone(),
                        }
                    }
                }
            }
            KeyCode::Backspace => {
                form.query.pop();
                form.result_sel = 0;
                Action::None
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                form.query.push(c);
                form.result_sel = 0; // new filter → reset the highlight
                Action::None
            }
            _ => Action::None,
        };
    }

    match key.code {
        KeyCode::Esc => {
            if form.step == 1 {
                // Model step: step back to the connection step (non-destructive).
                form.back_step();
                Action::None
            } else if form.first_run {
                // No usable session exists yet — quitting is safer than an
                // unconfigured Chat screen.
                Action::Quit
            } else if form.from_picker {
                // Opened via --resume flow: go back to the session list.
                Action::CancelKeyInputToPicker
            } else {
                // Opened from within an active chat: return to it.
                Action::CancelKeyInput
            }
        }
        // Field navigation stays WITHIN the current step.
        KeyCode::Tab | KeyCode::Down => {
            form.next_field();
            Action::None
        }
        KeyCode::Up => {
            form.prev_field();
            Action::None
        }
        KeyCode::Enter => {
            if form.step == 0 {
                // Connection step: advance to the model step only from the last
                // field (API key) with a non-empty key; otherwise move to the
                // next field.
                if form.is_last_field() {
                    if form.api_key.trim().is_empty() {
                        rest.status = "api key required".into();
                        Action::None
                    } else {
                        // Advance to the model step. For an OpenRouter endpoint,
                        // ALSO prefetch the catalogue so step 2's live search has
                        // results (advance first so the form is already on step 1
                        // when the fetch resolves). Non-OpenRouter: just advance.
                        let or = form.is_openrouter();
                        form.advance_step();
                        if or {
                            Action::FetchWizardCatalogue {
                                endpoint: form.endpoint.trim().to_string(),
                                api_key: form.api_key.trim().to_string(),
                            }
                        } else {
                            Action::None
                        }
                    }
                } else {
                    form.next_field();
                    Action::None
                }
            } else {
                // Model step (non-OpenRouter plain text): finish if non-empty.
                if form.can_finish() {
                    Action::SaveCreds {
                        endpoint: form.endpoint.trim().to_string(),
                        api_key: form.api_key.trim().to_string(),
                        model: form.model.trim().to_string(),
                    }
                } else {
                    rest.status = "model required".into();
                    Action::None
                }
            }
        }
        KeyCode::Backspace => {
            form.backspace();
            Action::None
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            form.push_char(c);
            Action::None
        }
        _ => Action::None,
    }
}

/// Handle a key press inside the `--resume` session picker.
///
/// Typing characters updates the live search query and triggers `refilter`.
/// `_rest` is accepted for API consistency with the other handlers but unused.
fn handle_picker(p: &mut PickerState, _rest: &mut AppStateRest, key: KeyEvent) -> Action {
    if is_ctrl(&key, 'c') {
        return Action::Quit;
    }

    match key.code {
        KeyCode::Esc => Action::Quit,
        KeyCode::Up => {
            p.move_up();
            Action::None
        }
        KeyCode::Down => {
            p.move_down();
            Action::None
        }
        KeyCode::Enter => Action::PickerSelect,
        KeyCode::Backspace => {
            p.query.pop();
            p.refilter();
            Action::None
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            p.query.push(c);
            p.refilter();
            Action::None
        }
        _ => Action::None,
    }
}

/// Handle a key press inside the `/settings` dashboard.
///
/// Nested focus design (deepest first):
///
/// 0. **picker** (`s.picker` is `Some`) – the FS directory picker overlay.
///    Type to filter, ↑/↓ select, Tab descends into the highlighted dir,
///    Enter confirms (applies to the path list), Esc cancels. Highest priority.
///
/// 1. **list_editing** – a path-list field is open for per-entry management.
///    ↑/↓ move the highlighted entry; `+`/`a` add (opens the picker); `-`/`d`
///    remove (min-1 rule); Enter edits the entry (opens the picker, seeded);
///    Esc returns to field navigation.
///
/// 2. **editing** – user is typing into a plain text field.
///    Enter / Esc commit the draft and drop back to detail navigation.
///    Backspace / Char delegate to the state mutation helpers.
///
/// 3. **in_detail** (none of the above) – cursor is on the field list of the
///    active category. Esc / Left return focus to the sidebar. Enter activates
///    the current field (toggle / edit / enter list management). Left/Right on
///    the Accent field cycle the accent; Left otherwise returns to the sidebar.
///
/// 4. **sidebar** – cursor is on the category list.
///    Esc saves all drafts and closes the dashboard (`Action::SaveSettings`).
///    Enter / Right move focus to the detail pane.
///
/// `rest` is used by the models-modal omnisearch (it reads `rest.models_cache`
/// to navigate/select catalogue results); the other branches don't touch it.
fn handle_settings(s: &mut SettingsState, rest: &mut AppStateRest, key: KeyEvent) -> Action {
    use crate::app::mode::{filter_models, SettingField};

    if is_ctrl(&key, 'c') {
        return Action::Quit;
    }

    // --- Role checkbox picker (DEEPEST level: a modal-on-modal over the model
    //     modal; intercepts ALL keys before the rest of the model-modal handling).
    //     Up/Down move the cursor, Space toggles the row, Enter commits the
    //     selection into `roles`, Esc discards. ---
    if s.mm_role_picker_open() {
        match key.code {
            KeyCode::Up => {
                s.mm_role_picker_up();
            }
            KeyCode::Down => {
                s.mm_role_picker_down();
            }
            KeyCode::Char(' ') => {
                s.mm_role_picker_toggle();
            }
            KeyCode::Enter => {
                s.confirm_role_picker();
            }
            KeyCode::Esc => {
                s.cancel_role_picker();
            }
            _ => {}
        }
        return Action::None;
    }

    // --- Add/edit-model modal (deepest level: intercepts ALL keys except Ctrl+C) ---
    if s.model_modal.is_some() {
        use crate::app::mode::settings::ModelField;
        let or = s.mm_provider_is_openrouter();
        let cur = s.mm_current_field();
        let query = s.mm_query().to_string();
        // Omnisearch is live only on the Model field, for an OpenRouter provider,
        // once the user has typed something.
        let search_mode = cur == Some(ModelField::Model) && or && !query.is_empty();

        // Selecting a model arms its provider-endpoints load: `mm_select_model`
        // sets the modal's loading flags, and we hand the chosen id back to the
        // runtime here so it spawns the fetch (the drain folds the result in).
        let mut modal_action = Action::None;

        if search_mode {
            let cache = rest.models_cache.as_deref().unwrap_or(&[]);
            match key.code {
                KeyCode::Esc => {
                    s.close_model_modal();
                }
                KeyCode::Up => {
                    s.mm_result_up();
                }
                KeyCode::Down => {
                    let len = filter_models(cache, &query).len();
                    s.mm_result_down(len.saturating_sub(1));
                }
                KeyCode::Enter => {
                    let results = filter_models(cache, &query);
                    if !results.is_empty() {
                        let sel = s
                            .model_modal
                            .as_ref()
                            .map(|m| m.result_sel)
                            .unwrap_or(0)
                            .min(results.len() - 1);
                        let id = cache[results[sel]].id.clone();
                        // Set the model + arm the loading flags, then trigger the
                        // background endpoints fetch for the chosen id. (Search
                        // mode is OpenRouter-only, so an endpoints API always
                        // exists for this path.)
                        s.mm_select_model(id.clone());
                        modal_action = Action::FetchModelEndpoints(id);
                    }
                }
                // Tab escapes the search and advances to the next field.
                KeyCode::Tab => {
                    s.mm_down();
                }
                KeyCode::Backspace => {
                    s.mm_backspace();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.mm_push_char(c);
                }
                _ => {}
            }
        } else if cur == Some(ModelField::Route) {
            // Route field: Up/Down navigate the provider options (Auto + each
            // fetched endpoint), Enter commits the highlighted choice. Same
            // dual-semantics as the omnisearch results: here ↑/↓ drive the list,
            // NOT the modal field cursor. Tab/Left/Right still move fields.
            //
            // Boundary-escape: Up at the first option (sel == 0) exits the Route
            // list upward to the previous field; Down at the last option exits
            // downward to the next field. This prevents the user from getting
            // trapped in the Route list. Enter commits AND advances to the next
            // field so it gives visible forward progress.
            let count = s.mm_route_option_count();
            let sel   = s.mm_route_sel();
            match key.code {
                KeyCode::Esc => {
                    s.close_model_modal();
                }
                KeyCode::Up => {
                    if sel > 0 {
                        s.mm_route_up();
                    } else {
                        s.mm_up();
                    }
                }
                KeyCode::Down => {
                    if sel + 1 < count {
                        s.mm_route_down();
                    } else {
                        s.mm_down();
                    }
                }
                KeyCode::Enter => {
                    s.mm_route_commit();
                    s.mm_down();
                }
                // Tab advances out of the Route field to the next one.
                KeyCode::Tab => {
                    s.mm_down();
                }
                KeyCode::Left => {
                    s.mm_left();
                }
                KeyCode::Right => {
                    s.mm_right();
                }
                _ => {}
            }
        } else if cur == Some(ModelField::Role) {
            // Role field (picker closed): the value is a read-only summary;
            // Enter opens the Role checkbox picker overlay (handled above once
            // open). Up/Down|Tab just move off the field (non-trap nav, same as
            // the other fields); Esc closes the whole modal.
            match key.code {
                KeyCode::Esc => {
                    s.close_model_modal();
                }
                KeyCode::Enter => {
                    s.open_role_picker();
                }
                KeyCode::Up => {
                    s.mm_up();
                }
                KeyCode::Down | KeyCode::Tab => {
                    s.mm_down();
                }
                _ => {}
            }
        } else {
            // Field navigation (Name / Provider / Model-as-text / Save / Cancel).
            match key.code {
                KeyCode::Esc => {
                    s.close_model_modal();
                }
                KeyCode::Up => {
                    s.mm_up();
                }
                KeyCode::Down | KeyCode::Tab => {
                    s.mm_down();
                }
                KeyCode::Left => {
                    s.mm_left();
                }
                KeyCode::Right => {
                    s.mm_right();
                }
                KeyCode::Enter => {
                    match cur {
                        Some(ModelField::Save) => s.save_model_modal(false),
                        Some(ModelField::SaveSession) => s.save_model_modal(true),
                        Some(ModelField::Cancel) => s.close_model_modal(),
                        // Name / Provider / Model: advance to the next field.
                        _ => s.mm_down(),
                    }
                }
                KeyCode::Backspace => {
                    s.mm_backspace();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.mm_push_char(c);
                }
                _ => {}
            }
        }
        return modal_action;
    }

    // --- Add-provider modal (deepest level: intercepts ALL keys except Ctrl+C) ---
    if s.prov_modal.is_some() {
        match key.code {
            KeyCode::Esc => {
                s.close_provider_modal();
            }
            KeyCode::Up => {
                s.modal_up();
            }
            KeyCode::Down | KeyCode::Tab => {
                s.modal_down();
            }
            KeyCode::Left => {
                s.modal_left();
            }
            KeyCode::Right => {
                s.modal_right();
            }
            KeyCode::Enter => {
                let field = s.prov_modal.as_ref().map(|m| m.field).unwrap_or(0);
                if field == 3 {
                    s.save_provider_modal();
                } else if field == 4 {
                    s.close_provider_modal();
                } else {
                    // fields 0 (name), 1 (endpoint), 2 (api_key): advance to next
                    s.modal_down();
                }
            }
            KeyCode::Backspace => {
                s.modal_backspace();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                s.modal_push_char(c);
            }
            _ => {}
        }
        return Action::None;
    }

    if s.picker.is_some() {
        // --- FS directory picker (deepest level) ---
        match key.code {
            // Cancel the picker WITHOUT applying; stay in list management.
            KeyCode::Esc => {
                s.picker_cancel();
                Action::None
            }
            // Confirm: apply the chosen path to the list, close the picker.
            KeyCode::Enter => {
                s.picker_confirm();
                Action::None
            }
            KeyCode::Up => {
                if let Some(p) = s.picker.as_mut() {
                    p.up();
                }
                Action::None
            }
            KeyCode::Down => {
                if let Some(p) = s.picker.as_mut() {
                    p.down();
                }
                Action::None
            }
            // Tab descends into the highlighted match for easy directory walking.
            KeyCode::Tab => {
                s.picker_descend();
                Action::None
            }
            KeyCode::Backspace => {
                s.picker_backspace();
                Action::None
            }
            // Any printable char appends to the query.
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                s.picker_push_char(c);
                Action::None
            }
            _ => Action::None,
        }
    } else if s.list_editing {
        // --- Path-list per-entry management ---
        match key.code {
            // Done managing this list: back to field navigation.
            KeyCode::Esc => {
                s.list_editing = false;
                Action::None
            }
            KeyCode::Up => {
                s.list_up();
                Action::None
            }
            KeyCode::Down => {
                s.list_down();
                Action::None
            }
            // Add a new entry via the picker.
            KeyCode::Char('+') | KeyCode::Char('a') => {
                s.open_picker_add();
                Action::None
            }
            // Remove the highlighted entry (last entry is protected).
            KeyCode::Char('-') | KeyCode::Char('d') => {
                s.list_remove();
                Action::None
            }
            // Edit the highlighted entry via the picker (seeded with its value).
            KeyCode::Enter => {
                s.open_picker_replace();
                Action::None
            }
            _ => Action::None,
        }
    } else if s.editing {
        match key.code {
            // Commit the draft and return to detail navigation; do not close.
            KeyCode::Enter | KeyCode::Esc => {
                s.editing = false;
                Action::None
            }
            KeyCode::Backspace => {
                s.backspace();
                Action::None
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                s.push_char(c);
                Action::None
            }
            _ => Action::None,
        }
    } else if s.in_detail {
        // --- Providers category: custom navigation for the provider list ---
        if s.is_providers_category() {
            match key.code {
                KeyCode::Esc => {
                    s.focus_sidebar();
                }
                KeyCode::Up => {
                    s.prov_up();
                }
                KeyCode::Down | KeyCode::Tab => {
                    s.prov_down();
                }
                KeyCode::Char('+') => {
                    s.open_provider_modal();
                }
                KeyCode::Enter => {
                    if s.prov_on_add_button() {
                        s.open_provider_modal();
                    }
                }
                _ if is_ctrl(&key, 'x') => {
                    s.prov_arm_or_delete();
                }
                _ => {
                    s.prov_disarm();
                }
            }
            return Action::None;
        }

        // --- Models Select category: custom navigation for the models list ---
        if s.is_models_category() {
            // Opening an existing OpenRouter model for edit arms its endpoints
            // load; the chosen id is returned to the runtime so it spawns the
            // fetch (an existing model's providers load on open).
            let mut models_action = Action::None;
            match key.code {
                KeyCode::Esc => {
                    s.focus_sidebar();
                }
                KeyCode::Up => {
                    s.model_up();
                }
                KeyCode::Down | KeyCode::Tab => {
                    s.model_down();
                }
                // Catalogue prefetch happens on /settings open, so '+' just opens
                // the modal — no Action needed in Pass A.
                KeyCode::Char('+') => {
                    s.open_model_modal_add();
                }
                KeyCode::Enter => {
                    if s.model_on_add_button() {
                        s.open_model_modal_add();
                    } else {
                        s.open_model_modal_edit(s.model_sel);
                        // If the opened model's provider is OpenRouter and it has
                        // a model id, arm the loading flags + fetch its providers.
                        // Non-OpenRouter / empty id → no endpoints API, so this
                        // returns None and the modal opens without a fetch.
                        if let Some(id) = s.mm_arm_endpoints_load() {
                            models_action = Action::FetchModelEndpoints(id);
                        }
                    }
                }
                _ if is_ctrl(&key, 'x') => {
                    s.model_arm_or_delete();
                }
                _ => {
                    s.model_disarm();
                }
            }
            return models_action;
        }

        match key.code {
            // Return to the sidebar (also exits editing/list/picker state).
            KeyCode::Esc => {
                s.focus_sidebar();
                Action::None
            }
            KeyCode::Up => {
                s.up();
                Action::None
            }
            KeyCode::Down | KeyCode::Tab => {
                s.down();
                Action::None
            }
            // Theme/awareness toggle / start editing text field / enter a path list.
            KeyCode::Enter => {
                s.enter();
                Action::None
            }
            KeyCode::Left => {
                // Accent field: cycle backward. Any other field: go back to sidebar.
                if s.current_field() == SettingField::Accent {
                    s.cycle_accent(false);
                } else {
                    s.focus_sidebar();
                }
                Action::None
            }
            KeyCode::Right => {
                if s.current_field() == SettingField::Accent {
                    s.cycle_accent(true);
                }
                Action::None
            }
            _ => Action::None,
        }
    } else {
        // Sidebar focus.
        match key.code {
            // Save every draft and close the dashboard.
            KeyCode::Esc => Action::SaveSettings,
            KeyCode::Up => {
                s.up();
                Action::None
            }
            KeyCode::Down | KeyCode::Tab => {
                s.down();
                Action::None
            }
            // Move focus to the detail pane.
            KeyCode::Enter | KeyCode::Right => {
                s.focus_detail();
                Action::None
            }
            _ => Action::None,
        }
    }
}

/// Handle a key press inside the `/agents` management dashboard.
///
/// Context-sensitive dispatch keyed on the sub-mode + editing flag (deepest
/// focus first):
///
/// 0. **DeleteConfirm** – modal y/n; `y` deletes (`Action::DeleteAgent`),
///    `n`/Esc cancels back to Browse.
///
/// 1. **editing** (Edit/Create, typing a field) – Char/Backspace mutate the
///    draft; Ctrl+J / Shift+Enter add a newline in the body; Enter or Esc
///    commit the field and drop back to field navigation (Esc in Create only
///    leaves the field, it does NOT cancel the whole flow).
///
/// 2. **Edit/Create** (navigating fields, not editing) – ↑/↓ move the field
///    cursor; Enter starts editing the field (Name/Description/… ) or, for the
///    scope row in Create, toggles scope; `s` saves/creates; Esc cancels the
///    whole flow back to Browse.
///
/// 3. **Browse** – ↑/↓ move the LIST cursor; →/Enter open the selected agent
///    for editing (built-ins are read-only → status note, no transition);
///    `n` starts Create; `d` deletes the selected file-backed agent; Esc closes
///    the dashboard (`Action::CloseAgents`).
fn handle_agents(s: &mut AgentsState, rest: &mut AppStateRest, key: KeyEvent) -> Action {
    use crate::app::mode::{filter_models, AgentEditField, AgentSubMode};
    use crate::model::agent_def::AgentSource;

    if is_ctrl(&key, 'c') {
        return Action::Quit;
    }

    // --- Provider picker overlay (DEEPEST priority: intercepts ALL keys) ---
    // Single-select pick-one list: ↑/↓ navigate, Enter commits the cursor's
    // provider into the draft, Esc discards. Sits above the tool picker (both are
    // mutually exclusive, but only one can ever be open at a time).
    if s.provider_picker.is_some() {
        match key.code {
            KeyCode::Up => {
                if let Some(p) = s.provider_picker.as_mut() {
                    p.up();
                }
            }
            KeyCode::Down => {
                if let Some(p) = s.provider_picker.as_mut() {
                    p.down();
                }
            }
            KeyCode::Enter => {
                s.confirm_provider_picker();
            }
            KeyCode::Esc => {
                s.cancel_provider_picker();
            }
            _ => {}
        }
        return Action::None;
    }

    // --- Tool picker overlay (intercepts ALL keys) ---
    if s.tool_picker.is_some() {
        match key.code {
            KeyCode::Up => {
                if let Some(p) = s.tool_picker.as_mut() {
                    p.up();
                }
            }
            KeyCode::Down => {
                if let Some(p) = s.tool_picker.as_mut() {
                    p.down();
                }
            }
            KeyCode::Char(' ') => {
                if let Some(p) = s.tool_picker.as_mut() {
                    p.toggle();
                }
            }
            KeyCode::Enter => {
                s.confirm_tool_picker();
            }
            KeyCode::Esc => {
                s.cancel_tool_picker();
            }
            KeyCode::Backspace => {
                if let Some(p) = s.tool_picker.as_mut() {
                    p.backspace_filter();
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL) && c != ' ' =>
            {
                if let Some(p) = s.tool_picker.as_mut() {
                    p.push_filter(c);
                }
            }
            _ => {}
        }
        return Action::None;
    }

    match s.mode {
        // --- DeleteConfirm: modal y/n ---
        AgentSubMode::DeleteConfirm => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Action::DeleteAgent,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                s.mode = AgentSubMode::Browse;
                Action::None
            }
            _ => Action::None,
        },

        // --- Edit / Create ---
        AgentSubMode::Edit | AgentSubMode::Create => {
            // Is the Model field being edited as an OpenRouter omnisearch? (Model
            // field + editing + the chosen provider is OpenRouter.) When so, keys
            // drive the query + results list instead of plain text editing.
            let model_search = s.editing
                && s.field == AgentEditField::Model
                && s.selected_provider_is_openrouter(
                    &rest.config,
                    rest.models_cache.as_deref().unwrap_or(&[]),
                );

            if model_search {
                // --- Model omnisearch over the OpenRouter catalogue ---
                let cache = rest.models_cache.as_deref().unwrap_or(&[]);
                match key.code {
                    // Leave search without committing (keeps the existing model).
                    KeyCode::Esc => {
                        s.editing = false;
                        s.model_query = String::new();
                        s.model_result_sel = 0;
                        Action::None
                    }
                    KeyCode::Up => {
                        s.model_result_sel = s.model_result_sel.saturating_sub(1);
                        Action::None
                    }
                    KeyCode::Down => {
                        let n = filter_models(cache, &s.model_query).len();
                        s.model_result_sel =
                            (s.model_result_sel + 1).min(n.saturating_sub(1));
                        Action::None
                    }
                    KeyCode::Enter => {
                        let results = filter_models(cache, &s.model_query);
                        if !results.is_empty() {
                            // Pick the highlighted catalogue model.
                            let sel = s.model_result_sel.min(results.len() - 1);
                            s.draft_model = cache[results[sel]].id.clone();
                        } else {
                            // No-trap fallback: an empty result set commits the raw
                            // query as a manual model id (only when non-empty, so a
                            // bare Enter on an empty box just leaves the model as-is).
                            let typed = s.model_query.trim();
                            if !typed.is_empty() {
                                s.draft_model = typed.to_string();
                            }
                        }
                        s.editing = false;
                        s.model_query = String::new();
                        s.model_result_sel = 0;
                        Action::None
                    }
                    KeyCode::Backspace => {
                        s.model_query.pop();
                        s.model_result_sel = 0;
                        Action::None
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        s.model_query.push(c);
                        s.model_result_sel = 0; // new filter → reset the highlight
                        Action::None
                    }
                    _ => Action::None,
                }
            } else if s.editing {
                // Typing into the highlighted draft field (plain text fields,
                // incl. the Model field for a non-OpenRouter provider).
                // Ctrl+J always inserts a body newline (reliable multiline key).
                if is_ctrl(&key, 'j') {
                    s.newline();
                    return Action::None;
                }
                match key.code {
                    // Commit the field; stay in the editor.
                    KeyCode::Esc => {
                        s.editing = false;
                        Action::None
                    }
                    KeyCode::Enter => {
                        // Shift+Enter (when reported) inserts a body newline;
                        // plain Enter commits the field.
                        if s.field == AgentEditField::Body
                            && key.modifiers.contains(KeyModifiers::SHIFT)
                        {
                            s.newline();
                        } else {
                            s.editing = false;
                        }
                        Action::None
                    }
                    KeyCode::Backspace => {
                        s.backspace();
                        Action::None
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        s.push_char(c);
                        Action::None
                    }
                    _ => Action::None,
                }
            } else {
                // Navigating the field list.
                match key.code {
                    KeyCode::Esc => {
                        s.cancel();
                        Action::None
                    }
                    KeyCode::Up => {
                        s.field_up();
                        Action::None
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        s.field_down();
                        Action::None
                    }
                    KeyCode::Enter => {
                        match s.field {
                            // Tools field uses the picker overlay, not inline editing.
                            AgentEditField::Tools => {
                                s.open_tool_picker();
                            }
                            // Provider field uses the single-select provider picker.
                            AgentEditField::Provider => {
                                s.open_provider_picker(&rest.config.providers);
                            }
                            // Model field on an OpenRouter provider enters the
                            // omnisearch (editing=true with an empty query shows
                            // the search box); every other field, plus the Model
                            // field on a non-OpenRouter provider, is plain text.
                            AgentEditField::Model => {
                                s.model_query = String::new();
                                s.model_result_sel = 0;
                                s.editing = true;
                            }
                            _ => {
                                s.editing = true;
                            }
                        }
                        Action::None
                    }
                    // Scope toggle is only meaningful in Create; bind it to ←/→
                    // so it never collides with field text entry.
                    KeyCode::Left | KeyCode::Right if s.mode == AgentSubMode::Create => {
                        s.toggle_scope();
                        Action::None
                    }
                    // Save (Edit) / create (Create).
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        if s.mode == AgentSubMode::Create {
                            if s.draft_name.trim().is_empty() {
                                rest.status = "name required".into();
                                Action::None
                            } else if s.draft_description.trim().is_empty() {
                                rest.status = "description required".into();
                                Action::None
                            } else {
                                Action::CreateAgent
                            }
                        } else if s.draft_description.trim().is_empty() {
                            rest.status = "description required".into();
                            Action::None
                        } else {
                            Action::SaveAgent
                        }
                    }
                    _ => Action::None,
                }
            }
        }

        // --- Browse: navigate the LIST ---
        AgentSubMode::Browse => match key.code {
            KeyCode::Esc => Action::CloseAgents,
            KeyCode::Up => {
                s.list_up();
                Action::None
            }
            KeyCode::Down | KeyCode::Tab => {
                s.list_down();
                Action::None
            }
            KeyCode::Enter | KeyCode::Right => {
                match s.current_agent().map(|a| a.source) {
                    Some(AgentSource::Builtin) => {
                        rest.status = "built-in agents are read-only".into();
                        Action::None
                    }
                    Some(_) => {
                        s.enter_edit();
                        Action::None
                    }
                    None => Action::None,
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                s.enter_create();
                Action::None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                match s.current_agent().map(|a| a.source) {
                    Some(AgentSource::Builtin) => {
                        rest.status = "cannot delete a built-in agent".into();
                        Action::None
                    }
                    Some(_) => {
                        s.enter_delete();
                        Action::None
                    }
                    None => Action::None,
                }
            }
            _ => Action::None,
        },
    }
}

/// Handle a key press inside the `/effort` reasoning-effort picker.
///
/// Up/Down move the selection; Enter confirms the highlighted option (the
/// runtime stores it, rebuilds the client, and returns to Chat); Esc cancels
/// back to Chat; Ctrl+C quits. `_rest` is accepted for handler-signature
/// consistency but unused.
fn handle_effort(e: &mut EffortPickerState, _rest: &mut AppStateRest, key: KeyEvent) -> Action {
    if is_ctrl(&key, 'c') {
        return Action::Quit;
    }

    match key.code {
        KeyCode::Esc => Action::EffortCancel,
        KeyCode::Up => {
            e.up();
            Action::None
        }
        KeyCode::Down => {
            e.down();
            Action::None
        }
        KeyCode::Enter => match e.selected_option() {
            Some(opt) => Action::SaveEffort(opt.clone()),
            None => Action::EffortCancel,
        },
        _ => Action::None,
    }
}

/// Handle a key press while the startup loading splash is shown.
///
/// `Esc` skips the remaining warm work: mark any still-`Running` step `Skipped`
/// (especially awareness — the slow one this skip exists for) and return
/// [`Action::SkipLoading`] so the runtime drops into Chat immediately. The
/// background warm tasks keep running; their results still populate
/// `AppStateRest` via the `warm_rx` drain even after the skip.
///
/// Every other key is ignored — the splash has no text entry, so a stray key
/// must not crash or leak into the chat input underneath.
fn handle_loading(l: &mut LoadingState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            // Mark non-terminal steps Skipped for correctness (the splash is about
            // to be replaced by Chat, but leaving a step stuck on Running would be
            // wrong if anything reads it). Workspace is included so nothing dangles.
            if matches!(l.workspace, WarmStatus::Running) {
                l.workspace = WarmStatus::Skipped;
            }
            if matches!(l.catalogue, WarmStatus::Running) {
                l.catalogue = WarmStatus::Skipped;
            }
            if matches!(l.awareness, WarmStatus::Running) {
                l.awareness = WarmStatus::Skipped;
            }
            Action::SkipLoading
        }
        // No text entry on the splash: swallow every other key.
        _ => Action::None,
    }
}
