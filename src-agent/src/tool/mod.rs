//! Tool foundation for the agentic loop.
//!
//! A [`Tool`] is a callable shaped for OpenRouter function-calling: it exposes a
//! name, a description, and a JSON-Schema `parameters` object, and runs against a
//! shared [`ToolCtx`] (the session's workspace root + the background file cache).
//! [`all_tools`] returns the built-in set; [`resolve`] sandboxes every path so a
//! tool can never touch anything outside the workspace.
//!
//! The trait, the registry, the tool structs, and [`resolve`] are driven by the
//! agentic loop: `service::openrouter::stream_complete` advertises a caller-chosen
//! subset of the tool set to the model (the main loop uses [`main_tool_names`],
//! which hides agent-only tools; each sub-agent advertises only its allow-list),
//! and `app::runtime::stream::run_tool` dispatches the model's requested calls
//! back through [`Tool::run`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use anyhow::{bail, Result};
use serde_json::Value;

pub mod dircache;
pub mod fs;
pub mod internet;
pub mod memory;
pub mod pong;
pub mod search;
pub mod shell;
pub mod task;

pub use dircache::DirCache;

/// Shared context handed to every tool invocation.
pub struct ToolCtx {
    /// Absolute workspace root (the session's primary workdir). All tool paths
    /// are resolved against this and may not escape it.
    pub workspace: PathBuf,
    /// All configured workspace roots (may be >1 when the user lists multiple
    /// workdirs in settings). Indexed as [0], [1], etc. in `@`-prefixed paths.
    pub workspaces: Vec<PathBuf>,
    pub dir_cache: Arc<RwLock<DirCache>>,
    /// The active session's memory directory (`<session_dir>/memory`), where
    /// `MEMORY.md` lives. `None` when no session is active.
    pub memory_dir: Option<PathBuf>,
}

/// Parse a `[N]` workspace-index prefix from the start of a path string.
/// If the path starts with `[digits]`, returns `(index, rest)`.
/// Otherwise returns `(0, original)` — a bare path resolves against workspace 0.
pub fn parse_ws_prefix(path: &str) -> (usize, &str) {
    if !path.starts_with('[') {
        return (0, path);
    }
    if let Some(end) = path.find(']') {
        if let Ok(idx) = path[1..end].parse::<usize>() {
            return (idx, &path[end + 1..]);
        }
    }
    (0, path)
}

/// A callable tool, shaped for OpenRouter function-calling: `parameters` is a
/// JSON Schema object; `run` takes the decoded arguments and returns a string
/// result fed back to the model.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String>;
}

/// The built-in tool set.
pub fn all_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(fs::Read),
        Box::new(search::Grep),
        Box::new(search::Glob),
        Box::new(fs::Write),
        Box::new(fs::Edit),
        Box::new(fs::Delete),
        Box::new(shell::Bash),
        Box::new(fs::DirList),
        Box::new(dircache::DirCacheUpdate),
        Box::new(pong::Pong),
        Box::new(memory::Remember),
        Box::new(task::Task),
        Box::new(internet::WebFetch),
        Box::new(internet::WebSearch),
        Box::new(internet::Research),
    ]
}

/// Tools that are NEVER advertised to the main chat model. They are reachable
/// only by sub-agents whose allow-list names them (the sub-agent caller
/// advertises its own `tools`, not [`main_tool_names`]). `research` is heavy
/// (spawns a real browser) and is intentionally reserved for the `researcher`
/// sub-agent so the main model delegates rather than driving it directly.
const INTERNAL_ONLY: &[&str] = &["research"];

/// Tool names advertised to the MAIN chat model (everything except agent-only
/// tools). Used by the interactive loop's `stream_complete` call so the main
/// model never sees [`INTERNAL_ONLY`] tools.
pub fn main_tool_names() -> Vec<String> {
    all_tools()
        .iter()
        .map(|t| t.name().to_string())
        .filter(|n| !INTERNAL_ONLY.contains(&n.as_str()))
        .collect()
}

/// Resolve a path (optionally with `[N]` workspace-index prefix) and enforce
/// containment. A bare path like `src/main.rs` resolves against workspace 0.
/// A prefixed path like `[2]src/main.rs` resolves against workspace 2.
pub fn resolve(workspaces: &[PathBuf], rel: &str) -> Result<PathBuf> {
    let (ws_idx, bare) = parse_ws_prefix(rel);
    let base = workspaces.get(ws_idx)
        .ok_or_else(|| anyhow::anyhow!("workspace index [{ws_idx}] out of range (have {})", workspaces.len()))?;
    let ws = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let joined = ws.join(bare);
    // Canonicalize as far as exists, then re-append the non-existent tail, so
    // `..` tricks are normalised out before the containment check.
    let candidate = match joined.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            let mut existing = joined.as_path();
            let mut tail: Vec<std::ffi::OsString> = Vec::new();
            while !existing.exists() {
                match existing.file_name() {
                    Some(n) => tail.push(n.to_os_string()),
                    None => break,
                }
                match existing.parent() {
                    Some(p) => existing = p,
                    None => break,
                }
            }
            let mut base = existing.canonicalize().unwrap_or_else(|_| existing.to_path_buf());
            for seg in tail.iter().rev() {
                base.push(seg);
            }
            base
        }
    };
    // Containment: must be inside the resolved workspace.
    if !candidate.starts_with(&ws) {
        bail!("path '{bare}' is outside workspace [{ws_idx}]");
    }
    Ok(candidate)
}

/// Resolve a path for READ-ONLY tools, forgiving a dropped [N] prefix.
/// An explicit [N] prefix is honoured strictly (same as resolve). A BARE path
/// resolves against workspace 0 first; if that file does not exist on disk, the
/// other workspaces are tried by existence and the first physical match wins.
/// This lets weak models that drop the [N] prefix still READ a file that only
/// lives in another workspace, while writes (which keep using resolve) stay
/// strictly pinned to workspace 0 unless an explicit [N] is given.
pub fn resolve_read(workspaces: &[PathBuf], rel: &str) -> Result<PathBuf> {
    if rel.starts_with('[') {
        return resolve(workspaces, rel);
    }
    let primary = resolve(workspaces, rel)?;
    if primary.exists() {
        return Ok(primary);
    }
    for idx in 1..workspaces.len() {
        if let Ok(p) = resolve(workspaces, &format!("[{idx}]{rel}")) {
            if p.exists() {
                return Ok(p);
            }
        }
    }
    Ok(primary)
}

/// Pure tool dispatcher: given a ready [`ToolCtx`] and a [`ToolCall`], loop
/// all built-in tools, parse arguments, dispatch to the matching tool's
/// [`Tool::run`], and return the result string. Does NOT touch any app state.
///
/// Error strings match exactly what `run_tool` produced before the refactor:
/// - `"error: <msg>"` on a tool execution failure
/// - `"error: unknown tool '<name>'"` when no tool matches
pub fn execute_tool(ctx: &ToolCtx, call: &crate::dto::chat::ToolCall) -> String {
    // OpenAI/OpenRouter send `arguments` as a JSON-encoded string; an empty or
    // malformed payload degrades to `{}` so the tool sees no arguments. Sanitize
    // first: a non-delta provider may have produced a duplicated `{...}{...}`
    // string (valid JSON document + trailing copy) that `from_str` would reject
    // outright — collapsing it to one clean value here recovers the real arguments
    // (e.g. the bash `command`) instead of silently degrading to `{}`. A single
    // clean value is unchanged, so the normal path is unaffected.
    let sanitized = crate::dto::chat::sanitize_tool_arguments(&call.function.arguments);
    let args: serde_json::Value =
        serde_json::from_str(&sanitized).unwrap_or_else(|_| serde_json::json!({}));
    for tool in all_tools() {
        if tool.name() == call.function.name {
            return match tool.run(ctx, &args) {
                Ok(s) => s,
                Err(e) => format!("error: {e}"),
            };
        }
    }
    format!("error: unknown tool '{}'", call.function.name)
}

/// Find which workspace contains the given absolute path.
/// Returns the canonicalized workspace root if found.
pub fn find_workspace(workspaces: &[PathBuf], abs: &Path) -> Option<PathBuf> {
    for ws in workspaces {
        let ws = ws.canonicalize().unwrap_or_else(|_| ws.clone());
        if abs.starts_with(&ws) {
            return Some(ws);
        }
    }
    None
}
