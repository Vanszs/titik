//! Controller – keyboard input handler ("C" in MVC).
//!
//! Every raw [`crossterm::event::KeyEvent`] that the event loop receives is
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

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crate::app::mode::{KeyInputForm, Mode, PickerState};
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
    // --- KeyInput actions ---
    /// Credentials form confirmed; carry the api key and model string out.
    SaveCreds { api_key: String, model: String },
    /// Esc on a credentials form that was NOT opened from the picker — return
    /// to the normal Chat view.
    CancelKeyInput,
    /// Esc from a KeyInput that was opened from the --resume picker: go back to
    /// the picker rather than pinning a no-client Chat.
    CancelKeyInputToPicker,
    // --- Picker actions ---
    /// Enter on the session picker — open the highlighted session.
    PickerSelect,
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
    }
}

/// Returns `true` when `key` is the given ASCII `c` held with Ctrl.
fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(x) if x == c)
}

/// Handle a key press while the app is in Chat mode.
///
/// Ctrl+C and Esc both interrupt an in-flight request when `waiting` is true;
/// when idle they quit the app.  Ctrl+R re-sends the last message (idle only).
fn handle_chat(rest: &mut AppStateRest, key: KeyEvent) -> Action {
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

    match key.code {
        KeyCode::Esc => {
            if rest.waiting {
                Action::Interrupt
            } else {
                Action::Quit
            }
        }
        KeyCode::Enter => {
            if rest.input.trim().starts_with('/') {
                let line = rest.take_input();
                Action::Slash(command::parse(&line))
            } else if !rest.input.trim().is_empty() && !rest.waiting {
                Action::Submit(rest.take_input())
            } else {
                Action::None
            }
        }
        KeyCode::Backspace => {
            rest.backspace();
            Action::None
        }
        KeyCode::Up => {
            rest.scroll_up();
            Action::None
        }
        KeyCode::Down => {
            rest.scroll_down();
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
