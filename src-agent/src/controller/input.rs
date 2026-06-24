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
use crate::app::mode::{KeyInputForm, Mode, PickerState, SettingsState};
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
    /// Credentials form confirmed; carry the api key, model, and provider out.
    SaveCreds { api_key: String, model: String, provider: String },
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
    }
}

/// Insert pasted text into the active input. In Chat the text is inserted
/// verbatim (newlines kept → multiline input, never a submit); single-line
/// fields strip newlines. `\r` is always dropped (paste may use CRLF).
pub fn handle_paste(state: &mut AppState, text: &str) {
    match &mut state.mode {
        Mode::Chat => {
            for c in text.chars() {
                if c != '\r' {
                    state.rest.push_char(c); // '\n' kept → newline in the input
                }
            }
        }
        Mode::KeyInput(form) => {
            for c in text.chars() {
                if c != '\r' && c != '\n' {
                    form.push_char(c);
                }
            }
        }
        Mode::Settings(s) => {
            if s.editing {
                for c in text.chars() {
                    if c != '\r' && c != '\n' {
                        s.push_char(c);
                    }
                }
            }
        }
        Mode::SessionPicker(_) => {}
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

    // Ctrl+C: interrupt if waiting, else quit.
    if is_ctrl(&key, 'c') {
        return if rest.waiting {
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
            if rest.waiting {
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
        KeyCode::Up => {
            // Command palette takes precedence; then file palette; then history recall.
            if !command::palette_matches(&rest.input).is_empty() {
                rest.palette_sel = rest.palette_sel.saturating_sub(1);
            } else {
                let fmatches: Vec<String> = file_ref_partial(&rest.input)
                    .map(|p| rest.dir_cache.read().map(|c| c.search(p, FILE_PAL_MAX)).unwrap_or_default())
                    .unwrap_or_default();
                if !fmatches.is_empty() {
                    rest.palette_sel = rest.palette_sel.saturating_sub(1);
                } else {
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
                } else {
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
        // End / Ctrl+End: jump to the bottom and resume following.
        KeyCode::End => {
            rest.reset_scroll();
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

/// Handle a key press while the credentials form is active.
///
/// Esc routing has three cases:
/// 1. `first_run = true` → no prior client exists, so Esc must quit rather than drop back to a broken Chat view.
/// 2. `from_picker = true` → form was opened from the `--resume` session picker, so Esc returns there (`CancelKeyInputToPicker`).
/// 3. Otherwise → Esc cancels back to the existing Chat view.
fn handle_key_input(form: &mut KeyInputForm, rest: &mut AppStateRest, key: KeyEvent) -> Action {
    if is_ctrl(&key, 'c') {
        return Action::Quit;
    }

    match key.code {
        KeyCode::Esc => {
            if form.first_run {
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
        KeyCode::Tab | KeyCode::Down => {
            form.next_field();
            Action::None
        }
        KeyCode::Up => {
            form.prev_field();
            Action::None
        }
        KeyCode::Enter => {
            if form.is_last() {
                if form.api_key.trim().is_empty() {
                    rest.status = "api key required".into();
                    Action::None
                } else {
                    Action::SaveCreds {
                        api_key: form.api_key.trim().to_string(),
                        model: form.model.trim().to_string(),
                        provider: form.provider.trim().to_string(),
                    }
                }
            } else {
                form.next_field();
                Action::None
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
/// Three-level focus design:
///
/// 1. **editing** – user is typing into a text field.
///    Enter / Esc commit the draft and drop back to detail navigation.
///    Backspace / Char delegate to the state mutation helpers.
///
/// 2. **in_detail** (not editing) – cursor is on the field list of the active
///    category.  Esc / Left return focus to the sidebar.  Enter activates the
///    current field.  Left/Right on the Accent field cycle the accent; Left
///    otherwise returns to the sidebar.
///
/// 3. **sidebar** – cursor is on the category list.
///    Esc saves all drafts and closes the dashboard (`Action::SaveSettings`).
///    Enter / Right move focus to the detail pane.
///
/// `_rest` is accepted for handler-signature consistency but unused.
fn handle_settings(s: &mut SettingsState, _rest: &mut AppStateRest, key: KeyEvent) -> Action {
    use crate::app::mode::SettingField;

    if is_ctrl(&key, 'c') {
        return Action::Quit;
    }

    if s.editing {
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
        match key.code {
            // Return to the sidebar (also exits editing, already false here).
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
            // Theme toggle / start editing text field.
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
