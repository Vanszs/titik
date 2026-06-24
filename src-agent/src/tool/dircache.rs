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
    /// Global case-insensitive substring search over every file AND every
    /// ancestor directory in the cache.
    ///
    /// Candidate set: every file path in `self.files` plus every unique ancestor
    /// directory (rendered with a trailing "/"). Example: the file
    /// "src-agent/x/a/b/c/ages.rs" contributes itself plus the dirs
    /// "src-agent/", "src-agent/x/", "src-agent/x/a/", "src-agent/x/a/b/",
    /// "src-agent/x/a/b/c/".
    ///
    /// If `query` is empty the depth-1 entries (immediate root children, files
    /// and first-level directories) are returned capped at `limit`, matching the
    /// original `@` browse behaviour.
    ///
    /// Otherwise: keep every candidate whose full path contains `query`
    /// case-insensitively, then rank:
    ///   (a) entries whose basename (last segment, stripping any trailing "/")
    ///       STARTS WITH the query — ranked first;
    ///   (b) all others that merely contain it.
    /// Within each group, sort by ascending path length then lexicographically.
    /// Truncate to `limit`.
    pub fn search(&self, query: &str, limit: usize) -> Vec<String> {
        if query.is_empty() {
            // Depth-1 browse: same as children("") capped at limit.
            return self.children("").into_iter().take(limit).collect();
        }

        // Build the full candidate set: all files + all unique ancestor dirs.
        let q = query.to_lowercase();
        let mut dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut candidates: Vec<String> = Vec::new();

        for f in &self.files {
            // Ancestor directories for this file.
            let mut path = f.as_str();
            while let Some(i) = path.rfind('/') {
                path = &path[..i];
                let dir_entry = format!("{path}/");
                dirs.insert(dir_entry);
            }
            candidates.push(f.clone());
        }
        for d in dirs {
            candidates.push(d);
        }

        // Filter by substring match, then rank.
        let mut starts: Vec<String> = Vec::new();
        let mut contains: Vec<String> = Vec::new();
        for c in candidates {
            let cl = c.to_lowercase();
            if !cl.contains(&q) {
                continue;
            }
            // Basename: strip trailing "/" then take everything after the last "/".
            let base = {
                let stripped = c.trim_end_matches('/');
                match stripped.rfind('/') {
                    Some(i) => &stripped[i + 1..],
                    None => stripped,
                }
            };
            if base.to_lowercase().starts_with(&q) {
                starts.push(c);
            } else {
                contains.push(c);
            }
        }

        // Sort each group: shorter path first, then lexicographic.
        starts.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
        contains.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));

        starts.extend(contains);
        starts.truncate(limit);
        starts
    }

    /// Immediate children (files + subfolders) of a workspace-relative directory,
    /// derived from the cached file list. `dir` may be "", ".", "src", "src/".
    /// Files are basenames; subfolders end with "/". Sorted, deduped.
    pub fn children(&self, dir: &str) -> Vec<String> {
        let d = dir.trim().trim_start_matches("./").trim_end_matches('/');
        let prefix = if d.is_empty() || d == "." { String::new() } else { format!("{d}/") };
        let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for f in &self.files {
            if let Some(rest) = f.strip_prefix(&prefix) {
                if rest.is_empty() { continue; }
                match rest.find('/') {
                    None => { set.insert(rest.to_string()); }              // file
                    Some(j) => { set.insert(format!("{}/", &rest[..j])); } // subfolder
                }
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
