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
pub mod search;
pub mod shell;

pub use dircache::DirCache;

/// Shared context handed to every tool invocation.
pub struct ToolCtx {
    /// Absolute workspace root (the session's workdir). All tool paths are
    /// resolved against this and may not escape it.
    pub workspace: PathBuf,
    pub dir_cache: Arc<RwLock<DirCache>>,
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
    ]
}

/// Resolve a workspace-relative path and ENFORCE it stays inside the workspace.
/// Works for existing and not-yet-created paths (canonicalizes the nearest
/// existing ancestor). Returns an error if the path escapes the workspace.
pub fn resolve(workspace: &Path, rel: &str) -> Result<PathBuf> {
    let ws = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    let joined = ws.join(rel);
    // Canonicalize as far as exists, then re-append the non-existent tail, so
    // `..` tricks are normalised out before the containment check.
    let candidate = match joined.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // Walk up to the nearest existing ancestor, canonicalize it, re-join the rest.
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
    if !candidate.starts_with(&ws) {
        bail!("path '{rel}' is outside the workspace");
    }
    Ok(candidate)
}
