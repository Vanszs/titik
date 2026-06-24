//! App – UI mode enum and associated state types.
//!
//! The app is always in exactly one of five modes, represented by [`Mode`]:
//!
//! | Variant          | Meaning                                       |
//! |-----------------|-----------------------------------------------|
//! | `KeyInput`       | Credentials form (api key + model)            |
//! | `SessionPicker`  | `--resume` session list with live search      |
//! | `Chat`           | Normal conversation view                      |
//! | `Settings`       | In-app `/settings` dashboard                  |
//! | `Effort`         | `/effort` reasoning-effort picker overlay     |
//!
//! Mode-specific state is stored inline in the variant so the type system
//! ensures the runtime can only access data that is relevant to the active
//! mode.  [`KeyInputForm`], [`PickerState`], [`SettingsState`], and
//! [`EffortPickerState`] live here; `Chat` carries no extra state beyond
//! `AppStateRest`.

use std::path::{Path, PathBuf};

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
    ///
    /// Boxed: `SettingsState` is much larger than the other variants (it carries
    /// every draft plus the path-list/picker working state), so storing it inline
    /// would bloat `Mode` everywhere. The box keeps the enum small.
    Settings(Box<SettingsState>),
    /// Reasoning/thinking-effort picker (`/effort`): a small overlay listing the
    /// effort options the current model supports. The inner [`EffortPickerState`]
    /// holds the option list, the cursor, and a one-line capability note. Boxed
    /// to keep `Mode` small and consistent with `Settings`.
    Effort(Box<EffortPickerState>),
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
    /// Toggle: master switch for the safety harness ("Pass B").
    ClassifierEnabled,
    /// Text: model used for the safety classifier.
    ClassifierModel,
    /// Text: provider slug (strict-pinned) for the safety classifier.
    ClassifierProvider,
    /// Text: extra allowed folders (comma-separated) for the workspace check.
    AllowedFolders,
    /// Toggle: master kill-switch for the short-send token saver.
    ShortSendEnabled,
    /// Toggle: cache-warmth-adaptive summarization. On only for models with a
    /// sliding/refreshing prompt cache (e.g. Anthropic).
    SlidingCache,
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
            SettingField::ClassifierEnabled  => "Harness",
            SettingField::ClassifierModel    => "Class. model",
            SettingField::ClassifierProvider => "Class. provider",
            SettingField::AllowedFolders     => "Allowed dirs",
            SettingField::ShortSendEnabled   => "Short-send",
            SettingField::SlidingCache       => "Sliding cache",
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
        fields: &[SettingField::Name, SettingField::Workdir, SettingField::ShortSendEnabled, SettingField::SlidingCache],
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
    SettingCategory {
        name: "Harness",
        fields: &[
            SettingField::ClassifierEnabled,
            SettingField::ClassifierModel,
            SettingField::ClassifierProvider,
            SettingField::AllowedFolders,
        ],
    },
];

/// What a confirmed [`PathPicker`] selection does to the target path list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    /// Append the chosen path as a new entry.
    Add,
    /// Replace the entry at this index in the list.
    Replace(usize),
}

/// A real-filesystem directory picker overlay (the `@`-style descent UI).
///
/// `query` is the raw text the user types (a leading `@` is allowed and stripped
/// when matching). `matches` is the live list of directories under the resolved
/// parent whose name starts with the typed prefix, rendered in the SAME form the
/// user is typing (absolute → absolute, relative → relative). `sel` indexes
/// `matches`. `mode` decides whether confirming adds or replaces in the list.
#[derive(Debug, Clone)]
pub struct PathPicker {
    /// Raw query text (may begin with `@`; relative or absolute).
    pub query: String,
    /// Directory matches for the current query, capped at the view limit.
    pub matches: Vec<String>,
    /// Cursor within `matches`.
    pub sel: usize,
    /// Add a new entry, or replace an existing one at the given index.
    pub mode: PickerMode,
}

/// Max directory matches surfaced in the picker overlay (mirrors the chat `@`
/// file palette and the view-side window constant).
pub const PICKER_MAX: usize = 10;

impl PathPicker {
    /// Open a picker in the given `mode`, seeded with `query` (used to prefill a
    /// REPLACE with the current entry). Computes the first match set against `cwd`.
    pub fn new(mode: PickerMode, query: String, cwd: &Path) -> Self {
        let matches = list_dirs(&query, cwd, PICKER_MAX);
        Self {
            query,
            matches,
            sel: 0,
            mode,
        }
    }

    /// Recompute `matches` for the current `query` and clamp `sel` into range.
    pub fn recompute(&mut self, cwd: &Path) {
        self.matches = list_dirs(&self.query, cwd, PICKER_MAX);
        if self.sel >= self.matches.len() {
            self.sel = self.matches.len().saturating_sub(1);
        }
    }

    /// Move the selection up one row (clamps at 0).
    pub fn up(&mut self) {
        self.sel = self.sel.saturating_sub(1);
    }

    /// Move the selection down one row (clamps at the last match).
    pub fn down(&mut self) {
        if self.sel + 1 < self.matches.len() {
            self.sel += 1;
        }
    }

    /// The currently highlighted match, if any.
    pub fn selected(&self) -> Option<&String> {
        self.matches.get(self.sel)
    }
}

/// List directories for an `@`-style `query`, rendered in the same form the user
/// is typing them, capped at `limit`.
///
/// Resolution:
/// - A leading `@` is stripped.
/// - If the (stripped) query begins with `/` it is ABSOLUTE; otherwise it is
///   resolved relative to `cwd`.
/// - The query is split into `(parent, prefix)` at the last `/`. A query ending
///   in `/` means "list everything in this directory" (prefix = "").
/// - `parent` is read with `std::fs::read_dir`; only sub-DIRECTORIES whose file
///   name starts with `prefix` (case-insensitive) are kept. Hidden dirs (leading
///   `.`) are skipped UNLESS the prefix itself starts with `.`.
/// - Each kept directory is rendered back in the user's form (absolute → an
///   absolute path string, relative → a relative string) WITHOUT a trailing
///   slash, sorted, then capped at `limit`.
///
/// Any IO error (unreadable parent, etc.) yields an empty vec — the picker just
/// shows nothing rather than failing.
pub fn list_dirs(query: &str, cwd: &Path, limit: usize) -> Vec<String> {
    // Strip an optional leading '@'; the rest is the path the user is typing.
    let raw = query.strip_prefix('@').unwrap_or(query);
    let is_abs = raw.starts_with('/');

    // Split into the directory part and the in-progress final segment (prefix).
    // A trailing '/' means the whole thing is the parent and the prefix is empty.
    let (dir_part, prefix) = match raw.rfind('/') {
        Some(i) => (&raw[..=i], &raw[i + 1..]), // keep the slash on dir_part
        None => ("", raw),                       // no slash: parent is cwd-relative root
    };

    // Resolve the parent directory on the real filesystem.
    let parent: PathBuf = if is_abs {
        // dir_part always starts with '/' here (raw starts with '/').
        PathBuf::from(dir_part)
    } else if dir_part.is_empty() {
        cwd.to_path_buf()
    } else {
        cwd.join(dir_part)
    };

    let entries = match std::fs::read_dir(&parent) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let prefix_lower = prefix.to_lowercase();
    // Honour hidden dirs only when the user is explicitly typing a dotted prefix.
    let want_hidden = prefix.starts_with('.');

    let mut out: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        // Directories only (follows symlinks via file_type()? no — use metadata
        // through is_dir on the path so symlinked dirs are included, matching the
        // lenient spirit of the workspace check).
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue, // non-UTF-8 directory name: skip
        };
        if name.starts_with('.') && !want_hidden {
            continue;
        }
        if !name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        // Render back in the user's typing form: dir_part (which carries the
        // trailing slash, or is empty) + the matched name, no trailing slash.
        out.push(format!("{dir_part}{name}"));
    }

    out.sort();
    out.truncate(limit);
    out
}

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
        // The effective workdir doubles as the picker's relative-path base.
        let effective_cwd = session.workdir();
        // Workdir list: prefer the stored entries (trimmed, non-empty); if none,
        // show the single effective dir so the field is never blank.
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
        // Allowed dirs list: stored entries, or the launch cwd when empty so the
        // always-allowed launch directory is visible (preserves prior behaviour).
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
            SettingField::AwarenessModel if !self.awareness_inherit => {
                Some(&mut self.awareness_model)
            }
            SettingField::AwarenessProvider if !self.awareness_inherit => {
                Some(&mut self.awareness_provider)
            }
            SettingField::ClassifierModel    => Some(&mut self.classifier_model),
            SettingField::ClassifierProvider => Some(&mut self.classifier_provider),
            // Workdir / AllowedFolders are PATH LISTS (managed via the picker),
            // not plain text fields, so they have no scalar text draft.
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

    /// Return focus to the sidebar; also exits editing/list/picker modes so the
    /// detail pane is back at plain field navigation next time it's focused.
    pub fn focus_sidebar(&mut self) {
        self.in_detail = false;
        self.editing = false;
        self.list_editing = false;
        self.list_sel = 0;
        self.picker = None;
    }

    /// Act on Enter while in the detail pane.
    ///
    /// - Theme → toggle dark/light (no edit mode).
    /// - Accent → no-op (arrows cycle it instead).
    /// - AwarenessEnabled → toggle on/off.
    /// - AwarenessSource → toggle inherit/custom.
    /// - AwarenessModel / AwarenessProvider → edit only when source is "custom";
    ///   a no-op while inheriting (the values are irrelevant then).
    /// - Workdir / AllowedFolders (PATH LISTS) → enter per-entry list management.
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
                // Path lists: drop into per-entry management, top row selected.
                self.list_editing = true;
                self.list_sel = 0;
            }
            _ => {
                self.editing = true;
            }
        }
    }

    // --- Path-list management (one nesting level below field navigation) -------

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

    /// Remove the highlighted entry, honouring the min-1 rule: the last entry
    /// can never be removed. Clamps `list_sel` afterwards.
    pub fn list_remove(&mut self) {
        let f = self.current_field();
        let sel = self.list_sel;
        if let Some(v) = self.path_list_mut(f) {
            if v.len() > 1 && sel < v.len() {
                v.remove(sel);
            }
        }
        // Re-clamp the cursor against the (possibly shortened) list.
        let len = self.path_list(f).map(|v| v.len()).unwrap_or(0);
        if self.list_sel >= len {
            self.list_sel = len.saturating_sub(1);
        }
    }

    /// Open the FS picker in ADD mode (a fresh, empty query).
    pub fn open_picker_add(&mut self) {
        self.picker = Some(PathPicker::new(PickerMode::Add, String::new(), &self.cwd));
    }

    /// Open the FS picker in REPLACE mode for the highlighted entry, seeding the
    /// query with that entry's current value for easy editing.
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

    /// Confirm the active picker: apply the chosen path (the selected match, else
    /// the raw query stripped of a leading `@`), trimmed, to the target list,
    /// then close the picker. A blank choice is ignored (picker still closes).
    pub fn picker_confirm(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        // Selected match wins; otherwise fall back to the raw typed query.
        let chosen = picker
            .selected()
            .cloned()
            .unwrap_or_else(|| picker.query.clone());
        let chosen = chosen.strip_prefix('@').unwrap_or(&chosen).trim().to_string();
        if chosen.is_empty() {
            return; // nothing to apply; picker already closed
        }
        let f = self.current_field();
        match picker.mode {
            PickerMode::Add => {
                if let Some(v) = self.path_list_mut(f) {
                    v.push(chosen);
                    // Keep the cursor on the freshly added entry.
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

    /// Drill into the currently highlighted match: set the query to that match
    /// plus a trailing `/` and recompute, descending one level. No-op when no
    /// match is highlighted.
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

/// State for the `/effort` reasoning-effort picker overlay.
///
/// `options` is the capability-derived list shown to the user (e.g.
/// `["default","off","low","high"]` for an effort model, or `["off","on"]` for
/// an on/off-only one). `selected` indexes `options`; `note` is a short
/// capability line shown dimmed in the footer (e.g. why a model has no control,
/// or that capabilities couldn't be fetched). Built in the `/effort` command
/// handler; keystrokes are handled by [`controller::input::handle_effort`].
pub struct EffortPickerState {
    /// The effort options offered for the current model, in display order.
    pub options: Vec<String>,
    /// Cursor within `options`.
    pub selected: usize,
    /// One-line capability note rendered dim in the footer.
    pub note: String,
}

impl EffortPickerState {
    /// Move the cursor up one row (clamps at 0).
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the cursor down one row (clamps at the last option).
    pub fn down(&mut self) {
        if self.selected + 1 < self.options.len() {
            self.selected += 1;
        }
    }

    /// The currently highlighted option, if any.
    pub fn selected_option(&self) -> Option<&String> {
        self.options.get(self.selected)
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
