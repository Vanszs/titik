use std::path::PathBuf;

use crate::model::app_config::{AppConfig, ThemeMode};
use crate::model::session::Session;
use crate::view::theme::ACCENTS;

use super::super::SettingField;
use super::picker::{PathPicker, PickerMode};

/// Working state for the in-app `/settings` dashboard.
///
/// Holds editable *drafts* of every settable value; nothing is persisted until
/// the user saves (Esc from the sidebar), at which point the runtime reads these
/// fields back out and applies them.
///
/// Navigation is now THREE-level inside the detail pane for the path-list fields
/// (Workdir, Allowed dirs): `cat` selects a category in the sidebar; `field`
/// selects a row within the category's detail list; for a path-list field,
/// `list_editing` enters per-entry management (`list_sel` highlights a row) and a
/// `picker` overlay drives add/replace via the real filesystem. `in_detail`
/// tracks which pane has keyboard focus. `editing` means typing into a plain
/// text field.
#[derive(Debug, Clone)]
pub struct SettingsState {
    /// Selected category index into [`SETTING_CATEGORIES`](super::SETTING_CATEGORIES).
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
    /// Draft working-directory path list for this session (min 1 entry on save).
    pub workdir: Vec<String>,
    /// Draft: project-awareness summary enabled.
    pub awareness_enabled: bool,
    /// Draft: awareness model source — `true` = inherit the session model,
    /// `false` = use the dedicated awareness model/provider below.
    pub awareness_inherit: bool,
    /// Draft: dedicated awareness model (used when `awareness_inherit` is false).
    pub awareness_model: String,
    /// Draft: dedicated awareness provider (used when `awareness_inherit` is false).
    pub awareness_provider: String,
    /// Draft: safety-harness master switch.
    pub classifier_enabled: bool,
    /// Draft: safety-classifier model.
    pub classifier_model: String,
    /// Draft: safety-classifier provider slug.
    pub classifier_provider: String,
    /// Draft: extra allowed folders as a managed path list. Seeded from
    /// `settings.allowed_folders` (or the launch cwd when empty) and written back
    /// to `Vec<String>` (trim, drop empties) on save.
    pub allowed_folders: Vec<String>,
    /// Draft: short-send token-saver master switch.
    pub short_send_enabled: bool,
    /// Draft: cache-warmth-adaptive summarization toggle.
    pub sliding_cache: bool,
    /// The session's effective working directory, captured at construction. Used
    /// as the base for resolving workspace-relative paths in the FS picker.
    pub cwd: PathBuf,
    /// `true` when the user has entered a path-list field to manage its entries
    /// (one nesting level below field navigation, above the picker).
    pub list_editing: bool,
    /// Highlighted entry row within the active path list (while `list_editing`).
    pub list_sel: usize,
    /// Active filesystem directory picker overlay, if any. When `Some` it has
    /// keyboard focus (deepest nesting level) until confirmed or cancelled.
    pub picker: Option<PathPicker>,
}

impl SettingsState {
    /// Build a dashboard pre-populated from the active session and global config.
    ///
    /// Text drafts come from `session.settings` (and `session.name`); the
    /// theme/accent drafts come from `config`. Starts on the sidebar of the
    /// first category with editing off.
    pub fn from(session: &Session, config: &AppConfig) -> Self {
        let effective_cwd = session.workdir();
        let workdir: Vec<String> = {
            let stored: Vec<String> = session
                .settings
                .workdir
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if stored.is_empty() {
                vec![effective_cwd.display().to_string()]
            } else {
                stored
            }
        };
        let allowed_folders: Vec<String> = if session.settings.allowed_folders.is_empty() {
            std::env::current_dir()
                .map(|p| vec![p.display().to_string()])
                .unwrap_or_else(|_| vec![effective_cwd.display().to_string()])
        } else {
            session.settings.allowed_folders.clone()
        };
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
            workdir,
            awareness_enabled: session.settings.awareness_enabled,
            awareness_inherit: session.settings.awareness_inherit,
            awareness_model: session.settings.awareness_model.clone(),
            awareness_provider: session.settings.awareness_provider.clone(),
            classifier_enabled: session.settings.classifier_enabled,
            classifier_model: session.settings.classifier_model.clone(),
            classifier_provider: session.settings.classifier_provider.clone(),
            allowed_folders,
            short_send_enabled: session.settings.short_send_enabled,
            sliding_cache: session.settings.sliding_cache,
            cwd: effective_cwd,
            list_editing: false,
            list_sel: 0,
            picker: None,
        }
    }

    /// Return the [`SettingField`] currently highlighted in the detail pane.
    pub fn current_field(&self) -> SettingField {
        super::SETTING_CATEGORIES[self.cat].fields[self.field]
    }

    /// Return a mutable reference to the text draft for `f`, or `None` for
    /// non-text fields (Theme, Accent, the awareness toggles).
    pub fn text_draft_mut(&mut self, f: SettingField) -> Option<&mut String> {
        match f {
            SettingField::ApiKey   => Some(&mut self.api_key),
            SettingField::Model    => Some(&mut self.model),
            SettingField::Provider => Some(&mut self.provider),
            SettingField::Name     => Some(&mut self.name),
            SettingField::AwarenessModel if !self.awareness_inherit => {
                Some(&mut self.awareness_model)
            }
            SettingField::AwarenessProvider if !self.awareness_inherit => {
                Some(&mut self.awareness_provider)
            }
            SettingField::ClassifierModel    => Some(&mut self.classifier_model),
            SettingField::ClassifierProvider => Some(&mut self.classifier_provider),
            SettingField::Workdir
            | SettingField::AllowedFolders
            | SettingField::Theme
            | SettingField::Accent
            | SettingField::AwarenessEnabled
            | SettingField::AwarenessSource
            | SettingField::AwarenessModel
            | SettingField::AwarenessProvider
            | SettingField::ClassifierEnabled
            | SettingField::ShortSendEnabled
            | SettingField::SlidingCache => None,
        }
    }

    /// Whether `f` is a managed PATH LIST (Workdir or Allowed dirs).
    pub fn is_path_list(f: SettingField) -> bool {
        matches!(f, SettingField::Workdir | SettingField::AllowedFolders)
    }

    /// Mutable handle to the path-list draft vec for `f`, or `None` if `f` is
    /// not a path-list field.
    pub fn path_list_mut(&mut self, f: SettingField) -> Option<&mut Vec<String>> {
        match f {
            SettingField::Workdir        => Some(&mut self.workdir),
            SettingField::AllowedFolders => Some(&mut self.allowed_folders),
            _ => None,
        }
    }

    /// Immutable handle to the path-list draft vec for `f` (view-side reads).
    pub fn path_list(&self, f: SettingField) -> Option<&Vec<String>> {
        match f {
            SettingField::Workdir        => Some(&self.workdir),
            SettingField::AllowedFolders => Some(&self.allowed_folders),
            _ => None,
        }
    }

    /// Move the cursor up.
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
    pub fn down(&mut self) {
        if self.in_detail {
            let max = super::SETTING_CATEGORIES[self.cat].fields.len().saturating_sub(1);
            if self.field < max {
                self.field += 1;
            }
        } else {
            let max = super::SETTING_CATEGORIES.len().saturating_sub(1);
            if self.cat < max {
                self.cat += 1;
                self.field = 0;
            }
        }
    }

    /// Move focus to the detail pane (only if the current category has fields).
    pub fn focus_detail(&mut self) {
        if !super::SETTING_CATEGORIES[self.cat].fields.is_empty() {
            self.in_detail = true;
            self.field = 0;
        }
    }

    /// Return focus to the sidebar; also exits editing/list/picker modes.
    pub fn focus_sidebar(&mut self) {
        self.in_detail = false;
        self.editing = false;
        self.list_editing = false;
        self.list_sel = 0;
        self.picker = None;
    }

    /// Act on Enter while in the detail pane.
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
                if !self.awareness_inherit {
                    self.editing = true;
                }
            }
            SettingField::ClassifierEnabled => {
                self.classifier_enabled = !self.classifier_enabled;
            }
            SettingField::ShortSendEnabled => {
                self.short_send_enabled = !self.short_send_enabled;
            }
            SettingField::SlidingCache => {
                self.sliding_cache = !self.sliding_cache;
            }
            SettingField::Workdir | SettingField::AllowedFolders => {
                self.list_editing = true;
                self.list_sel = 0;
            }
            _ => {
                self.editing = true;
            }
        }
    }

    // --- Path-list management ---

    /// Move the highlighted list entry up (clamps at 0).
    pub fn list_up(&mut self) {
        self.list_sel = self.list_sel.saturating_sub(1);
    }

    /// Move the highlighted list entry down (clamps at the last entry).
    pub fn list_down(&mut self) {
        let len = self
            .path_list(self.current_field())
            .map(|v| v.len())
            .unwrap_or(0);
        if self.list_sel + 1 < len {
            self.list_sel += 1;
        }
    }

    /// Remove the highlighted entry, honouring the min-1 rule.
    pub fn list_remove(&mut self) {
        let f = self.current_field();
        let sel = self.list_sel;
        if let Some(v) = self.path_list_mut(f) {
            if v.len() > 1 && sel < v.len() {
                v.remove(sel);
            }
        }
        let len = self.path_list(f).map(|v| v.len()).unwrap_or(0);
        if self.list_sel >= len {
            self.list_sel = len.saturating_sub(1);
        }
    }

    /// Open the FS picker in ADD mode.
    pub fn open_picker_add(&mut self) {
        self.picker = Some(PathPicker::new(PickerMode::Add, String::new(), &self.cwd));
    }

    /// Open the FS picker in REPLACE mode for the highlighted entry.
    pub fn open_picker_replace(&mut self) {
        let f = self.current_field();
        let sel = self.list_sel;
        let seed = self
            .path_list(f)
            .and_then(|v| v.get(sel))
            .cloned()
            .unwrap_or_default();
        self.picker = Some(PathPicker::new(PickerMode::Replace(sel), seed, &self.cwd));
    }

    /// Confirm the active picker: apply the chosen path to the target list.
    pub fn picker_confirm(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let chosen = picker
            .selected()
            .cloned()
            .unwrap_or_else(|| picker.query.clone());
        let chosen = chosen.strip_prefix('@').unwrap_or(&chosen).trim().to_string();
        if chosen.is_empty() {
            return;
        }
        let f = self.current_field();
        match picker.mode {
            PickerMode::Add => {
                if let Some(v) = self.path_list_mut(f) {
                    v.push(chosen);
                    self.list_sel = v.len().saturating_sub(1);
                }
            }
            PickerMode::Replace(i) => {
                if let Some(v) = self.path_list_mut(f) {
                    if let Some(slot) = v.get_mut(i) {
                        *slot = chosen;
                    }
                }
            }
        }
    }

    /// Cancel the active picker without applying anything.
    pub fn picker_cancel(&mut self) {
        self.picker = None;
    }

    /// Append `c` to the picker query and recompute matches.
    pub fn picker_push_char(&mut self, c: char) {
        let cwd = self.cwd.clone();
        if let Some(p) = self.picker.as_mut() {
            p.query.push(c);
            p.recompute(&cwd);
        }
    }

    /// Delete the last char of the picker query and recompute matches.
    pub fn picker_backspace(&mut self) {
        let cwd = self.cwd.clone();
        if let Some(p) = self.picker.as_mut() {
            p.query.pop();
            p.recompute(&cwd);
        }
    }

    /// Drill into the currently highlighted match.
    pub fn picker_descend(&mut self) {
        let cwd = self.cwd.clone();
        if let Some(p) = self.picker.as_mut() {
            if let Some(sel) = p.selected().cloned() {
                p.query = format!("{sel}/");
                p.sel = 0;
                p.recompute(&cwd);
            }
        }
    }

    /// Append `c` to the draft of the current text field.
    pub fn push_char(&mut self, c: char) {
        let f = self.current_field();
        if let Some(s) = self.text_draft_mut(f) {
            s.push(c);
        }
    }

    /// Delete the last character from the current text field's draft.
    pub fn backspace(&mut self) {
        let f = self.current_field();
        if let Some(s) = self.text_draft_mut(f) {
            s.pop();
        }
    }

    /// Cycle the accent draft to the next/previous entry in [`ACCENTS`], wrapping.
    pub fn cycle_accent(&mut self, forward: bool) {
        let len = ACCENTS.len();
        if len == 0 {
            return;
        }
        let cur = ACCENTS.iter().position(|a| *a == self.accent).unwrap_or(0);
        let next = if forward {
            (cur + 1) % len
        } else {
            (cur + len - 1) % len
        };
        self.accent = ACCENTS[next].to_string();
    }
}
