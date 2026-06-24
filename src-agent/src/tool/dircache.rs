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
    pub files: Vec<String>,
    pub indexing: bool,
}

/// Re-index one or more workspace roots on a background thread
/// (gitignore-respecting via the `ignore` crate). Non-blocking: returns
/// immediately; the cache is replaced when done.
///
/// When there are 2+ roots, each file is prefixed with `[N]/` where N is the
/// root's index — e.g. `[0]src/main.rs`, `[1]kantara-player/README.md`. A
/// single root produces bare paths (no prefix) for backwards compatibility.
pub fn reindex(roots: Vec<PathBuf>, cache: Arc<RwLock<DirCache>>) {
    if let Ok(mut c) = cache.write() {
        c.indexing = true;
    }
    let multi = roots.len() > 1;
    std::thread::spawn(move || {
        let mut files: Vec<String> = Vec::new();
        for (i, root) in roots.iter().enumerate() {
            for dent in ignore::WalkBuilder::new(root).build().flatten() {
                if dent.file_type().is_some_and(|t| t.is_file()) {
                    if let Ok(rel) = dent.path().strip_prefix(root) {
                        let path = rel.to_string_lossy().into_owned();
                        if multi {
                            files.push(format!("[{i}]{path}"));
                        } else {
                            files.push(path);
                        }
                    }
                }
            }
        }
        files.sort();
        if let Ok(mut c) = cache.write() {
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
            // Depth-1 browse: list top-level entries from all workspaces.
            let mut result: Vec<String> = Vec::new();
            if self.is_multi() {
                // Collect unique workspace indices from file prefixes.
                let mut ws_indices: Vec<usize> = Vec::new();
                for f in &self.files {
                    if let Some(rest) = f.strip_prefix('[') {
                        if let Some(end) = rest.find(']') {
                            if let Ok(idx) = rest[..end].parse::<usize>() {
                                if !ws_indices.contains(&idx) {
                                    ws_indices.push(idx);
                                }
                            }
                        }
                    }
                }
                ws_indices.sort();
                for idx in &ws_indices {
                    result.extend(self.children("", *idx));
                }
            } else {
                result.extend(self.children("", 0));
            }
            result.truncate(limit);
            return result;
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
    ///
    /// `ws_idx` is used to filter prefixed entries (e.g. `[0]src/main.rs`)
    /// when there are multiple workspaces. Pass 0 for single-workspace mode
    /// (prefixes are absent).
    pub fn children(&self, dir: &str, ws_idx: usize) -> Vec<String> {
        let d = dir.trim().trim_start_matches("./").trim_end_matches('/');
        let prefix = if d.is_empty() || d == "." { String::new() } else { format!("{d}/") };
        // When multiple workspaces are indexed, files are prefixed with `[N]`.
        // Strip the prefix before matching, but re-add it in the output so the
        // model can reference the workspace in subsequent tool calls.
        let ws_tag = format!("[{ws_idx}]");
        let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for f in &self.files {
            let bare = if self.is_multi() {
                match f.strip_prefix(&ws_tag) {
                    Some(rest) => rest,
                    None => continue, // belongs to a different workspace
                }
            } else {
                f.as_str()
            };
            if let Some(rest) = bare.strip_prefix(&prefix) {
                if rest.is_empty() { continue; }
                let entry = if self.is_multi() {
                    match rest.find('/') {
                        None => format!("[{ws_idx}]{rest}"),
                        Some(j) => format!("[{ws_idx}]{}/", &rest[..j]),
                    }
                } else {
                    match rest.find('/') {
                        None => rest.to_string(),
                        Some(j) => format!("{}/", &rest[..j]),
                    }
                };
                set.insert(entry);
            }
        }
        set.into_iter().collect()
    }

    /// True when the cache holds files from 2+ workspaces (prefixed with `[N]`).
    pub fn is_multi(&self) -> bool {
        self.files.first().is_some_and(|f| f.starts_with('['))
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
        reindex(ctx.workspaces.clone(), ctx.dir_cache.clone());
        Ok("Re-indexing the workspace in the background.".into())
    }
}
