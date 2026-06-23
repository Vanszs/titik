//! The workspace directory cache + its background indexer.
//!
//! [`DirCache`] holds a flat, sorted list of gitignore-respecting relative file
//! paths for the active session's workspace. [`reindex`] rebuilds it on a
//! background thread (non-blocking) so the UI never stalls on a large tree. The
//! [`DirCacheUpdate`] tool lets the model trigger a refresh after it creates or
//! deletes files.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use anyhow::Result;
use serde_json::{json, Value};
use super::{Tool, ToolCtx};

/// Workspace file index (relative paths), refreshed in the background. Feeds
/// `@`-file autocomplete and the DirList tool.
#[derive(Default)]
pub struct DirCache {
    pub root: PathBuf,
    pub files: Vec<String>,
    pub indexing: bool,
}

/// Re-index `root` on a background thread (gitignore-respecting via the `ignore`
/// crate). Non-blocking: returns immediately; the cache is replaced when done.
pub fn reindex(root: PathBuf, cache: Arc<RwLock<DirCache>>) {
    if let Ok(mut c) = cache.write() {
        c.indexing = true;
    }
    std::thread::spawn(move || {
        let mut files: Vec<String> = Vec::new();
        for dent in ignore::WalkBuilder::new(&root).build().flatten() {
            if dent.file_type().is_some_and(|t| t.is_file()) {
                if let Ok(rel) = dent.path().strip_prefix(&root) {
                    files.push(rel.to_string_lossy().into_owned());
                }
            }
        }
        files.sort();
        if let Ok(mut c) = cache.write() {
            c.root = root;
            c.files = files;
            c.indexing = false;
        }
    });
}

impl DirCache {
    /// Immediate children (files + subfolders) for the path being typed after
    /// `@`. `partial` is split into a directory prefix (up to and including the
    /// last `/`) and a filename fragment (after it). Returns files directly in
    /// the prefix dir plus subfolder names (with a trailing `/`), filtered by the
    /// fragment (case-insensitive prefix match on the child name), sorted.
    pub fn list_at(&self, partial: &str) -> Vec<String> {
        let (prefix, frag) = match partial.rfind('/') {
            Some(i) => (&partial[..=i], &partial[i + 1..]), // prefix keeps trailing '/'
            None => ("", partial),
        };
        let frag = frag.to_lowercase();
        let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for f in &self.files {
            let rest = match f.strip_prefix(prefix) {
                Some(r) if !r.is_empty() => r,
                _ => continue,
            };
            let entry = match rest.find('/') {
                None => f.clone(),                             // file directly in prefix
                Some(j) => format!("{prefix}{}/", &rest[..j]), // subfolder (trailing '/')
            };
            // filter by the typed fragment against the child name (after prefix)
            let name = entry.strip_prefix(prefix).unwrap_or(&entry);
            if frag.is_empty() || name.to_lowercase().starts_with(&frag) {
                set.insert(entry);
            }
        }
        set.into_iter().collect()
    }
}

/// Tool: re-index the workspace file tree in the background.
pub struct DirCacheUpdate;
impl Tool for DirCacheUpdate {
    fn name(&self) -> &'static str { "dir_cache_update" }
    fn description(&self) -> &'static str {
        "Re-index the workspace file tree (respecting .gitignore) in the background. Call after creating or deleting files so the file list stays current."
    }
    fn parameters(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    fn run(&self, ctx: &ToolCtx, _args: &Value) -> Result<String> {
        reindex(ctx.workspace.clone(), ctx.dir_cache.clone());
        Ok("Re-indexing the workspace in the background.".into())
    }
}
