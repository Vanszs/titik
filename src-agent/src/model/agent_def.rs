//! Agent-definition data layer: load, merge, and persist agent definitions.
//!
//! An agent is a YAML-frontmatter + markdown file. The markdown BODY is the
//! agent's system prompt; the YAML frontmatter holds its metadata. The agent's
//! NAME is the filename stem, lowercased (`PINEAPPLE.md` → `pineapple`).
//!
//! ```text
//! ---
//! description: When to use this agent.
//! model: openai/gpt-oss-20b      # optional; None inherits the session model
//! tools: [read, grep, glob]      # allow-list; "task" is never auto-included
//! steps: 15                      # optional iteration cap
//! ---
//! You are a focused subagent. Do X, then report Y.   ← the system prompt
//! ```
//!
//! **Loader tiers** (later overrides earlier, by lowercased name):
//! 1. Built-in agents compiled into the binary ([`builtin_agents`]).
//! 2. Global agents from `~/.simple-coder/agents/*.md`.
//! 3. Session agents from `<session_dir>/agents/*.md`.
//!
//! After merging, `disable: true` removes a name from the registry, and
//! `hidden: true` keeps it but hides it from menus. A single malformed file is
//! logged and skipped — it never panics or blocks the rest of the registry.

// This is the data layer + public API; its consumers (the `/agents` UI and the
// `task` tool) are wired up in a later change. Until then the public surface is
// unreferenced from the binary, which is intentional — silence dead-code here
// rather than littering each item with `#[allow]`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::model::store::base_dir;

/// The safe read-only default tool set used when an agent declares no `tools`.
const DEFAULT_TOOLS: [&str; 4] = ["read", "grep", "glob", "dir_list"];

/// The recursion-guard tool that is NEVER auto-included in an agent's allow-list.
const TASK_TOOL: &str = "task";

// ---------------------------------------------------------------------------
// AgentDef
// ---------------------------------------------------------------------------

/// Source origin of the agent definition (determines load tier / precedence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentSource {
    /// Built-in agent compiled into the binary.
    Builtin,
    /// Global agent from `~/.simple-coder/agents/`.
    Global,
    /// Session-specific agent from `<session_dir>/agents/`.
    #[default]
    Session,
}

/// One agent definition loaded from frontmatter + markdown.
///
/// Frontmatter fields are deserialized directly; `name`, `prompt`, `source`,
/// and `file_path` are runtime-only state (`#[serde(skip)]`) populated by the
/// loader.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    /// Agent name — the filename stem, lowercased (`PINEAPPLE.md` → `pineapple`).
    /// Validated to alnum + dash, no path traversal. The agent identifier.
    #[serde(skip)]
    pub name: String,

    /// User-facing description (when to use this agent; shown in menus). Required.
    pub description: String,

    /// OpenRouter model slug (e.g. `"openai/gpt-oss-20b"`), or `None` to inherit
    /// the session model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// OpenRouter provider routing slug (e.g. `"groq"`). `None` means default
    /// routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    /// Allow-list of tool names this agent may invoke. The `"task"` tool is never
    /// auto-included (recursion guard). An empty/absent list means the safe
    /// read-only default `[read, grep, glob, dir_list]`.
    #[serde(default)]
    pub tools: Vec<String>,

    /// Maximum agentic iterations for this agent (reasoning-loop cap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,

    /// Reasoning/thinking effort (`""`, `"off"`, `"low"`, `"high"`, `"max"`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    /// Sampling temperature (typically `0.0`–`2.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Theme accent (color name or hex code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,

    /// When `true`, hide this agent from menus but keep it in the registry.
    #[serde(default)]
    pub hidden: bool,

    /// When `true`, remove this agent from the registry (disables a built-in or
    /// file agent). Processed at load time.
    #[serde(default)]
    pub disable: bool,

    /// The markdown body following the frontmatter — the agent system prompt.
    /// Not from frontmatter; extracted after the split.
    #[serde(skip)]
    pub prompt: String,

    /// Where this agent was loaded from. Populated by the loader.
    #[serde(skip)]
    pub source: AgentSource,

    /// Absolute path to the `.md` file, or `None` for built-ins. Used for
    /// save/delete.
    #[serde(skip)]
    pub file_path: Option<PathBuf>,
}

impl Default for AgentDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            model: None,
            provider: None,
            tools: Vec::new(),
            steps: None,
            effort: None,
            temperature: None,
            color: None,
            hidden: false,
            disable: false,
            prompt: String::new(),
            source: AgentSource::Session,
            file_path: None,
        }
    }
}

impl AgentDef {
    /// Construct a built-in agent (no file path, `source = Builtin`).
    pub fn builtin(name: &str, description: &str, prompt: &str, tools: Vec<String>) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            prompt: prompt.to_string(),
            tools,
            source: AgentSource::Builtin,
            ..Default::default()
        }
    }

    /// The effective tool allow-list for this agent.
    ///
    /// Falls back to the safe read-only default when `tools` is empty, and always
    /// strips the `"task"` tool (recursion guard) regardless of what was declared.
    pub fn effective_tools(&self) -> Vec<String> {
        let base: Vec<String> = if self.tools.is_empty() {
            DEFAULT_TOOLS.iter().map(|t| t.to_string()).collect()
        } else {
            self.tools.clone()
        };
        base.into_iter().filter(|t| t != TASK_TOOL).collect()
    }

    /// Render this agent into a frontmatter + markdown string for writing to disk.
    ///
    /// Only emits fields that are explicitly set; the read-only default tool set
    /// is treated as "unset" and omitted so the round-trip stays minimal.
    pub fn to_markdown(&self) -> String {
        let mut fm = serde_yaml_ng::Mapping::new();
        let key = |s: &str| serde_yaml_ng::Value::String(s.to_string());
        let sval = |s: &str| serde_yaml_ng::Value::String(s.to_string());

        // Required.
        fm.insert(key("description"), sval(&self.description));

        if let Some(model) = &self.model {
            fm.insert(key("model"), sval(model));
        }
        if let Some(provider) = &self.provider {
            fm.insert(key("provider"), sval(provider));
        }
        if !self.tools.is_empty() {
            let seq = self.tools.iter().map(|t| sval(t)).collect();
            fm.insert(key("tools"), serde_yaml_ng::Value::Sequence(seq));
        }
        if let Some(steps) = self.steps {
            fm.insert(key("steps"), serde_yaml_ng::Value::Number(steps.into()));
        }
        if let Some(effort) = &self.effort {
            fm.insert(key("effort"), sval(effort));
        }
        if let Some(temperature) = self.temperature {
            // f32 -> f64 keeps the YAML number well-formed.
            fm.insert(
                key("temperature"),
                serde_yaml_ng::Value::Number((temperature as f64).into()),
            );
        }
        if let Some(color) = &self.color {
            fm.insert(key("color"), sval(color));
        }
        if self.hidden {
            fm.insert(key("hidden"), serde_yaml_ng::Value::Bool(true));
        }
        if self.disable {
            fm.insert(key("disable"), serde_yaml_ng::Value::Bool(true));
        }

        let fm_str = serde_yaml_ng::to_string(&serde_yaml_ng::Value::Mapping(fm))
            .unwrap_or_default();
        let fm_str = fm_str.trim_end();
        format!("---\n{fm_str}\n---\n{}", self.prompt)
    }
}

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Split a `.md` file into its optional YAML frontmatter and markdown body.
///
/// Format: `---\n<yaml>\n---\n<body>`. A file that does NOT start with `---` is
/// treated as all body (no frontmatter), which is not an error. An opening `---`
/// with no closing delimiter IS an error.
fn split_frontmatter(content: &str) -> Result<(Option<&str>, &str)> {
    // Only treat a leading `---` followed by a newline as a frontmatter fence,
    // so a body that merely starts with a markdown horizontal rule (`---\ntext`)
    // is handled, but a Setext heading like "Title\n---" is not misread.
    let after_fence = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"));
    let Some(rest) = after_fence else {
        return Ok((None, content));
    };
    // Find the closing fence: a line that is exactly `---`.
    let Some((fm, body)) = find_closing_fence(rest) else {
        return Err(anyhow!("frontmatter not closed (missing closing ---)"));
    };
    Ok((Some(fm), body.trim_start_matches(['\n', '\r'])))
}

/// Locate the closing `---` fence line in `rest` (everything after the opening
/// fence). Returns `(frontmatter, body)` split around it, or `None` if absent.
fn find_closing_fence(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            let fm = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((fm, body));
        }
        offset += line.len();
    }
    None
}

/// Deserialize frontmatter YAML into a partial [`AgentDef`].
///
/// All frontmatter fields are optional (`Option` or serde default), so missing
/// keys fall back to defaults; `name`/`prompt`/`source`/`file_path` stay at their
/// `Default` until the loader fills them in.
fn parse_frontmatter(yaml_str: &str) -> Result<AgentDef> {
    // An empty frontmatter block deserializes to the default agent.
    if yaml_str.trim().is_empty() {
        return Ok(AgentDef::default());
    }
    let agent: AgentDef = serde_yaml_ng::from_str(yaml_str)?;
    Ok(agent)
}

/// Validate an agent name derived from a filename stem.
///
/// Rules: lowercase ASCII alphanumeric + dash only; no path traversal (`/`,
/// `\`, `..`); no leading/trailing dash; non-empty.
fn validate_agent_name(stem: &str) -> Result<String> {
    let lower = stem.to_lowercase();

    if lower.contains('/') || lower.contains('\\') || lower.contains("..") {
        return Err(anyhow!("invalid agent name: path traversal detected"));
    }
    if lower.is_empty() {
        return Err(anyhow!("invalid agent name: empty"));
    }
    if !lower
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(anyhow!(
            "invalid agent name: only alphanumeric and dash allowed"
        ));
    }
    if lower.starts_with('-') || lower.ends_with('-') {
        return Err(anyhow!("invalid agent name: no leading/trailing dash"));
    }
    Ok(lower)
}

/// Parse an in-memory `.md` string into an [`AgentDef`].
///
/// Shared by [`load_agent_file`] and the unit tests. `name`/`source` are caller
/// supplied; `file_path` is `None` here (set by the file loader).
fn parse_agent(name: &str, content: &str, source: AgentSource) -> Result<AgentDef> {
    let (fm_str, body) = split_frontmatter(content)?;

    let mut agent = match fm_str {
        Some(fm) => parse_frontmatter(fm)?,
        None => AgentDef::default(),
    };

    agent.name = validate_agent_name(name)?;
    agent.source = source;
    agent.prompt = body.to_string();

    if agent.description.trim().is_empty() {
        return Err(anyhow!(
            "agent {} missing required 'description'",
            agent.name
        ));
    }
    Ok(agent)
}

/// Load and parse a single `.md` agent file from disk.
///
/// The name is taken from the filename stem (lowercased + validated); the body
/// becomes the prompt. Any error (IO, UTF-8, malformed YAML, missing
/// description, bad name) is returned so the caller can skip the file non-fatally.
pub fn load_agent_file(path: &Path, source: AgentSource) -> Result<AgentDef> {
    let content = std::fs::read_to_string(path)?;
    let stem = path
        .file_stem()
        .ok_or_else(|| anyhow!("no filename"))?
        .to_string_lossy()
        .into_owned();

    let mut agent = parse_agent(&stem, &content, source)?;
    agent.file_path = Some(path.to_path_buf());
    Ok(agent)
}

// ---------------------------------------------------------------------------
// Directories
// ---------------------------------------------------------------------------

/// Returns `~/.simple-coder/agents/` (the global agent registry directory).
pub fn global_agents_dir() -> Result<PathBuf> {
    Ok(base_dir()?.join("agents"))
}

/// Returns `<session_dir>/agents/` (session-specific agents).
pub fn session_agents_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("agents")
}

/// Scope an agent operation targets: the global registry or a session directory.
#[derive(Debug, Clone, Copy)]
pub enum AgentScope<'a> {
    /// `~/.simple-coder/agents/`.
    Global,
    /// `<session_dir>/agents/`.
    Session(&'a Path),
}

/// Resolve the on-disk agents directory for a scope.
pub fn agents_dir(scope: AgentScope) -> Result<PathBuf> {
    match scope {
        AgentScope::Global => global_agents_dir(),
        AgentScope::Session(session_dir) => Ok(session_agents_dir(session_dir)),
    }
}

// ---------------------------------------------------------------------------
// Built-in agents
// ---------------------------------------------------------------------------

/// Construct the set of built-in agents compiled into the binary.
///
/// Built-ins have `model: None` (they inherit the session model). Their prompts
/// are embedded from `src-misc/` at compile time.
fn builtin_agents() -> Vec<AgentDef> {
    let tools = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    vec![
        AgentDef {
            steps: Some(15),
            ..AgentDef::builtin(
                "explore",
                "Read-only code locator: find where things are defined and used",
                include_str!("../../../src-misc/agent-explore-prompt.txt"),
                tools(&["read", "grep", "glob", "dir_list"]),
            )
        },
        AgentDef {
            steps: Some(25),
            ..AgentDef::builtin(
                "general",
                "General-purpose subagent for a scoped task",
                include_str!("../../../src-misc/agent-general-prompt.txt"),
                // NO "task" — recursion guard.
                tools(&["read", "grep", "glob", "dir_list", "edit", "write", "bash"]),
            )
        },
    ]
}

// ---------------------------------------------------------------------------
// Registry + loader
// ---------------------------------------------------------------------------

/// The in-memory agent registry: lowercased name → [`AgentDef`].
#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    agents: HashMap<String, AgentDef>,
}

impl AgentRegistry {
    /// Load the full registry for a session (or `None` for built-in + global only).
    ///
    /// Merge order, later overriding earlier by lowercased name: built-in, then
    /// global, then session. After merging, `disable: true` agents are removed
    /// from the registry.
    pub fn load(session_dir: Option<&Path>) -> Self {
        let mut agents: HashMap<String, AgentDef> = HashMap::new();

        // Tier 1: built-ins.
        for agent in builtin_agents() {
            agents.insert(agent.name.clone(), agent);
        }

        // Tier 2: global.
        if let Ok(dir) = global_agents_dir() {
            load_agents_from_dir(&dir, AgentSource::Global, &mut agents);
        }

        // Tier 3: session.
        if let Some(session_path) = session_dir {
            let dir = session_agents_dir(session_path);
            load_agents_from_dir(&dir, AgentSource::Session, &mut agents);
        }

        // Post-merge: drop disabled agents (a disabling file overrode any prior
        // tier of the same name above, so this removes the name entirely).
        agents.retain(|_, a| !a.disable);

        Self { agents }
    }

    /// Get an agent by name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.agents.get(&name.to_lowercase())
    }

    /// List agents, sorted by name. When `exclude_hidden` is true, hidden agents
    /// are omitted (they remain in the registry and are still resolvable by name).
    pub fn list(&self, exclude_hidden: bool) -> Vec<&AgentDef> {
        let mut out: Vec<&AgentDef> = self
            .agents
            .values()
            .filter(|a| !exclude_hidden || !a.hidden)
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// All agents as a map for advanced queries.
    pub fn all(&self) -> &HashMap<String, AgentDef> {
        &self.agents
    }

    /// Number of agents in the registry.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

/// Load every `*.md` file in `dir`, parse it, and merge into `agents`.
///
/// A missing directory is fine (nothing to load). Per-file errors are logged to
/// stderr and skipped — one corrupt file never breaks the registry. Later files
/// override earlier entries of the same lowercased name.
fn load_agents_from_dir(
    dir: &Path,
    source: AgentSource,
    agents: &mut HashMap<String, AgentDef>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // Missing/unreadable dir is not an error: nothing to merge.
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        match load_agent_file(&path, source) {
            Ok(agent) => {
                agents.insert(agent.name.clone(), agent);
            }
            Err(e) => {
                eprintln!("Warning: skipped agent {}: {e}", path.display());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API for the UI layer
// ---------------------------------------------------------------------------

/// Load the registry (built-in + global + optional session agents).
pub fn load_registry(session_dir: Option<&Path>) -> AgentRegistry {
    AgentRegistry::load(session_dir)
}

/// Persist an agent definition into a scope, creating the directory if needed.
///
/// The file is `<scope_dir>/<name>.md` (overwritten if it exists). The agent's
/// `name` is re-validated to keep the filename safe. Built-in prompts can be
/// saved out to disk this way (which then shadows the built-in on next load).
pub fn save_agent(scope: AgentScope, agent: &AgentDef) -> Result<PathBuf> {
    let name = validate_agent_name(&agent.name)?;
    let dir = agents_dir(scope)?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.md"));
    std::fs::write(&path, agent.to_markdown())?;
    Ok(path)
}

/// Delete an agent file `<scope_dir>/<name>.md`.
///
/// Returns an error if the name is invalid. A missing file is treated as success
/// (idempotent delete) so a double-delete from the UI does not error.
pub fn delete_agent(scope: AgentScope, name: &str) -> Result<()> {
    let name = validate_agent_name(name)?;
    let dir = agents_dir(scope)?;
    let path = dir.join(format!("{name}.md"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_frontmatter_and_body() {
        let content = "---\n\
description: Do the thing\n\
model: openai/gpt-oss-20b\n\
provider: groq\n\
tools: [read, grep, edit]\n\
steps: 12\n\
effort: high\n\
temperature: 0.4\n\
color: cyan\n\
hidden: true\n\
---\n\
You are a focused subagent.\nReport tersely.";

        let a = parse_agent("MyAgent", content, AgentSource::Global).unwrap();
        assert_eq!(a.name, "myagent"); // lowercased
        assert_eq!(a.description, "Do the thing");
        assert_eq!(a.model.as_deref(), Some("openai/gpt-oss-20b"));
        assert_eq!(a.provider.as_deref(), Some("groq"));
        assert_eq!(a.tools, vec!["read", "grep", "edit"]);
        assert_eq!(a.steps, Some(12));
        assert_eq!(a.effort.as_deref(), Some("high"));
        assert_eq!(a.temperature, Some(0.4));
        assert_eq!(a.color.as_deref(), Some("cyan"));
        assert!(a.hidden);
        assert_eq!(a.source, AgentSource::Global);
        assert_eq!(
            a.prompt,
            "You are a focused subagent.\nReport tersely."
        );
    }

    #[test]
    fn body_only_file_has_defaults_and_prompt() {
        // No frontmatter fence at all: the whole content is the body. But the
        // loader requires a description, so a pure body-only file is rejected.
        // Here we assert the SPLIT + default behavior via a frontmatter that
        // only carries the required description, leaving everything else default.
        let content = "---\ndescription: Minimal\n---\nThis is the system prompt body.";
        let a = parse_agent("minimal", content, AgentSource::Session).unwrap();
        assert_eq!(a.prompt, "This is the system prompt body.");
        assert!(a.model.is_none());
        assert!(a.provider.is_none());
        assert!(a.tools.is_empty());
        assert!(a.steps.is_none());
        assert!(!a.hidden);
        assert!(!a.disable);
        // Empty tools -> effective tools fall back to the read-only default.
        assert_eq!(
            a.effective_tools(),
            vec!["read", "grep", "glob", "dir_list"]
        );
    }

    #[test]
    fn pure_body_no_frontmatter_splits_as_all_body() {
        // split_frontmatter alone: a file with no opening fence is all body.
        let (fm, body) = split_frontmatter("just a body, no fence\nsecond line").unwrap();
        assert!(fm.is_none());
        assert_eq!(body, "just a body, no fence\nsecond line");
    }

    #[test]
    fn malformed_frontmatter_is_an_error_not_a_panic() {
        // Unclosed fence.
        let unclosed = "---\ndescription: oops\nno closing fence here";
        assert!(parse_agent("x", unclosed, AgentSource::Global).is_err());

        // Invalid YAML inside a closed fence.
        let bad_yaml = "---\ndescription: [unterminated\n---\nbody";
        assert!(parse_agent("x", bad_yaml, AgentSource::Global).is_err());

        // Missing required description.
        let no_desc = "---\nmodel: foo/bar\n---\nbody";
        assert!(parse_agent("x", no_desc, AgentSource::Global).is_err());
    }

    #[test]
    fn tools_allow_list_parsed_and_task_stripped() {
        let content =
            "---\ndescription: d\ntools: [read, task, bash]\n---\nbody";
        let a = parse_agent("t", content, AgentSource::Global).unwrap();
        // Raw parse keeps what was written...
        assert_eq!(a.tools, vec!["read", "task", "bash"]);
        // ...but the effective allow-list strips the "task" recursion guard.
        assert_eq!(a.effective_tools(), vec!["read", "bash"]);
    }

    #[test]
    fn builtins_present_with_expected_shape() {
        let builtins = builtin_agents();
        assert_eq!(builtins.len(), 2);

        let explore = builtins.iter().find(|a| a.name == "explore").unwrap();
        assert_eq!(explore.source, AgentSource::Builtin);
        assert!(explore.model.is_none()); // inherit
        assert_eq!(explore.steps, Some(15));
        assert_eq!(explore.tools, vec!["read", "grep", "glob", "dir_list"]);
        assert!(!explore.prompt.trim().is_empty());

        let general = builtins.iter().find(|a| a.name == "general").unwrap();
        assert_eq!(general.steps, Some(25));
        assert!(general.tools.contains(&"bash".to_string()));
        // The general agent must never carry the task tool.
        assert!(!general.tools.contains(&"task".to_string()));
    }

    #[test]
    fn precedence_session_over_global_over_builtin() {
        let mut agents: HashMap<String, AgentDef> = HashMap::new();

        // Built-in "explore".
        for a in builtin_agents() {
            agents.insert(a.name.clone(), a);
        }
        assert_eq!(agents["explore"].source, AgentSource::Builtin);

        // Global overrides built-in.
        let global = parse_agent(
            "explore",
            "---\ndescription: global explore\n---\nglobal body",
            AgentSource::Global,
        )
        .unwrap();
        agents.insert(global.name.clone(), global);
        assert_eq!(agents["explore"].source, AgentSource::Global);
        assert_eq!(agents["explore"].description, "global explore");

        // Session overrides global.
        let session = parse_agent(
            "explore",
            "---\ndescription: session explore\n---\nsession body",
            AgentSource::Session,
        )
        .unwrap();
        agents.insert(session.name.clone(), session);
        assert_eq!(agents["explore"].source, AgentSource::Session);
        assert_eq!(agents["explore"].description, "session explore");
    }

    #[test]
    fn disable_removes_agent_from_registry() {
        let mut agents: HashMap<String, AgentDef> = HashMap::new();
        for a in builtin_agents() {
            agents.insert(a.name.clone(), a);
        }
        // A disabling override of a built-in.
        let off = parse_agent(
            "general",
            "---\ndescription: gone\ndisable: true\n---\nbody",
            AgentSource::Global,
        )
        .unwrap();
        agents.insert(off.name.clone(), off);

        // Same retain step the loader applies.
        agents.retain(|_, a| !a.disable);

        assert!(!agents.contains_key("general"));
        assert!(agents.contains_key("explore"));
    }

    #[test]
    fn name_sanitization_rules() {
        // Uppercasing -> lowercase.
        assert_eq!(validate_agent_name("PINEAPPLE").unwrap(), "pineapple");
        assert_eq!(validate_agent_name("My-Agent").unwrap(), "my-agent");

        // Path traversal rejected.
        assert!(validate_agent_name("../x").is_err());
        assert!(validate_agent_name("..").is_err());
        assert!(validate_agent_name("a/b").is_err());
        assert!(validate_agent_name("a\\b").is_err());

        // Empty rejected.
        assert!(validate_agent_name("").is_err());

        // Leading/trailing dash rejected.
        assert!(validate_agent_name("-x").is_err());
        assert!(validate_agent_name("x-").is_err());

        // Illegal chars rejected.
        assert!(validate_agent_name("a b").is_err());
        assert!(validate_agent_name("a.b").is_err());
    }

    #[test]
    fn to_markdown_round_trips_set_fields_only() {
        let a = AgentDef {
            name: "rt".to_string(),
            description: "round trip".to_string(),
            model: Some("openai/gpt-oss-20b".to_string()),
            tools: vec!["read".to_string(), "edit".to_string()],
            steps: Some(7),
            prompt: "Body here.".to_string(),
            ..Default::default()
        };
        let md = a.to_markdown();
        assert!(md.starts_with("---\n"));
        assert!(md.contains("description: round trip"));
        assert!(md.contains("model: openai/gpt-oss-20b"));
        assert!(md.ends_with("Body here."));
        // provider/effort/etc. are unset -> not emitted.
        assert!(!md.contains("provider:"));
        assert!(!md.contains("effort:"));

        // Re-parse and confirm the set fields survive.
        let b = parse_agent("rt", &md, AgentSource::Session).unwrap();
        assert_eq!(b.description, "round trip");
        assert_eq!(b.model.as_deref(), Some("openai/gpt-oss-20b"));
        assert_eq!(b.tools, vec!["read", "edit"]);
        assert_eq!(b.steps, Some(7));
        assert_eq!(b.prompt, "Body here.");
    }
}
