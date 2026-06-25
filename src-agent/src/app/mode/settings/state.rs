use std::path::PathBuf;

use crate::model::app_config::{AppConfig, ThemeMode};
use crate::model::session::Session;
use crate::view::theme::ACCENTS;

use super::super::SettingField;
use super::picker::{PathPicker, PickerMode};
use super::{ModelDraft, ModelField, ModelModal, ModelRole, ProviderDraft, ProviderModal};

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
    /// In-memory list of API provider drafts (stub only, not persisted).
    pub providers: Vec<ProviderDraft>,
    /// Selected row in the providers list. Index == `providers.len()` means the
    /// `[+ add]` button row is highlighted.
    pub prov_sel: usize,
    /// `true` after the first Ctrl+X: next Ctrl+X confirms the delete.
    pub prov_delete_armed: bool,
    /// Active add-provider modal, if open.
    pub prov_modal: Option<ProviderModal>,
    /// In-memory list of model drafts (stub only, not persisted).
    pub models: Vec<ModelDraft>,
    /// Selected row in the models list. Index == `models.len()` means the
    /// `[+ add model]` button row is highlighted.
    pub model_sel: usize,
    /// `true` after the first Ctrl+X on a model row: next Ctrl+X confirms.
    pub model_delete_armed: bool,
    /// Active add/edit-model modal, if open.
    pub model_modal: Option<ModelModal>,
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
        // Provider drafts come straight from the global catalogue (empty on a
        // fresh install — no demo seeds).
        let providers: Vec<ProviderDraft> = config
            .providers
            .iter()
            .map(|p| ProviderDraft {
                uuid: p.uuid.clone(),
                name: p.name.clone(),
                endpoint: p.endpoint.clone(),
                api_type: p.api_type,
                api_key: p.api_key.clone(),
            })
            .collect();
        // Model drafts: global catalogue entries (session_only = false) followed
        // by this session's override-layer models (session_only = true). Each
        // entry's `provider_uuid` is resolved back to a positional `provider_idx`
        // against the providers built above; a dangling uuid (provider deleted
        // out-of-band) falls back to idx 0 so the row surfaces for re-pick rather
        // than vanishing.
        let map_entry = |m: &crate::model::app_config::ModelEntry, session_only: bool| ModelDraft {
            uuid: m.uuid.clone(),
            name: m.name.clone(),
            model_id: m.model_id.clone(),
            provider_idx: config.provider_index_by_uuid(&m.provider_uuid).unwrap_or(0),
            // Fold the legacy single-role field into the multi-role list on load.
            roles: m.effective_roles(),
            route: m.route.clone(),
            session_only,
        };
        let mut models: Vec<ModelDraft> =
            config.models.iter().map(|m| map_entry(m, false)).collect();
        models.extend(
            session
                .settings
                .session_models
                .iter()
                .map(|m| map_entry(m, true)),
        );
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
            providers,
            prov_sel: 0,
            prov_delete_armed: false,
            prov_modal: None,
            models,
            model_sel: 0,
            model_delete_armed: false,
            model_modal: None,
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

    /// Move focus to the detail pane (only if the current category has fields,
    /// or if the category is one of the special interactive screens — API
    /// Providers / Models Select — which carry no [`SettingField`] rows).
    pub fn focus_detail(&mut self) {
        if !super::SETTING_CATEGORIES[self.cat].fields.is_empty()
            || self.is_providers_category()
            || self.is_models_category()
        {
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

    // --- API Providers screen helpers ---

    /// `true` when the selected category is "API Providers".
    pub fn is_providers_category(&self) -> bool {
        super::SETTING_CATEGORIES[self.cat].name == "API Providers"
    }

    /// Move selection up in the providers list; clears the delete-armed flag.
    pub fn prov_up(&mut self) {
        self.prov_sel = self.prov_sel.saturating_sub(1);
        self.prov_delete_armed = false;
    }

    /// Move selection down in the providers list (max index = providers.len()
    /// which is the add-button row); clears the delete-armed flag.
    pub fn prov_down(&mut self) {
        self.prov_sel = (self.prov_sel + 1).min(self.providers.len());
        self.prov_delete_armed = false;
    }

    /// `true` when the `[+ add]` button row is highlighted.
    pub fn prov_on_add_button(&self) -> bool {
        self.prov_sel == self.providers.len()
    }

    /// First Ctrl+X arms the delete; second Ctrl+X confirms it.
    /// Has no effect when the add-button row is selected.
    pub fn prov_arm_or_delete(&mut self) {
        if self.prov_on_add_button() {
            return;
        }
        if self.prov_delete_armed {
            // Confirm: remove the entry.
            if self.prov_sel < self.providers.len() {
                self.providers.remove(self.prov_sel);
            }
            self.prov_delete_armed = false;
            // Clamp selection to the new length.
            let max = self.providers.len(); // add-button index
            if self.prov_sel > max {
                self.prov_sel = max;
            }
        } else {
            self.prov_delete_armed = true;
        }
    }

    /// Cancel the armed-delete state (any key other than Ctrl+X).
    pub fn prov_disarm(&mut self) {
        self.prov_delete_armed = false;
    }

    /// Open the add-provider modal with a blank draft.
    pub fn open_provider_modal(&mut self) {
        self.prov_modal = Some(ProviderModal::new());
    }

    /// Close the add-provider modal without saving.
    pub fn close_provider_modal(&mut self) {
        self.prov_modal = None;
    }

    /// Save the modal draft as a new provider and close the modal. Mints a fresh
    /// uuid for the new entry (the add-provider modal only ever creates).
    pub fn save_provider_modal(&mut self) {
        if let Some(m) = self.prov_modal.take() {
            let name     = m.name.trim().to_string();
            let endpoint = m.endpoint.trim().to_string();
            let api_key  = m.api_key.trim().to_string();
            self.providers.push(ProviderDraft {
                uuid: super::new_uuid(),
                name,
                endpoint,
                api_type: m.api_type,
                api_key,
            });
            // Select the newly added entry (last real index).
            self.prov_sel = self.providers.len().saturating_sub(1);
        }
    }

    /// Move focus up in the modal (clamp 0..=4).
    pub fn modal_up(&mut self) {
        if let Some(m) = self.prov_modal.as_mut() {
            m.field = m.field.saturating_sub(1);
        }
    }

    /// Move focus down in the modal (clamp 0..=4).
    pub fn modal_down(&mut self) {
        if let Some(m) = self.prov_modal.as_mut() {
            m.field = (m.field + 1).min(4);
        }
    }

    /// Move focus left in the modal: moves Cancel→Save on field 4.
    pub fn modal_left(&mut self) {
        if let Some(m) = self.prov_modal.as_mut() {
            if m.field == 4 {
                m.field = 3;
            }
        }
    }

    /// Move focus right in the modal: moves Save→Cancel on field 3.
    pub fn modal_right(&mut self) {
        if let Some(m) = self.prov_modal.as_mut() {
            if m.field == 3 {
                m.field = 4;
            }
        }
    }

    /// Append `c` to the active text field in the modal (field 0=name, 1=endpoint, 2=api_key).
    pub fn modal_push_char(&mut self, c: char) {
        if let Some(m) = self.prov_modal.as_mut() {
            match m.field {
                0 => m.name.push(c),
                1 => m.endpoint.push(c),
                2 => m.api_key.push(c),
                _ => {}
            }
        }
    }

    /// Delete the last character of the active text field in the modal.
    pub fn modal_backspace(&mut self) {
        if let Some(m) = self.prov_modal.as_mut() {
            match m.field {
                0 => { m.name.pop(); }
                1 => { m.endpoint.pop(); }
                2 => { m.api_key.pop(); }
                _ => {}
            }
        }
    }

    // --- Models Select screen helpers ---

    /// `true` when the selected category is "Models Select".
    pub fn is_models_category(&self) -> bool {
        super::SETTING_CATEGORIES[self.cat].name == "Models Select"
    }

    /// Move selection up in the models list; clears the delete-armed flag.
    pub fn model_up(&mut self) {
        self.model_sel = self.model_sel.saturating_sub(1);
        self.model_delete_armed = false;
    }

    /// Move selection down in the models list (max index = models.len() which is
    /// the add-button row); clears the delete-armed flag.
    pub fn model_down(&mut self) {
        self.model_sel = (self.model_sel + 1).min(self.models.len());
        self.model_delete_armed = false;
    }

    /// `true` when the `[+ add model]` button row is highlighted.
    pub fn model_on_add_button(&self) -> bool {
        self.model_sel == self.models.len()
    }

    /// First Ctrl+X arms the delete; second Ctrl+X confirms it. No effect when
    /// the add-button row is selected.
    pub fn model_arm_or_delete(&mut self) {
        if self.model_on_add_button() {
            return;
        }
        if self.model_delete_armed {
            // Confirm: remove the entry.
            if self.model_sel < self.models.len() {
                self.models.remove(self.model_sel);
            }
            self.model_delete_armed = false;
            // Clamp selection to the new length (add-button index).
            let max = self.models.len();
            if self.model_sel > max {
                self.model_sel = max;
            }
        } else {
            self.model_delete_armed = true;
        }
    }

    /// Cancel the armed-delete state (any key other than Ctrl+X).
    pub fn model_disarm(&mut self) {
        self.model_delete_armed = false;
    }

    /// Open the add-model modal with a blank draft (default provider index 0).
    pub fn open_model_modal_add(&mut self) {
        self.model_modal = Some(ModelModal::new_add(0));
    }

    /// Open the edit-model modal, prefilled from `models[idx]`.
    pub fn open_model_modal_edit(&mut self, idx: usize) {
        if let Some(m) = self.models.get(idx) {
            self.model_modal = Some(ModelModal {
                editing_idx: Some(idx),
                uuid: m.uuid.clone(),
                name: m.name.clone(),
                provider_idx: m.provider_idx,
                model_id: m.model_id.clone(),
                field: 0,
                roles: m.roles.clone(),
                role_cursor: 0,
                query: String::new(),
                result_sel: 0,
                // Carry the stored route forward. `route_sel` starts at 0 and is
                // re-synced when endpoints load / the user navigates the list.
                route: m.route.clone(),
                route_sel: 0,
                endpoints: None,
                endpoints_loading: false,
                endpoints_for: None,
            });
        }
    }

    /// Close the add/edit-model modal without saving.
    pub fn close_model_modal(&mut self) {
        self.model_modal = None;
    }

    /// Save the modal draft as a model (replace when editing, else append) and
    /// close the modal; selects the affected row.
    ///
    /// `session_only` is `true` when the caller chose "Save session" (scoped to
    /// this session only) and `false` for plain "Save" (global scope). Persistence
    /// is stubbed — the flag is stored in-memory only.
    ///
    /// Role-steal (per role): a model may hold several roles, but each role is
    /// globally unique. For EACH role the target now holds, that role is removed
    /// from every OTHER model before the target's full role set is written, so no
    /// two models ever share a role.
    pub fn save_model_modal(&mut self, session_only: bool) {
        if let Some(m) = self.model_modal.take() {
            let draft = ModelDraft {
                // Preserve the carried identity (edit) or the freshly minted one
                // (add); mint as a last resort if it somehow arrived empty.
                uuid: if m.uuid.is_empty() {
                    super::new_uuid()
                } else {
                    m.uuid.clone()
                },
                name: m.name.trim().to_string(),
                model_id: m.model_id.trim().to_string(),
                provider_idx: m.provider_idx,
                roles: m.roles.clone(),
                route: m.route.clone(),
                session_only,
            };
            // Determine the target index before inserting/replacing.
            let target_idx = match m.editing_idx {
                Some(i) if i < self.models.len() => i,
                _ => self.models.len(), // will be the pushed index
            };
            // Per-role steal: drop each role the target now holds from every OTHER
            // model so each role stays on at most one model (the target keeps all
            // of its own roles).
            for (i, other) in self.models.iter_mut().enumerate() {
                if i != target_idx {
                    other.roles.retain(|r| !draft.roles.contains(r));
                }
            }
            match m.editing_idx {
                Some(i) if i < self.models.len() => {
                    self.models[i] = draft;
                    self.model_sel = i;
                }
                _ => {
                    self.models.push(draft);
                    self.model_sel = self.models.len().saturating_sub(1);
                }
            }
        }
    }

    /// `true` when the modal's selected provider is an OpenRouter endpoint
    /// (its [`ProviderDraft::endpoint`], lowercased, contains `"openrouter"`).
    /// `false` when no modal is open or the provider index is out of range.
    pub fn mm_provider_is_openrouter(&self) -> bool {
        self.model_modal
            .as_ref()
            .and_then(|m| self.providers.get(m.provider_idx))
            .map(|p| p.endpoint.to_lowercase().contains("openrouter"))
            .unwrap_or(false)
    }

    /// `true` when the modal's selected provider can serve the per-model
    /// provider-endpoints GET: it must be `OpenAiCompatible` (the endpoints
    /// catalogue is an OpenRouter/OpenAI-shaped API — an Anthropic-typed provider
    /// has no equivalent) AND an OpenRouter endpoint (the GET is OpenRouter-only).
    /// The runtime checks this before firing `list_model_endpoints` so a non-
    /// OpenRouter or Anthropic provider never triggers a doomed request — the modal
    /// is resolved to an empty endpoints list instead. `false` when no modal is
    /// open or the provider index is out of range.
    pub fn mm_provider_has_endpoints_api(&self) -> bool {
        self.model_modal
            .as_ref()
            .and_then(|m| self.providers.get(m.provider_idx))
            .map(|p| {
                p.api_type.is_routable() && p.endpoint.to_lowercase().contains("openrouter")
            })
            .unwrap_or(false)
    }

    /// The fields the model modal exposes right now, in navigation order.
    ///
    /// Always `Name, Provider, Model, …, Save, Cancel`. A `Route` field is
    /// inserted when the provider is OpenRouter AND a model is selected (so the
    /// user can pin an upstream provider or leave it on Auto); a `Role` field is
    /// inserted in EDIT mode. The modal's `field` index addresses into this vec.
    pub fn model_modal_fields(&self) -> Vec<ModelField> {
        let mut v = vec![ModelField::Name, ModelField::Provider, ModelField::Model];
        if let Some(m) = &self.model_modal {
            if self.mm_provider_is_openrouter() && !m.model_id.is_empty() {
                v.push(ModelField::Route);
            }
            if m.is_edit() {
                v.push(ModelField::Role);
            }
        }
        v.push(ModelField::Save);
        v.push(ModelField::SaveSession);
        v.push(ModelField::Cancel);
        v
    }

    /// The [`ModelField`] currently focused in the model modal, or `None` when
    /// no modal is open (or `field` somehow points past the computed list).
    pub fn mm_current_field(&self) -> Option<ModelField> {
        let m = self.model_modal.as_ref()?;
        self.model_modal_fields().get(m.field).copied()
    }

    /// The number of Route options (Auto + one per fetched endpoint). `1` when
    /// no endpoints are loaded (just the Auto entry).
    pub fn mm_route_option_count(&self) -> usize {
        1 + self
            .model_modal
            .as_ref()
            .and_then(|m| m.endpoints.as_ref())
            .map(|e| e.len())
            .unwrap_or(0)
    }

    /// Move focus up one field (clamps at 0).
    pub fn mm_up(&mut self) {
        if let Some(m) = self.model_modal.as_mut() {
            m.field = m.field.saturating_sub(1);
        }
    }

    /// Move focus down one field (clamps at the last computed field).
    pub fn mm_down(&mut self) {
        let max = self.model_modal_fields().len().saturating_sub(1);
        if let Some(m) = self.model_modal.as_mut() {
            m.field = (m.field + 1).min(max);
        }
    }

    /// Move left in the model modal, dispatching on the focused field:
    /// - Provider → cycle provider backward (wrapping, resets search), then
    ///   re-clamp `field` since the Route field may appear/disappear.
    /// - Save/SaveSession/Cancel → step left within the button group, clamping at Save.
    /// - everything else (Name/Model/Route) → no-op.
    ///
    /// The Role field is NOT handled here: it's a chip multi-select whose ←→ move
    /// the chip cursor ([`Self::mm_role_left`]/[`Self::mm_role_right`]), driven
    /// directly from the input layer.
    pub fn mm_left(&mut self) {
        let n = self.providers.len();
        match self.mm_current_field() {
            Some(ModelField::Provider) => {
                if let Some(m) = self.model_modal.as_mut() {
                    if n > 0 {
                        m.provider_idx = (m.provider_idx + n - 1) % n;
                        m.query.clear();
                        m.result_sel = 0;
                    }
                }
                self.mm_clamp_field();
            }
            // Button group: Save → SaveSession → Cancel; Left steps backward, clamping at Save.
            Some(ModelField::SaveSession) => {
                self.mm_focus_field(ModelField::Save);
            }
            Some(ModelField::Cancel) => {
                self.mm_focus_field(ModelField::SaveSession);
            }
            Some(ModelField::Save) => {
                // Already at the leftmost button — no-op.
            }
            _ => {}
        }
    }

    /// Move right in the model modal, dispatching on the focused field:
    /// - Provider → cycle provider forward (wrapping, resets search), then
    ///   re-clamp `field` since the Route field may appear/disappear.
    /// - Save/SaveSession/Cancel → step right within the button group, clamping at Cancel.
    /// - everything else (Name/Model/Route) → no-op.
    ///
    /// The Role field is NOT handled here — see [`Self::mm_left`].
    pub fn mm_right(&mut self) {
        let n = self.providers.len();
        match self.mm_current_field() {
            Some(ModelField::Provider) => {
                if let Some(m) = self.model_modal.as_mut() {
                    if n > 0 {
                        m.provider_idx = (m.provider_idx + 1) % n;
                        m.query.clear();
                        m.result_sel = 0;
                    }
                }
                self.mm_clamp_field();
            }
            // Button group: Save → SaveSession → Cancel; Right steps forward, clamping at Cancel.
            Some(ModelField::Save) => {
                self.mm_focus_field(ModelField::SaveSession);
            }
            Some(ModelField::SaveSession) => {
                self.mm_focus_field(ModelField::Cancel);
            }
            Some(ModelField::Cancel) => {
                // Already at the rightmost button — no-op.
            }
            _ => {}
        }
    }

    /// Point `field` at `target` if it exists in the current computed field list.
    fn mm_focus_field(&mut self, target: ModelField) {
        if let Some(pos) = self.model_modal_fields().iter().position(|f| *f == target) {
            if let Some(m) = self.model_modal.as_mut() {
                m.field = pos;
            }
        }
    }

    /// Clamp `field` to the current computed field list (used after the Route
    /// field appears/disappears from a provider change).
    fn mm_clamp_field(&mut self) {
        let max = self.model_modal_fields().len().saturating_sub(1);
        if let Some(m) = self.model_modal.as_mut() {
            if m.field > max {
                m.field = max;
            }
        }
    }

    // --- Role chip multi-select (EDIT-mode Role field) ---

    /// Move the role-chip cursor left over `0..ModelRole::ALL.len()` (clamps at
    /// 0). Drives which chip the Role field highlights/toggles.
    pub fn mm_role_left(&mut self) {
        if let Some(m) = self.model_modal.as_mut() {
            m.role_cursor = m.role_cursor.saturating_sub(1);
        }
    }

    /// Move the role-chip cursor right over `0..ModelRole::ALL.len()` (clamps at
    /// the last chip).
    pub fn mm_role_right(&mut self) {
        let max = ModelRole::ALL.len().saturating_sub(1);
        if let Some(m) = self.model_modal.as_mut() {
            m.role_cursor = (m.role_cursor + 1).min(max);
        }
    }

    /// Toggle membership of the focused chip's role (`ModelRole::ALL[role_cursor]`)
    /// in the modal's `roles`: add when absent, remove when present. The global
    /// per-role steal runs later, on save.
    pub fn mm_role_toggle(&mut self) {
        if let Some(m) = self.model_modal.as_mut() {
            let role = ModelRole::ALL[m.role_cursor.min(ModelRole::ALL.len() - 1)];
            if let Some(pos) = m.roles.iter().position(|r| *r == role) {
                m.roles.remove(pos);
            } else {
                m.roles.push(role);
            }
        }
    }

    /// Move the Route option cursor up (clamps at 0). No-op unless the Route
    /// field is focused.
    pub fn mm_route_up(&mut self) {
        if self.mm_current_field() != Some(ModelField::Route) {
            return;
        }
        if let Some(m) = self.model_modal.as_mut() {
            m.route_sel = m.route_sel.saturating_sub(1);
        }
    }

    /// Move the Route option cursor down (clamps at the last option). No-op
    /// unless the Route field is focused.
    pub fn mm_route_down(&mut self) {
        if self.mm_current_field() != Some(ModelField::Route) {
            return;
        }
        let max = self.mm_route_option_count().saturating_sub(1);
        if let Some(m) = self.model_modal.as_mut() {
            m.route_sel = (m.route_sel + 1).min(max);
        }
    }

    /// Commit the highlighted Route option to `route`: option 0 = Auto (`None`);
    /// option `i` pins `endpoints[i-1]`'s provider name (fallback `name`). Stays
    /// on the Route field. No-op unless the Route field is focused.
    pub fn mm_route_commit(&mut self) {
        if self.mm_current_field() != Some(ModelField::Route) {
            return;
        }
        if let Some(m) = self.model_modal.as_mut() {
            if m.route_sel == 0 {
                m.route = None;
            } else if let Some(eps) = m.endpoints.as_ref() {
                if let Some(ep) = eps.get(m.route_sel - 1) {
                    let pick = ep
                        .provider_name
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or_else(|| ep.name.clone().filter(|s| !s.is_empty()));
                    // Only commit when we actually resolved a name; otherwise
                    // leave the existing route untouched (skip).
                    if pick.is_some() {
                        m.route = pick;
                    }
                }
            }
        }
    }

    /// Append `c` to the active model-modal text field: Name → name; Model → the
    /// omnisearch query when the provider is OpenRouter, else the raw model id.
    /// The Route/Role/button fields ignore typed chars.
    pub fn mm_push_char(&mut self, c: char) {
        let or = self.mm_provider_is_openrouter();
        match self.mm_current_field() {
            Some(ModelField::Name) => {
                if let Some(m) = self.model_modal.as_mut() {
                    m.name.push(c);
                }
            }
            Some(ModelField::Model) => {
                if let Some(m) = self.model_modal.as_mut() {
                    if or {
                        m.query.push(c);
                        m.result_sel = 0;
                    } else {
                        m.model_id.push(c);
                    }
                }
            }
            _ => {}
        }
    }

    /// Delete the last char of the active model-modal text field (mirrors
    /// [`Self::mm_push_char`]).
    pub fn mm_backspace(&mut self) {
        let or = self.mm_provider_is_openrouter();
        match self.mm_current_field() {
            Some(ModelField::Name) => {
                if let Some(m) = self.model_modal.as_mut() {
                    m.name.pop();
                }
            }
            Some(ModelField::Model) => {
                if let Some(m) = self.model_modal.as_mut() {
                    if or {
                        m.query.pop();
                        if m.query.is_empty() {
                            m.result_sel = 0;
                        }
                    } else {
                        m.model_id.pop();
                    }
                }
            }
            _ => {}
        }
    }

    /// Commit the chosen `model_id` from the omnisearch: set it on the modal,
    /// clear the query/selection, and arm the provider-endpoints load. The flags
    /// (`endpoints = None`, `endpoints_loading = true`, `endpoints_for = id`) make
    /// the UI show "loading providers…" immediately; the input layer returns
    /// [`Action::FetchModelEndpoints`](crate::controller::input::Action::FetchModelEndpoints)
    /// so the runtime spawns the actual fetch.
    pub fn mm_select_model(&mut self, model_id: String) {
        if let Some(m) = self.model_modal.as_mut() {
            m.model_id = model_id.clone();
            m.query.clear();
            m.result_sel = 0;
            // A different model has different upstream providers — reset the
            // route choice back to Auto so a stale pin can't carry over.
            m.route = None;
            m.route_sel = 0;
            m.endpoints = None;
            m.endpoints_loading = true;
            m.endpoints_for = Some(model_id);
        }
    }

    /// Arm a provider-endpoints load for the OPEN model modal, returning the
    /// model id to fetch (so the input layer can hand it to
    /// [`Action::FetchModelEndpoints`](crate::controller::input::Action::FetchModelEndpoints)).
    ///
    /// Used by the edit-open path: when an existing model is opened for edit and
    /// its provider is OpenRouter with a non-empty `model_id`, this sets the
    /// loading flags (`endpoints = None`, `endpoints_loading = true`,
    /// `endpoints_for = id`) so the UI shows "loading providers…" at once, and
    /// returns `Some(id)`. Returns `None` (and changes nothing) when no modal is
    /// open, the provider isn't OpenRouter, or the model id is empty — those
    /// cases have no endpoints API to call.
    pub fn mm_arm_endpoints_load(&mut self) -> Option<String> {
        if !self.mm_provider_is_openrouter() {
            return None;
        }
        let m = self.model_modal.as_mut()?;
        let id = m.model_id.trim().to_string();
        if id.is_empty() {
            return None;
        }
        m.endpoints = None;
        m.endpoints_loading = true;
        m.endpoints_for = Some(id.clone());
        Some(id)
    }

    /// The current `route_sel` index in the model modal (0 when no modal is
    /// open). Used by the input handler to decide whether Up/Down should move
    /// within the Route list or escape to the adjacent field.
    pub fn mm_route_sel(&self) -> usize {
        self.model_modal.as_ref().map(|m| m.route_sel).unwrap_or(0)
    }

    /// The current omnisearch query (empty string when no modal is open). Lets
    /// the input handler compute the result set against the model cache.
    pub fn mm_query(&self) -> &str {
        self.model_modal.as_ref().map(|m| m.query.as_str()).unwrap_or("")
    }

    /// Move the omnisearch result cursor up (clamps at 0).
    pub fn mm_result_up(&mut self) {
        if let Some(m) = self.model_modal.as_mut() {
            m.result_sel = m.result_sel.saturating_sub(1);
        }
    }

    /// Move the omnisearch result cursor down, clamped to `max` (the last valid
    /// result index = `results.len().saturating_sub(1)`).
    pub fn mm_result_down(&mut self, max: usize) {
        if let Some(m) = self.model_modal.as_mut() {
            m.result_sel = (m.result_sel + 1).min(max);
        }
    }
}
