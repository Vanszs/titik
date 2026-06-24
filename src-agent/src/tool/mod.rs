//! Tool foundation for the agentic loop.
//!
//! A [`Tool`] is a callable shaped for OpenRouter function-calling: it exposes a
//! name, a description, and a JSON-Schema `parameters` object, and runs against a
//! shared [`ToolCtx`] (the session's workspace root + the background file cache).
//! [`all_tools`] returns the built-in set; [`resolve`] sandboxes every path so a
//! tool can never touch anything outside the workspace.
//!
//! The trait, the registry, the tool structs, and [`resolve`] are driven by the
//! agentic loop: `service::openrouter::stream_complete` advertises every tool to
//! the model, and `app::runtime::stream::run_tool` dispatches the model's
//! requested calls back through [`Tool::run`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use anyhow::{bail, Result};
use serde_json::Value;

pub mod dircache;
pub mod fs;
pub mod pong;
pub mod search;
pub mod shell;

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
    ]
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
