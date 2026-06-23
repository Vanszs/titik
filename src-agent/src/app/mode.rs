//! App – UI mode enum and associated state types.
//!
//! The app is always in exactly one of four modes, represented by [`Mode`]:
//!
//! | Variant          | Meaning                                       |
//! |-----------------|-----------------------------------------------|
//! | `KeyInput`       | Credentials form (api key + model)            |
//! | `SessionPicker`  | `--resume` session list with live search      |
//! | `Chat`           | Normal conversation view                      |
//! | `Settings`       | In-app `/settings` dashboard                  |
//!
//! Mode-specific state is stored inline in the variant so the type system
//! ensures the runtime can only access data that is relevant to the active
//! mode.  [`KeyInputForm`], [`PickerState`], and [`SettingsState`] live here;
//! `Chat` carries no extra state beyond `AppStateRest`.

use crate::model::app_config::{AppConfig, ThemeMode};
use crate::model::session::Session;
use crate::model::store::SessionMeta;
use crate::view::theme::ACCENTS;

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
    /// In-app settings dashboard (`/settings`): edit per-session credentials,
    /// the session name, and the global theme/accent. The inner
    /// [`SettingsState`] holds working drafts that are applied on save.
    Settings(SettingsState),
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

/// Working state for the in-app `/settings` dashboard.
///
/// Holds editable *drafts* of every settable value; nothing is persisted until
/// the user saves (Esc), at which point the runtime reads these fields back out
/// and applies them. `selected` indexes the five sections in display order:
///
/// | index | section      | kind            |
/// |-------|--------------|-----------------|
/// | 0     | API key      | text            |
/// | 1     | Model        | text            |
/// | 2     | Provider     | text            |
/// | 3     | Theme        | toggle + cycle  |
/// | 4     | Session name | text            |
///
/// Text rows (0/1/2/4) are edited in place once `editing` is set; the Theme row
/// (3) never enters `editing` — Enter toggles dark/light and ←/→ cycle the
/// accent instead.
#[derive(Debug, Clone)]
pub struct SettingsState {
    /// Active section (0=API key, 1=Model, 2=Provider, 3=Theme, 4=Session name).
    pub selected: usize,
    /// `true` while typing into a text row; `false` while navigating sections.
    pub editing: bool,
    /// Draft API key (session-scoped).
    pub api_key: String,
    /// Draft OpenRouter model identifier.
    pub model: String,
    /// Draft OpenRouter provider slug (may be empty for default routing).
    pub provider: String,
    /// Draft session display name (applied via `rename_session` on save).
    pub name: String,
    /// Draft global theme mode.
    pub theme: ThemeMode,
    /// Draft global accent name (one of [`ACCENTS`]).
    pub accent: String,
}

impl SettingsState {
    /// Build a dashboard pre-populated from the active session and global config.
    ///
    /// Text drafts come from `session.settings` (and `session.name`); the
    /// theme/accent drafts come from `config`. Starts on the first section with
    /// editing off.
    pub fn from(session: &Session, config: &AppConfig) -> Self {
        Self {
            selected: 0,
            editing: false,
            api_key: session.settings.api_key.clone(),
            model: session.settings.model.clone(),
            provider: session.settings.provider.clone(),
            name: session.name.clone(),
            theme: config.theme.clone(),
            accent: config.accent.clone(),
        }
    }

    /// Move the section cursor up one row (clamps at 0).
    ///
    /// Only meaningful while navigating; the caller guards against calling this
    /// while `editing`.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the section cursor down one row (clamps at the last section, 4).
    pub fn down(&mut self) {
        if self.selected < 4 {
            self.selected += 1;
        }
    }

    /// Act on Enter for the current section.
    ///
    /// On the Theme row (3) this toggles dark/light without entering edit mode;
    /// on every text row it starts editing so subsequent keystrokes append to
    /// the draft.
    pub fn enter(&mut self) {
        if self.selected == 3 {
            self.theme = match self.theme {
                ThemeMode::Dark => ThemeMode::Light,
                ThemeMode::Light => ThemeMode::Dark,
            };
        } else {
            self.editing = true;
        }
    }

    /// Append `c` to the draft of the currently-selected text row.
    ///
    /// No-op for the Theme row (3), which has no free-text value.
    pub fn push_char(&mut self, c: char) {
        match self.selected {
            0 => self.api_key.push(c),
            1 => self.model.push(c),
            2 => self.provider.push(c),
            4 => self.name.push(c),
            _ => {}
        }
    }

    /// Delete the last character from the currently-selected text row's draft.
    ///
    /// No-op for the Theme row (3).
    pub fn backspace(&mut self) {
        match self.selected {
            0 => { self.api_key.pop(); }
            1 => { self.model.pop(); }
            2 => { self.provider.pop(); }
            4 => { self.name.pop(); }
            _ => {}
        };
    }

    /// Cycle the accent draft to the next/previous entry in [`ACCENTS`], wrapping.
    ///
    /// `forward = true` steps to the next accent, `false` to the previous. If the
    /// current draft isn't a known accent name, this resets to the first entry.
    /// Only invoked on the Theme row (3).
    pub fn cycle_accent(&mut self, forward: bool) {
        let len = ACCENTS.len();
        if len == 0 {
            return;
        }
        let cur = ACCENTS.iter().position(|a| *a == self.accent).unwrap_or(0);
        let next = if forward {
            (cur + 1) % len
        } else {
            // +len before the modulo so subtracting 1 at index 0 wraps to the end
            // instead of underflowing the unsigned index.
            (cur + len - 1) % len
        };
        self.accent = ACCENTS[next].to_string();
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
