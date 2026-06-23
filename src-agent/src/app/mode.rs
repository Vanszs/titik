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

/// A single editable/toggleable field within a settings category.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SettingField {
    ApiKey,
    Model,
    Provider,
    Theme,
    Accent,
    Name,
    Workdir,
    /// Toggle: whether the project-awareness summary is generated/injected.
    AwarenessEnabled,
    /// Toggle: awareness model source — inherit the session model or use the
    /// dedicated awareness model/provider.
    AwarenessSource,
    /// Text: dedicated awareness model (ignored when the source is "inherit").
    AwarenessModel,
    /// Text: dedicated awareness provider (ignored when the source is "inherit").
    AwarenessProvider,
}

impl SettingField {
    /// Human-readable label shown in the detail pane.
    pub fn label(self) -> &'static str {
        match self {
            SettingField::ApiKey            => "API key",
            SettingField::Model             => "Model",
            SettingField::Provider          => "Provider",
            SettingField::Theme             => "Theme",
            SettingField::Accent            => "Accent",
            SettingField::Name              => "Session name",
            SettingField::Workdir           => "Workdir",
            SettingField::AwarenessEnabled  => "Awareness",
            SettingField::AwarenessSource   => "Model source",
            SettingField::AwarenessModel    => "Aware model",
            SettingField::AwarenessProvider => "Aware provider",
        }
    }
}

/// A named group of related settings fields shown in the sidebar.
pub struct SettingCategory {
    pub name: &'static str,
    pub fields: &'static [SettingField],
}

/// All settings categories in sidebar display order.
///
/// Adding a new category or field here is sufficient — the view and input
/// handler iterate over this slice generically.
pub const SETTING_CATEGORIES: &[SettingCategory] = &[
    SettingCategory {
        name: "Connection",
        fields: &[SettingField::ApiKey, SettingField::Model, SettingField::Provider],
    },
    SettingCategory {
        name: "Appearance",
        fields: &[SettingField::Theme, SettingField::Accent],
    },
    SettingCategory {
        name: "Session",
        fields: &[SettingField::Name, SettingField::Workdir],
    },
    SettingCategory {
        name: "Awareness",
        fields: &[
            SettingField::AwarenessEnabled,
            SettingField::AwarenessSource,
            SettingField::AwarenessModel,
            SettingField::AwarenessProvider,
        ],
    },
];

/// Working state for the in-app `/settings` dashboard.
///
/// Holds editable *drafts* of every settable value; nothing is persisted until
/// the user saves (Esc from the sidebar), at which point the runtime reads these
/// fields back out and applies them.
///
/// Navigation is two-level: `cat` selects a category in the sidebar; `field`
/// selects a row within the category's detail list. `in_detail` tracks which
/// pane has keyboard focus. `editing` means the user is typing into a text field.
#[derive(Debug, Clone)]
pub struct SettingsState {
    /// Selected category index into [`SETTING_CATEGORIES`].
    pub cat: usize,
    /// Selected field index within `SETTING_CATEGORIES[cat].fields`.
    pub field: usize,
    /// `false` = focus on the sidebar; `true` = focus on the detail field list.
    pub in_detail: bool,
    /// `true` while typing into a text field; `false` while navigating.
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
    /// Draft working directory for this session.
    pub workdir: String,
    /// Draft: project-awareness summary enabled.
    pub awareness_enabled: bool,
    /// Draft: awareness model source — `true` = inherit the session model,
    /// `false` = use the dedicated awareness model/provider below.
    pub awareness_inherit: bool,
    /// Draft: dedicated awareness model (used when `awareness_inherit` is false).
    pub awareness_model: String,
    /// Draft: dedicated awareness provider (used when `awareness_inherit` is false).
    pub awareness_provider: String,
}

impl SettingsState {
    /// Build a dashboard pre-populated from the active session and global config.
    ///
    /// Text drafts come from `session.settings` (and `session.name`); the
    /// theme/accent drafts come from `config`. Starts on the sidebar of the
    /// first category with editing off.
    pub fn from(session: &Session, config: &AppConfig) -> Self {
        Self {
            cat: 0,
            field: 0,
            in_detail: false,
            editing: false,
            api_key: session.settings.api_key.clone(),
            model: session.settings.model.clone(),
            provider: session.settings.provider.clone(),
            name: session.name.clone(),
            theme: config.theme.clone(),
            accent: config.accent.clone(),
            workdir: session.settings.workdir.clone(),
            awareness_enabled: session.settings.awareness_enabled,
            awareness_inherit: session.settings.awareness_inherit,
            awareness_model: session.settings.awareness_model.clone(),
            awareness_provider: session.settings.awareness_provider.clone(),
        }
    }

    /// Return the [`SettingField`] currently highlighted in the detail pane.
    pub fn current_field(&self) -> SettingField {
        SETTING_CATEGORIES[self.cat].fields[self.field]
    }

    /// Return a mutable reference to the text draft for `f`, or `None` for
    /// non-text fields (Theme, Accent, the awareness toggles).
    ///
    /// The awareness model/provider are text fields ONLY when the source is
    /// "custom" (`awareness_inherit == false`); while inheriting they return
    /// `None` so they can't be edited (push/backspace and Enter all no-op),
    /// matching their dimmed "(inherited)" display.
    pub fn text_draft_mut(&mut self, f: SettingField) -> Option<&mut String> {
        match f {
            SettingField::ApiKey   => Some(&mut self.api_key),
            SettingField::Model    => Some(&mut self.model),
            SettingField::Provider => Some(&mut self.provider),
            SettingField::Name     => Some(&mut self.name),
            SettingField::Workdir  => Some(&mut self.workdir),
            SettingField::AwarenessModel if !self.awareness_inherit => {
                Some(&mut self.awareness_model)
            }
            SettingField::AwarenessProvider if !self.awareness_inherit => {
                Some(&mut self.awareness_provider)
            }
            SettingField::Theme
            | SettingField::Accent
            | SettingField::AwarenessEnabled
            | SettingField::AwarenessSource
            | SettingField::AwarenessModel
            | SettingField::AwarenessProvider => None,
        }
    }

    /// Move the cursor up.
    ///
    /// In the sidebar: step `cat` up (clamp at 0) and reset `field` to 0.
    /// In the detail pane: step `field` up within the current category (clamp at 0).
    pub fn up(&mut self) {
        if self.in_detail {
            self.field = self.field.saturating_sub(1);
        } else {
            let prev = self.cat;
            self.cat = self.cat.saturating_sub(1);
            if self.cat != prev {
                self.field = 0;
            }
        }
    }

    /// Move the cursor down.
    ///
    /// In the sidebar: step `cat` down (clamp at last category) and reset `field` to 0.
    /// In the detail pane: step `field` down within the current category (clamp at last).
    pub fn down(&mut self) {
        if self.in_detail {
            let max = SETTING_CATEGORIES[self.cat].fields.len().saturating_sub(1);
            if self.field < max {
                self.field += 1;
            }
        } else {
            let max = SETTING_CATEGORIES.len().saturating_sub(1);
            if self.cat < max {
                self.cat += 1;
                self.field = 0;
            }
        }
    }

    /// Move focus to the detail pane (only if the current category has fields).
    ///
    /// Resets `field` to 0.
    pub fn focus_detail(&mut self) {
        if !SETTING_CATEGORIES[self.cat].fields.is_empty() {
            self.in_detail = true;
            self.field = 0;
        }
    }

    /// Return focus to the sidebar; also exits editing mode.
    pub fn focus_sidebar(&mut self) {
        self.in_detail = false;
        self.editing = false;
    }

    /// Act on Enter while in the detail pane.
    ///
    /// - Theme → toggle dark/light (no edit mode).
    /// - Accent → no-op (arrows cycle it instead).
    /// - AwarenessEnabled → toggle on/off.
    /// - AwarenessSource → toggle inherit/custom.
    /// - AwarenessModel / AwarenessProvider → edit only when source is "custom";
    ///   a no-op while inheriting (the values are irrelevant then).
    /// - Other text fields → enter editing mode.
    ///
    /// No-op when not in the detail pane.
    pub fn enter(&mut self) {
        if !self.in_detail {
            return;
        }
        match self.current_field() {
            SettingField::Theme => {
                self.theme = match self.theme {
                    ThemeMode::Dark  => ThemeMode::Light,
                    ThemeMode::Light => ThemeMode::Dark,
                };
            }
            SettingField::Accent => {
                // Accent is cycled with arrow keys; Enter is intentionally a no-op.
            }
            SettingField::AwarenessEnabled => {
                self.awareness_enabled = !self.awareness_enabled;
            }
            SettingField::AwarenessSource => {
                self.awareness_inherit = !self.awareness_inherit;
            }
            SettingField::AwarenessModel | SettingField::AwarenessProvider => {
                // Editable only as a "custom" source; while inheriting the
                // dedicated model/provider are irrelevant, so Enter is a no-op.
                if !self.awareness_inherit {
                    self.editing = true;
                }
            }
            _ => {
                self.editing = true;
            }
        }
    }

    /// Append `c` to the draft of the current text field.
    ///
    /// No-op for non-text fields (Theme, Accent).
    pub fn push_char(&mut self, c: char) {
        let f = self.current_field();
        if let Some(s) = self.text_draft_mut(f) {
            s.push(c);
        }
    }

    /// Delete the last character from the current text field's draft.
    ///
    /// No-op for non-text fields (Theme, Accent).
    pub fn backspace(&mut self) {
        let f = self.current_field();
        if let Some(s) = self.text_draft_mut(f) {
            s.pop();
        }
    }

    /// Cycle the accent draft to the next/previous entry in [`ACCENTS`], wrapping.
    ///
    /// `forward = true` steps to the next accent, `false` to the previous. If the
    /// current draft isn't a known accent name, this resets to the first entry.
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
