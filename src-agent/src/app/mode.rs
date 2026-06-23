//! App – UI mode enum and associated state types.
//!
//! The app is always in exactly one of three modes, represented by [`Mode`]:
//!
//! | Variant          | Meaning                                       |
//! |-----------------|-----------------------------------------------|
//! | `KeyInput`       | Credentials form (api key + model)            |
//! | `SessionPicker`  | `--resume` session list with live search      |
//! | `Chat`           | Normal conversation view                      |
//!
//! Mode-specific state is stored inline in the variant so the type system
//! ensures the runtime can only access data that is relevant to the active
//! mode.  Both [`KeyInputForm`] and [`PickerState`] live here; `Chat` carries
//! no extra state beyond `AppStateRest`.

use crate::model::store::SessionMeta;

/// The three mutually-exclusive UI modes of the application.
pub enum Mode {
    /// Credentials form: collects api key and model name before a session can
    /// start.  The inner [`KeyInputForm`] holds the in-progress field values
    /// and tracks which field is focused.
    KeyInput(KeyInputForm),
    /// `--resume` session picker: shows saved sessions and a live search bar.
    SessionPicker(PickerState),
    /// Normal chat view: messages are rendered and the user types in the
    /// input bar.  All chat-specific state lives in `AppStateRest`.
    Chat,
}

/// Transient state for the credentials input form.
///
/// Created with [`KeyInputForm::prefilled`]; fields are edited in place via
/// `push_char` / `backspace`; the controller reads `field` to know which
/// entry is active. Three fields in order: api_key (0), model (1), provider (2).
#[derive(Debug, Clone, Default)]
pub struct KeyInputForm {
    pub api_key: String,
    pub model: String,
    /// OpenRouter provider slug for strict-pin routing (optional, may be empty).
    pub provider: String,
    /// Active field index: `0` = api_key, `1` = model, `2` = provider.
    pub field: usize,
    /// `true` when no prior session / configured client exists.
    /// Controls Esc behaviour: if true, Esc must quit (there is no Chat view
    /// to return to).
    pub first_run: bool,
    /// `true` when this form was entered from the `--resume` session picker.
    /// Esc returns to the picker instead of Quit / Chat.
    pub from_picker: bool,
}

impl KeyInputForm {
    /// Construct a form pre-populated with existing credentials.
    ///
    /// - `first_run = true`:   Esc quits (no usable Chat fallback).
    /// - `from_picker = true`: Esc returns to the session picker.
    /// - `provider`:           OpenRouter provider slug (may be empty for default routing).
    pub fn prefilled(
        api_key: String,
        model: String,
        provider: String,
        first_run: bool,
        from_picker: bool,
    ) -> Self {
        Self {
            api_key,
            model,
            provider,
            field: 0,
            first_run,
            from_picker,
        }
    }

    /// Advance to the next field (clamps at the last field, index 2).
    pub fn next_field(&mut self) {
        if self.field < 2 {
            self.field += 1;
        }
    }

    /// Move to the previous field (clamps at zero).
    pub fn prev_field(&mut self) {
        if self.field > 0 {
            self.field -= 1;
        }
    }

    /// Append a character to whichever field is currently active.
    pub fn push_char(&mut self, c: char) {
        match self.field {
            0 => self.api_key.push(c),
            1 => self.model.push(c),
            _ => self.provider.push(c),
        }
    }

    /// Delete the last character from the active field.
    pub fn backspace(&mut self) {
        match self.field {
            0 => { self.api_key.pop(); }
            1 => { self.model.pop(); }
            _ => { self.provider.pop(); }
        };
    }

    /// Returns `true` when the cursor is on the last field (provider, index 2).
    ///
    /// Used by the controller to decide whether Enter should advance the
    /// cursor or submit the form.
    pub fn is_last(&self) -> bool {
        self.field == 2
    }
}

/// State for the `--resume` session picker.
///
/// `all` holds every discovered session; `filtered_idx` is a subset of
/// indices into `all` that match the current `query`.  `selected` is an
/// index into `filtered_idx` (not into `all`).
pub struct PickerState {
    /// The user's live search string (updated on every keypress).
    pub query: String,
    /// All available sessions, unfiltered, in discovery order.
    pub all: Vec<SessionMeta>,
    /// Indices into `all` of sessions that match the current `query`.
    /// Empty query → all sessions included (same order as `all`).
    pub filtered_idx: Vec<usize>,
    /// Cursor position within `filtered_idx` (not within `all`).
    pub selected: usize,
}

impl PickerState {
    /// Initialise the picker with all known sessions and run the first filter
    /// pass (which with an empty query just includes everything).
    pub fn new(all: Vec<SessionMeta>) -> Self {
        let mut s = Self {
            query: String::new(),
            all,
            filtered_idx: vec![],
            selected: 0,
        };
        s.refilter();
        s
    }

    /// Rebuild `filtered_idx` from `all` using the current `query`.
    ///
    /// Matching is case-insensitive substring search on both the session name
    /// and session id.  After filtering, `selected` is clamped to the last
    /// valid index so it never points outside the filtered list — for example
    /// when a filter narrows the list below the previous cursor position.
    pub fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered_idx = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                q.is_empty()
                    || m.name.to_lowercase().contains(&q)
                    || m.id.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();

        // Clamp `selected` so it remains a valid index after the list shrinks.
        // `saturating_sub(1)` handles the empty-list case (gives 0, which is
        // the only safe value when `filtered_idx` is empty).
        if self.selected >= self.filtered_idx.len() {
            self.selected = self.filtered_idx.len().saturating_sub(1);
        }
    }

    /// Move the cursor up one row (clamps at 0).
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the cursor down one row (clamps at the last filtered entry).
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.filtered_idx.len() {
            self.selected += 1;
        }
    }

    /// Return a reference to the metadata of the currently highlighted session,
    /// or `None` if the filtered list is empty.
    pub fn selected_meta(&self) -> Option<&SessionMeta> {
        self.filtered_idx
            .get(self.selected)
            .and_then(|&i| self.all.get(i))
    }
}
