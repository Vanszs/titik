use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crate::app::mode::{KeyInputForm, Mode, PickerState};
use crate::app::state::{AppState, AppStateRest};
use crate::controller::command::{self, Command};

pub enum Action {
    None,
    Quit,
    // Chat
    Submit(String),
    Slash(Command),
    Interrupt,
    Resend,
    // KeyInput
    SaveCreds { api_key: String, model: String },
    CancelKeyInput,
    /// Esc from a KeyInput that was opened from the --resume picker: go back to
    /// the picker rather than pinning a no-client Chat.
    CancelKeyInputToPicker,
    // Picker
    PickerSelect,
}

/// Disjoint-field borrow: &mut state.mode and &mut state.rest are independent
/// places, so the borrow checker accepts simultaneous mutable borrows.
pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Action {
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }
    match &mut state.mode {
        Mode::Chat => handle_chat(&mut state.rest, key),
        Mode::KeyInput(form) => handle_key_input(form, &mut state.rest, key),
        Mode::SessionPicker(p) => handle_picker(p, &mut state.rest, key),
    }
}

fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(x) if x == c)
}

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

fn handle_key_input(form: &mut KeyInputForm, rest: &mut AppStateRest, key: KeyEvent) -> Action {
    if is_ctrl(&key, 'c') {
        return Action::Quit;
    }

    match key.code {
        KeyCode::Esc => {
            if form.first_run {
                Action::Quit
            } else if form.from_picker {
                Action::CancelKeyInputToPicker
            } else {
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
