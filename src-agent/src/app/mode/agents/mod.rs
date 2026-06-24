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
use crate::model::session::Session;
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
    /// OpenRouter model slug; empty = inherit the session model.
    Model,
    /// OpenRouter provider slug; empty = default routing.
    Provider,
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
            AgentEditField::Provider => "provider",
            AgentEditField::Tools => "tools",
            AgentEditField::Body => "prompt",
        }
    }
}

/// Field order while EDITING an existing agent (no name row).
const EDIT_FIELDS: &[AgentEditField] = &[
    AgentEditField::Description,
    AgentEditField::Model,
    AgentEditField::Provider,
    AgentEditField::Tools,
    AgentEditField::Body,
];

/// Field order while CREATING a new agent (name row first).
const CREATE_FIELDS: &[AgentEditField] = &[
    AgentEditField::Name,
    AgentEditField::Description,
    AgentEditField::Model,
    AgentEditField::Provider,
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
    /// Draft: model slug (empty = inherit).
    pub draft_model: String,
    /// Draft: provider slug (empty = default).
    pub draft_provider: String,
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
            draft_model: String::new(),
            draft_provider: String::new(),
            draft_tools: String::new(),
            draft_body: String::new(),
            session_dir: session.path.clone(),
            tool_picker: None,
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

    /// Mutable handle to the draft buffer for the highlighted field.
    fn draft_mut(&mut self) -> &mut String {
        match self.field {
            AgentEditField::Name => &mut self.draft_name,
            AgentEditField::Description => &mut self.draft_description,
            AgentEditField::Model => &mut self.draft_model,
            AgentEditField::Provider => &mut self.draft_provider,
            AgentEditField::Tools => &mut self.draft_tools,
            AgentEditField::Body => &mut self.draft_body,
        }
    }

    /// Immutable handle to the draft buffer for `f` (view-side reads).
    pub fn draft(&self, f: AgentEditField) -> &str {
        match f {
            AgentEditField::Name => &self.draft_name,
            AgentEditField::Description => &self.draft_description,
            AgentEditField::Model => &self.draft_model,
            AgentEditField::Provider => &self.draft_provider,
            AgentEditField::Tools => &self.draft_tools,
            AgentEditField::Body => &self.draft_body,
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
        self.draft_model = a.model.clone().unwrap_or_default();
        self.draft_provider = a.provider.clone().unwrap_or_default();
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
        self.draft_model = String::new();
        self.draft_provider = String::new();
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
        let opt = |s: &str| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        AgentDef {
            name,
            description: self.draft_description.trim().to_string(),
            model: opt(&self.draft_model),
            provider: opt(&self.draft_provider),
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
