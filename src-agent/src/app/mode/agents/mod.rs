//! Agents-mode types: the sub-mode state machine and the [`AgentsState`] draft
//! holder for the in-app `/agents` management dashboard.
//!
//! The dashboard is modelled on `/settings`: a LIST + DETAIL two-pane layout
//! with a small state machine layered on top. The data layer
//! ([`crate::model::agent_def`]) owns load/save/delete; this module only holds
//! the working drafts and navigation state, applying them via the data-layer
//! API on confirm (see `app::runtime::actions`).
//!
//! Sub-mode state machine ([`AgentSubMode`]):
//!
//! ```text
//!   Browse ── →/Enter (file-backed) ──▶ Edit ── s ──▶ save ──▶ Browse
//!     │                                   │
//!     │── n ──▶ Create ── s ──▶ create ──▶ Browse
//!     │
//!     └── d (file-backed) ──▶ DeleteConfirm ── y ──▶ delete ──▶ Browse
//! ```
//!
//! Built-in agents (`AgentSource::Builtin`, `file_path == None`) are read-only:
//! the input handler refuses Edit/Delete on them; they are only overridable by
//! creating a same-named session/global file.

use std::path::PathBuf;

use crate::model::agent_def::{load_registry, AgentDef, AgentSource};
use crate::model::app_config::{AppConfig, ModelEntry};
use crate::model::session::Session;
use crate::model::settings::Settings;
use crate::tool::all_tools;

/// Tool names excluded from the picker (internal / infra tools).
const EXCLUDED_TOOLS: &[&str] = &["task", "pong", "dir_cache_update"];

/// State for the tool multi-select picker overlay.
///
/// Opened from the Edit/Create form when the user presses Enter on the Tools
/// field. Closed by Enter (confirm) or Esc (cancel). All mutations happen
/// through the `AgentsState` helper methods so the cursor always stays within
/// filtered bounds.
#[derive(Debug, Clone)]
pub struct ToolPickerState {
    /// Full selectable tool name list (filtered copy of `all_tools()`, minus
    /// the excluded internal tools).
    pub options: Vec<String>,
    /// Parallel to `options`; `true` = this tool is currently checked.
    pub checked: Vec<bool>,
    /// Index into the FILTERED view (see `filtered_indices`).
    pub cursor: usize,
    /// Live search string; filters `options` by substring match.
    pub filter: String,
}

impl ToolPickerState {
    /// Build from the current `draft_tools` comma-joined string.
    ///
    /// All tools from `all_tools()` except `EXCLUDED_TOOLS` are listed.
    /// An option is pre-checked if its name appears in `draft_tools` (case-
    /// insensitive, split on comma, trimmed).
    fn from_draft(draft_tools: &str) -> Self {
        let options: Vec<String> = all_tools()
            .iter()
            .map(|t| t.name().to_string())
            .filter(|n| !EXCLUDED_TOOLS.contains(&n.as_str()))
            .collect();

        let selected: Vec<String> = draft_tools
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let checked: Vec<bool> = options
            .iter()
            .map(|n| selected.contains(&n.to_lowercase()))
            .collect();

        Self {
            options,
            checked,
            cursor: 0,
            filter: String::new(),
        }
    }

    /// Indices into `options` that match the current `filter`.
    ///
    /// If `filter` is empty, all indices are returned in order.
    pub fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            (0..self.options.len()).collect()
        } else {
            let q = self.filter.to_lowercase();
            self.options
                .iter()
                .enumerate()
                .filter(|(_, n)| n.to_lowercase().contains(&q))
                .map(|(i, _)| i)
                .collect()
        }
    }

    /// Move the cursor up within the filtered list.
    pub fn up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor down within the filtered list.
    pub fn down(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            return;
        }
        if self.cursor + 1 < len {
            self.cursor += 1;
        }
    }

    /// Toggle the checked state for the option at the current filtered cursor.
    ///
    /// No-op when the filtered list is empty.
    pub fn toggle(&mut self) {
        let indices = self.filtered_indices();
        if indices.is_empty() {
            return;
        }
        let real = indices[self.cursor.min(indices.len() - 1)];
        self.checked[real] = !self.checked[real];
    }

    /// Append a character to the filter and clamp the cursor.
    pub fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.clamp_cursor();
    }

    /// Remove the last character from the filter and clamp the cursor.
    pub fn backspace_filter(&mut self) {
        self.filter.pop();
        self.clamp_cursor();
    }

    /// Clamp `cursor` so it stays within the current filtered bounds.
    fn clamp_cursor(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    /// The checked tool names, in `options` order.
    pub fn selected(&self) -> Vec<String> {
        self.options
            .iter()
            .zip(self.checked.iter())
            .filter(|(_, &c)| c)
            .map(|(n, _)| n.clone())
            .collect()
    }
}

/// Resolve a [`ModelEntry`]'s provider connection to a human-readable name for
/// the model picker / browse rows: the provider's `name` (falling back to its
/// `endpoint`) looked up in `config.providers` by the entry's `provider_uuid`,
/// or `"?"` when the connection is missing/blank.
fn entry_provider_name(config: &AppConfig, entry: &ModelEntry) -> String {
    match config.providers.iter().find(|p| p.uuid == entry.provider_uuid) {
        Some(p) if !p.name.trim().is_empty() => p.name.clone(),
        Some(p) if !p.endpoint.trim().is_empty() => p.endpoint.clone(),
        _ => "?".to_string(),
    }
}

/// One-line label for a registered model in the picker / browse row:
/// `"name — model_id @ <provider name>"`.
fn entry_label(config: &AppConfig, entry: &ModelEntry) -> String {
    format!(
        "{} — {} @ {}",
        entry.name,
        entry.model_id,
        entry_provider_name(config, entry)
    )
}

/// State for the single-select MODEL picker overlay.
///
/// Opened from the Edit/Create form when the user presses Enter on the Model
/// field. It is a pick-ONE list over the REGISTERED models (the same entries
/// edited in `/settings` → Models): row 0 is the `None` "(inherit main)"
/// sentinel, then every [`ModelEntry`] from `settings.session_models` followed
/// by the global `config.models`. The cursor row is the chosen value. Closed by
/// Enter (confirm → write the cursor's uuid into `draft_model_uuid`) or Esc
/// (cancel → discard). Mirrors the tool picker's modal flow.
#[derive(Debug, Clone)]
pub struct ModelPickerState {
    /// One row per choice: `(model_uuid_or_none, display_label)`. Row 0 is always
    /// the `None` "(inherit main)" sentinel; the rest are registered model entries
    /// (session overrides first, then the global catalogue) in order.
    pub options: Vec<(Option<String>, String)>,
    /// Highlighted row, in `0..options.len()`.
    pub cursor: usize,
}

impl ModelPickerState {
    /// Build the option list from the registered models, placing the cursor on the
    /// row matching `current` (the agent's current `model_uuid`).
    ///
    /// Row 0 is the `None` "(inherit main)" sentinel; the remaining rows are the
    /// session model overrides (`settings.session_models`) followed by the global
    /// catalogue (`config.models`), each labelled `"name — model_id @ provider"`.
    /// The cursor lands on the row whose uuid equals `current` (or row 0 when
    /// `current` is `None` or no longer matches a registered entry).
    pub fn from_models(config: &AppConfig, settings: &Settings, current: &Option<String>) -> Self {
        let mut options: Vec<(Option<String>, String)> =
            vec![(None, "(inherit main)".to_string())];
        for entry in settings
            .session_models
            .iter()
            .chain(config.models.iter())
        {
            options.push((Some(entry.uuid.clone()), entry_label(config, entry)));
        }
        let cursor = match current {
            Some(uuid) => options
                .iter()
                .position(|(u, _)| u.as_deref() == Some(uuid.as_str()))
                .unwrap_or(0),
            None => 0,
        };
        Self { options, cursor }
    }

    /// Move the cursor up (clamps at 0).
    pub fn up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor down (clamps at the last option).
    pub fn down(&mut self) {
        if self.cursor + 1 < self.options.len() {
            self.cursor += 1;
        }
    }

    /// The model uuid at the cursor: `None` for the "(inherit main)" row, else the
    /// chosen registered model's uuid.
    pub fn selected(&self) -> Option<String> {
        self.options
            .get(self.cursor)
            .and_then(|(u, _)| u.clone())
    }
}

/// Which scope a freshly-created agent is written to.
///
/// Mirrors [`crate::model::agent_def::AgentScope`] but owns no borrow, so it can
/// live inside the long-lived [`AgentsState`]. Converted to the borrowed scope
/// at save time (it needs the session dir).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScope {
    /// `<session_dir>/agents/`.
    Session,
    /// `~/.simple-coder/agents/`.
    Global,
}

impl AgentScope {
    /// Short label used in the create-scope picker and source tags.
    pub fn label(self) -> &'static str {
        match self {
            AgentScope::Session => "session",
            AgentScope::Global => "global",
        }
    }
}

/// The active sub-mode of the `/agents` dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSubMode {
    /// Navigating the agent list / reading the selected agent (read-only).
    Browse,
    /// Editing an existing file-backed agent's fields + body.
    Edit,
    /// Creating a new agent (scope + name first, then the same field editor).
    Create,
    /// Confirming deletion of the selected file-backed agent (`y`/`n`).
    DeleteConfirm,
}

/// One editable field in the Edit/Create detail editor, in display/nav order.
///
/// `Name` is only navigable in Create (an existing agent's name is its
/// filename and is not renamed in place — renaming = delete + create).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEditField {
    /// Create-only: the filename slug (sanitised by the data layer on save).
    Name,
    /// Required user-facing description (frontmatter `description`).
    Description,
    /// Registered model the agent runs on; `(inherit main)` when unset. Opens a
    /// single-select picker over the models registered in `/settings`.
    Model,
    /// Comma/space separated tool allow-list (frontmatter `tools`).
    Tools,
    /// The markdown body = the agent system prompt.
    Body,
}

impl AgentEditField {
    /// Left-column label for the detail editor.
    pub fn label(self) -> &'static str {
        match self {
            AgentEditField::Name => "name",
            AgentEditField::Description => "description",
            AgentEditField::Model => "model",
            AgentEditField::Tools => "tools",
            AgentEditField::Body => "prompt",
        }
    }
}

/// Field order while EDITING an existing agent (no name row).
const EDIT_FIELDS: &[AgentEditField] = &[
    AgentEditField::Description,
    AgentEditField::Model,
    AgentEditField::Tools,
    AgentEditField::Body,
];

/// Field order while CREATING a new agent (name row first).
const CREATE_FIELDS: &[AgentEditField] = &[
    AgentEditField::Name,
    AgentEditField::Description,
    AgentEditField::Model,
    AgentEditField::Tools,
    AgentEditField::Body,
];

/// Seed body for a freshly-created agent (placeholder system prompt).
const TEMPLATE_BODY: &str =
    "You are a focused subagent. Do the task you are given, then report your\nfindings concisely.";

/// Working state for the in-app `/agents` dashboard.
///
/// Holds the loaded registry snapshot plus editable drafts. Nothing touches
/// disk until the user confirms a create/edit/delete, at which point the
/// runtime reads these drafts back out and calls the data-layer API.
#[derive(Debug, Clone)]
pub struct AgentsState {
    /// Snapshot of the registry (sorted by name), reloaded after every change.
    pub agents: Vec<AgentDef>,
    /// Selected index into `agents` (the LIST cursor).
    pub list_sel: usize,
    /// `false` = focus on the LIST pane; `true` = focus on the DETAIL pane.
    pub in_detail: bool,
    /// Active sub-mode (Browse / Edit / Create / DeleteConfirm).
    pub mode: AgentSubMode,
    /// Highlighted field within the Edit/Create editor.
    pub field: AgentEditField,
    /// `true` while typing into the highlighted text field; `false` = navigating.
    pub editing: bool,
    /// Chosen scope for a Create (toggled before naming).
    pub create_scope: AgentScope,
    /// Draft: filename slug (Create only).
    pub draft_name: String,
    /// Draft: description.
    pub draft_description: String,
    /// Draft: uuid of the REGISTERED model this agent runs on (`None` = inherit
    /// the Main role). Selected via the single-select model picker; on save it
    /// becomes [`AgentDef::model_uuid`].
    pub draft_model_uuid: Option<String>,
    /// Read-only legacy hint: an old agent file's free-text `model` slug, kept
    /// only so the Model row can SHOW it when the file predates `model_uuid`. It is
    /// never written back (the editor only ever writes `model_uuid`).
    pub draft_model_legacy: Option<String>,
    /// Draft: tool allow-list, raw comma/space separated text.
    pub draft_tools: String,
    /// Draft: markdown body / system prompt.
    pub draft_body: String,
    /// The session's directory (for the Session scope target).
    pub session_dir: PathBuf,
    /// When `Some`, the tool multi-select picker overlay is active.
    ///
    /// All key input is routed to the picker; the form underneath is frozen
    /// until the picker is confirmed (Enter) or cancelled (Esc).
    pub tool_picker: Option<ToolPickerState>,
    /// When `Some`, the single-select MODEL picker overlay is active (opened from
    /// the Model field). Like `tool_picker` it owns all key input while open; the
    /// deepest modal in the agents editor.
    pub model_picker: Option<ModelPickerState>,
    /// When `Some`, the FULL-SCREEN nano-style prompt editor is active (opened by
    /// activating the Prompt/Body field). It replaces the whole agents view and
    /// owns all key input; on Esc it commits its text back into `draft_body` and
    /// closes (`= None`). A sub-state of `Mode::Agents`, not a separate mode.
    pub prompt_editor: Option<crate::app::mode::editor::TextEditorState>,
}

impl AgentsState {
    /// Build the dashboard from the active session: load the registry and start
    /// in Browse with the LIST focused.
    pub fn from(session: &Session) -> Self {
        let agents = load_agents_snapshot(session);
        Self {
            agents,
            list_sel: 0,
            in_detail: false,
            mode: AgentSubMode::Browse,
            field: AgentEditField::Description,
            editing: false,
            create_scope: AgentScope::Session,
            draft_name: String::new(),
            draft_description: String::new(),
            draft_model_uuid: None,
            draft_model_legacy: None,
            draft_tools: String::new(),
            draft_body: String::new(),
            session_dir: session.path.clone(),
            tool_picker: None,
            model_picker: None,
            prompt_editor: None,
        }
    }

    /// Reload the registry snapshot from disk and clamp the cursor.
    ///
    /// Called after every create/edit/delete so the LIST reflects disk state.
    pub fn reload(&mut self, session: &Session) {
        self.agents = load_agents_snapshot(session);
        if self.list_sel >= self.agents.len() {
            self.list_sel = self.agents.len().saturating_sub(1);
        }
    }

    /// The currently-selected agent, if any.
    pub fn current_agent(&self) -> Option<&AgentDef> {
        self.agents.get(self.list_sel)
    }

    /// The fields visible in the current editor (Create has the name row).
    pub fn fields(&self) -> &'static [AgentEditField] {
        if self.mode == AgentSubMode::Create {
            CREATE_FIELDS
        } else {
            EDIT_FIELDS
        }
    }

    // --- LIST navigation (Browse) ---

    /// Move the LIST cursor up.
    pub fn list_up(&mut self) {
        self.list_sel = self.list_sel.saturating_sub(1);
    }

    /// Move the LIST cursor down.
    pub fn list_down(&mut self) {
        if self.list_sel + 1 < self.agents.len() {
            self.list_sel += 1;
        }
    }

    // --- Editor field navigation (Edit / Create) ---

    /// Move the editor cursor to the previous field.
    pub fn field_up(&mut self) {
        let fields = self.fields();
        let cur = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        let next = cur.saturating_sub(1);
        self.field = fields[next];
    }

    /// Move the editor cursor to the next field.
    pub fn field_down(&mut self) {
        let fields = self.fields();
        let cur = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        if cur + 1 < fields.len() {
            self.field = fields[cur + 1];
        }
    }

    /// Mutable handle to the draft buffer for the highlighted TEXT field.
    ///
    /// The Model field is a picker (never typed into), so it has no text buffer:
    /// the input handler opens the model picker on Enter instead of setting
    /// `editing`, so `draft_mut` is never called while it is highlighted.
    fn draft_mut(&mut self) -> &mut String {
        match self.field {
            AgentEditField::Name => &mut self.draft_name,
            AgentEditField::Description => &mut self.draft_description,
            AgentEditField::Tools => &mut self.draft_tools,
            AgentEditField::Body => &mut self.draft_body,
            // Model is a picker, not a text field; it never enters text-edit mode.
            AgentEditField::Model => unreachable!("model field is edited via the picker"),
        }
    }

    /// Immutable handle to the draft buffer for text field `f` (view-side reads).
    ///
    /// The Model field is a picker with no text buffer; the view renders it from
    /// `draft_model_uuid` directly, so it is not requested here.
    pub fn draft(&self, f: AgentEditField) -> &str {
        match f {
            AgentEditField::Name => &self.draft_name,
            AgentEditField::Description => &self.draft_description,
            AgentEditField::Tools => &self.draft_tools,
            AgentEditField::Body => &self.draft_body,
            // Model is a picker, not a text field; the view reads its uuid instead.
            AgentEditField::Model => "",
        }
    }

    /// Append `c` to the active draft. The Name field is restricted to
    /// slug-safe characters (alnum + dash) so the on-screen draft can never hold
    /// a value the data-layer sanitiser would later reject.
    pub fn push_char(&mut self, c: char) {
        if self.field == AgentEditField::Name {
            if c.is_ascii_alphanumeric() || c == '-' {
                self.draft_name.push(c.to_ascii_lowercase());
            }
            return;
        }
        self.draft_mut().push(c);
    }

    /// Delete the last char of the active draft. Body deletes a full char
    /// (including any trailing newline) like every other field.
    pub fn backspace(&mut self) {
        self.draft_mut().pop();
    }

    /// Insert a newline into the body draft (multiline prompt editing).
    pub fn newline(&mut self) {
        if self.field == AgentEditField::Body {
            self.draft_body.push('\n');
        }
    }

    // --- Sub-mode transitions ---

    /// Enter EDIT for the selected agent, seeding drafts from it. The caller has
    /// already verified the agent is file-backed (not a built-in).
    pub fn enter_edit(&mut self) {
        let Some(a) = self.current_agent().cloned() else {
            return;
        };
        self.draft_name = a.name.clone();
        self.draft_description = a.description.clone();
        self.draft_model_uuid = a.model_uuid.clone();
        // Legacy hint only when the file predates `model_uuid` (no registered
        // model chosen, but an old free-text `model` slug is present).
        self.draft_model_legacy = if a.model_uuid.is_none() {
            a.model.clone().filter(|m| !m.trim().is_empty())
        } else {
            None
        };
        self.draft_tools = a.tools.join(", ");
        self.draft_body = a.prompt.clone();
        self.mode = AgentSubMode::Edit;
        self.field = AgentEditField::Description;
        self.in_detail = true;
        self.editing = false;
    }

    /// Enter CREATE: reset every draft, seed the body template, focus the name.
    pub fn enter_create(&mut self) {
        self.draft_name = String::new();
        self.draft_description = String::new();
        self.draft_model_uuid = None;
        self.draft_model_legacy = None;
        self.draft_tools = String::new();
        self.draft_body = TEMPLATE_BODY.to_string();
        self.create_scope = AgentScope::Session;
        self.mode = AgentSubMode::Create;
        self.field = AgentEditField::Name;
        self.in_detail = true;
        self.editing = false;
    }

    /// Enter DELETE-CONFIRM for the selected agent. The caller has already
    /// verified the agent is file-backed (built-ins can never reach here).
    pub fn enter_delete(&mut self) {
        self.mode = AgentSubMode::DeleteConfirm;
        self.editing = false;
    }

    /// Toggle the create scope (Session <-> Global) — Create mode only.
    pub fn toggle_scope(&mut self) {
        self.create_scope = match self.create_scope {
            AgentScope::Session => AgentScope::Global,
            AgentScope::Global => AgentScope::Session,
        };
    }

    /// Discard drafts and return to Browse with the LIST focused.
    pub fn cancel(&mut self) {
        self.mode = AgentSubMode::Browse;
        self.editing = false;
        self.in_detail = false;
        self.field = AgentEditField::Description;
        self.tool_picker = None;
        self.model_picker = None;
        self.prompt_editor = None;
    }

    // --- Tool picker overlay ---

    /// Open the tool multi-select picker, seeding checked state from the
    /// current `draft_tools`.
    pub fn open_tool_picker(&mut self) {
        self.tool_picker = Some(ToolPickerState::from_draft(&self.draft_tools));
    }

    /// Confirm the picker: write the selected tools back into `draft_tools`
    /// (comma-joined, options order) and close the overlay.
    pub fn confirm_tool_picker(&mut self) {
        if let Some(p) = self.tool_picker.take() {
            self.draft_tools = p.selected().join(", ");
        }
    }

    /// Cancel the picker without modifying `draft_tools`.
    pub fn cancel_tool_picker(&mut self) {
        self.tool_picker = None;
    }

    // --- Model picker overlay ---

    /// Open the single-select model picker over the registered models, seeding the
    /// cursor from the current `draft_model_uuid`.
    pub fn open_model_picker(&mut self, config: &AppConfig, settings: &Settings) {
        self.model_picker = Some(ModelPickerState::from_models(
            config,
            settings,
            &self.draft_model_uuid,
        ));
    }

    /// Confirm the picker: write the cursor's choice into `draft_model_uuid` and
    /// close the overlay. Choosing a registered model clears the legacy hint (the
    /// agent now resolves through `model_uuid`, so the old slug is no longer shown).
    pub fn confirm_model_picker(&mut self) {
        if let Some(p) = self.model_picker.take() {
            self.draft_model_uuid = p.selected();
            self.draft_model_legacy = None;
        }
    }

    /// Cancel the picker without modifying `draft_model_uuid`.
    pub fn cancel_model_picker(&mut self) {
        self.model_picker = None;
    }

    // --- Full-screen prompt editor ---

    /// Open the full-screen prompt editor, seeded from the current `draft_body`.
    ///
    /// Called instead of starting inline editing when the user activates the
    /// Prompt/Body field (Enter on its row). While open it owns all key input and
    /// replaces the whole agents view.
    pub fn open_prompt_editor(&mut self) {
        self.prompt_editor =
            Some(crate::app::mode::editor::TextEditorState::from_text(&self.draft_body));
    }

    /// Commit the prompt editor: write its text back into `draft_body` and close
    /// the editor (returning to the field list). No-op when it isn't open.
    pub fn commit_prompt_editor(&mut self) {
        if let Some(ed) = self.prompt_editor.take() {
            self.draft_body = ed.text();
        }
    }

    /// Build an [`AgentDef`] from the current drafts (the value the runtime
    /// hands to the data layer on create/save).
    ///
    /// `tools` is parsed from the raw draft by splitting on commas/whitespace and
    /// dropping empties — only fields we know how to round-trip are written, so
    /// the on-disk file stays clean. The name is the create draft for Create, or
    /// the selected agent's name for Edit.
    pub fn to_agent_def(&self) -> AgentDef {
        let name = if self.mode == AgentSubMode::Create {
            self.draft_name.trim().to_string()
        } else {
            self.current_agent()
                .map(|a| a.name.clone())
                .unwrap_or_default()
        };
        let tools: Vec<String> = self
            .draft_tools
            .split([',', ' ', '\t', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        AgentDef {
            name,
            description: self.draft_description.trim().to_string(),
            // The chosen registered model (None = inherit Main). The editor no
            // longer writes the legacy `model` / `provider` / `provider_uuid`
            // fields — they stay at their `None` default for new/edited agents.
            model_uuid: self.draft_model_uuid.clone(),
            tools,
            prompt: self.draft_body.clone(),
            ..AgentDef::default()
        }
    }
}

/// Source label shown in the LIST and DETAIL panes.
pub fn source_label(source: AgentSource) -> &'static str {
    match source {
        AgentSource::Builtin => "built-in",
        AgentSource::Global => "global",
        AgentSource::Session => "session",
    }
}

/// Load the registry for a session and flatten it into a sorted owned `Vec`.
fn load_agents_snapshot(session: &Session) -> Vec<AgentDef> {
    let registry = load_registry(Some(&session.path));
    // `list(false)` returns every agent (including hidden) sorted by name; we
    // own a clone so the snapshot survives the registry being dropped.
    registry.list(false).into_iter().cloned().collect()
}
