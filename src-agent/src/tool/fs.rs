//! Sandboxed filesystem tools: list / read / write / delete.
//!
//! Every path argument is resolved through [`super::resolve`], which pins it
//! inside the session workspace — a tool can never read or write outside it.
//! These structs implement [`Tool`] and are advertised to the model via
//! [`super::all_tools`]; the agentic loop dispatches the model's requested calls
//! through [`Tool::run`].

use std::path::Path;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use super::{resolve, resolve_read, Tool, ToolCtx};

/// Pull a required string argument out of the decoded JSON args.
fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing required string argument '{key}'"))
}

/// Max directory entries to list in a not-found error before summarising the rest.
const NOT_FOUND_MAX_ENTRIES: usize = 30;

/// Build an ACTIONABLE "does not exist" error for a path that resolved cleanly
/// (inside the workspace) but points at a missing file.
///
/// Instead of a dead-end "X does not exist", we walk up from the requested path
/// to the nearest EXISTING ancestor directory (never escaping the workspace —
/// `abs` is already proven to be inside it by [`super::resolve`]) and list that
/// directory's entries so the model can retry with a correct path in the SAME
/// turn. Listing reuses the cache-backed [`super::dircache::DirCache::children`]
/// (gitignore-aware, sorted, folders suffixed with '/'); if the ancestor is not
/// in the index we fall back to a direct `read_dir` so output is still useful.
/// Entries are capped at [`NOT_FOUND_MAX_ENTRIES`] with a "… (N more)" note, and
/// we always point at `glob` as the escape hatch.
fn not_found_help(ctx: &ToolCtx, abs: &Path, rel: &str) -> String {
    // Parse workspace index from the path prefix.
    let (ws_idx, _bare) = super::parse_ws_prefix(rel);
    // Find which workspace contains the path, and use it as the floor.
    let ws = super::find_workspace(&ctx.workspaces, abs)
        .or_else(|| ctx.workspaces.get(ws_idx).cloned())
        .or_else(|| ctx.workspaces.first().cloned())
        .map(|p| p.canonicalize().unwrap_or(p))
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // Walk up `abs` to the nearest ancestor that exists AND is a directory,
    // stopping at the workspace root.
    let mut ancestor = abs.parent();
    while let Some(p) = ancestor {
        if p.is_dir() {
            break;
        }
        if p == ws {
            break;
        }
        ancestor = p.parent();
    }
    // Fall back to the workspace root if the walk produced nothing usable.
    let dir_abs = match ancestor {
        Some(p) if p.is_dir() => p,
        _ => ws.as_path(),
    };

    // Workspace-relative label for the ancestor ("" / "." -> the root itself).
    let dir_rel = dir_abs
        .strip_prefix(&ws)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir_label = if dir_rel.is_empty() {
        ".".to_string()
    } else {
        format!("{dir_rel}/")
    };

    // List the ancestor's entries. Prefer the gitignore-aware cache; if it has
    // nothing for this dir (e.g. not yet indexed / ignored), read the dir live.
    let mut entries: Vec<String> = ctx
        .dir_cache
        .read()
        .map(|c| c.children(&dir_rel, ws_idx))
        .unwrap_or_default();
    if entries.is_empty() {
        if let Ok(rd) = std::fs::read_dir(dir_abs) {
            for ent in rd.flatten() {
                let mut name = ent.file_name().to_string_lossy().into_owned();
                if ent.path().is_dir() {
                    name.push('/');
                }
                entries.push(name);
            }
            entries.sort();
        }
    }

    let total = entries.len();
    let shown = total.min(NOT_FOUND_MAX_ENTRIES);

    let mut msg = format!("'{rel}' does not exist.\n");
    if total == 0 {
        msg.push_str(&format!("Nearest existing directory '{dir_label}' is empty."));
    } else {
        msg.push_str(&format!(
            "Nearest existing directory '{dir_label}' contains:\n"
        ));
        for e in entries.iter().take(shown) {
            msg.push_str("  ");
            msg.push_str(e);
            msg.push('\n');
        }
        if total > shown {
            msg.push_str(&format!("  … ({} more)\n", total - shown));
        }
    }
    msg.push_str(
        "Pick a correct path from these (descend into subdirectories shown with '/'), \
         or use the `glob` tool to locate the file by name.",
    );
    msg
}

/// List directory contents from the indexed file tree (cache-backed, multi-path).
pub struct DirList;
impl Tool for DirList {
    fn name(&self) -> &'static str { "dir_list" }
    fn description(&self) -> &'static str {
        "List the contents of one or more workspace directories from the indexed file tree (folders end with '/'). Pass `paths` (an array) to list several directories in one call — prefer this over many separate calls."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Workspace-relative directory paths to list. Use [\".\"] for the workspace root. Prefix with [N] to target workspace N (e.g. \"[1]src\")."
                },
                "path": { "type": "string", "description": "A single directory (alternative to `paths`)." }
            }
        })
    }
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        // Accept `paths` (array) or a single `path`; default to the workspace root.
        let mut dirs: Vec<String> = Vec::new();
        if let Some(arr) = args.get("paths").and_then(Value::as_array) {
            for v in arr {
                if let Some(s) = v.as_str() { dirs.push(s.to_string()); }
            }
        }
        if let Some(s) = args.get("path").and_then(Value::as_str) {
            dirs.push(s.to_string());
        }
        if dirs.is_empty() { dirs.push(".".to_string()); }

        const MAX_ENTRIES: usize = 500;

        let cache = ctx.dir_cache.read().map_err(|_| anyhow::anyhow!("dir cache unavailable"))?;
        let mut out = String::new();
        let mut total_entries = 0usize;
        let mut truncated = false;
        'outer: for dir in &dirs {
            // sandbox the path even though we read from the cache, to reject escapes
            let _ = resolve(&ctx.workspaces, dir)?;
            let (ws_idx, bare) = super::parse_ws_prefix(dir);
            let children = cache.children(bare, ws_idx);
            let label = if bare.is_empty() || bare == "." { if ws_idx == 0 { ".".to_string() } else { format!("[{ws_idx}].") } } else { dir.to_string() };
            out.push_str(&format!("{label}:\n"));
            if children.is_empty() {
                out.push_str("  (empty or not indexed)\n");
            } else {
                for c in &children {
                    if total_entries >= MAX_ENTRIES {
                        truncated = true;
                        break 'outer;
                    }
                    out.push_str("  ");
                    out.push_str(c);
                    out.push('\n');
                    total_entries += 1;
                }
            }
            out.push('\n');
        }
        if truncated {
            out.push_str("... (truncated at 500 entries; use a more specific path)");
        }
        Ok(out.trim_end().to_string())
    }
}

/// Read a workspace-relative file, returning line-numbered content.
pub struct Read;
impl Tool for Read {
    fn name(&self) -> &'static str { "read" }
    fn description(&self) -> &'static str {
        "Read a workspace-relative file. Returns line-numbered content. Use offset/limit to paginate large files."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path. When multiple workspace roots are active, file listings prefix paths with [N] (e.g. \"[1]src/main.rs\") — copy that prefix exactly. A bare path with no prefix targets workspace [0]." },
                "offset": {
                    "type": "integer",
                    "description": "0-based line to start from (default 0)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max lines to read (default 2000, capped at 2000)."
                }
            },
            "required": ["path"]
        })
    }
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let rel = arg_str(args, "path")?;
        let path = resolve_read(&ctx.workspaces, rel)?;
        if path.is_dir() {
            bail!("'{rel}' is a directory, not a file");
        }
        if !path.exists() {
            bail!("{}", not_found_help(ctx, &path, rel));
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading file '{rel}'"))?;

        const MAX_LINES: usize = 2000;
        const MAX_BYTES: usize = 50 * 1024;

        // Parse optional offset/limit; clamp limit to the hard cap.
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args.get("limit")
            .and_then(Value::as_u64)
            .map(|v| (v as usize).min(MAX_LINES))
            .unwrap_or(MAX_LINES);

        // Collect all lines so we know the total for the notice.
        let all_lines: Vec<&str> = content.lines().collect();
        let total_lines = all_lines.len();

        let mut out = String::new();
        let mut bytes = 0usize;
        // `last_emitted_idx` tracks the 0-based index of the last line emitted.
        let mut last_emitted_idx: Option<usize> = None;
        let mut byte_truncated = false;

        for (idx, line) in all_lines.iter().enumerate().skip(offset) {
            if idx >= offset + limit {
                break;
            }
            // 1-indexed line numbers, right-aligned in a 6-wide field.
            let rendered = format!("{:>6}\t{}\n", idx + 1, line);
            if bytes + rendered.len() > MAX_BYTES && last_emitted_idx.is_some() {
                byte_truncated = true;
                break;
            }
            bytes += rendered.len();
            out.push_str(&rendered);
            last_emitted_idx = Some(idx);
        }

        // Determine whether we reached the end of the file.
        // `showed_through` is the 1-based line number of the last line shown.
        let showed_through = last_emitted_idx.map(|i| i + 1).unwrap_or(offset);
        // `next_offset` is what the caller should pass as offset to continue.
        let next_offset = showed_through;
        let reached_end = !byte_truncated && (offset + limit >= total_lines);

        if !reached_end || offset > 0 {
            // Only add the notice when content was cut or we started mid-file.
            let start_line = offset + 1; // 1-based
            out.push_str(&format!(
                "\n[truncated: showing lines {start_line}-{showed_through} of {total_lines}. Use read with offset={next_offset} to continue.]"
            ));
        }
        Ok(out)
    }
}

/// Create or overwrite a workspace-relative file.
pub struct Write;
impl Tool for Write {
    fn name(&self) -> &'static str { "write" }
    fn description(&self) -> &'static str {
        "Create or overwrite a workspace-relative file with the given content."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path. When multiple workspace roots are active, file listings prefix paths with [N] (e.g. \"[1]src/main.rs\") — copy that prefix exactly. A bare path with no prefix targets workspace [0]." },
                "content": { "type": "string", "description": "Full file content to write" }
            },
            "required": ["path", "content"]
        })
    }
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let rel = arg_str(args, "path")?;
        let content = arg_str(args, "content")?;
        let path = resolve(&ctx.workspaces, rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directories for '{rel}'"))?;
        }
        std::fs::write(&path, content.as_bytes())
            .with_context(|| format!("writing file '{rel}'"))?;
        super::dircache::reindex(ctx.workspaces.clone(), ctx.dir_cache.clone());
        Ok(format!("Wrote {} bytes to {}.", content.len(), rel))
    }
}

/// Replace an exact string in a file in place.
pub struct Edit;
impl Tool for Edit {
    fn name(&self) -> &'static str { "edit" }
    fn description(&self) -> &'static str {
        "Replace an exact string in a file in place. Read the file first to get the exact text. Fails if the old string is missing or not unique."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path. When multiple workspace roots are active, file listings prefix paths with [N] (e.g. \"[1]src/main.rs\") — copy that prefix exactly. A bare path with no prefix targets workspace [0]." },
                "old": { "type": "string", "description": "Exact substring to replace" },
                "new": { "type": "string", "description": "Replacement text" },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence (default false — requires unique match)"
                }
            },
            "required": ["path", "old", "new"]
        })
    }
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let rel = arg_str(args, "path")?;
        let old = arg_str(args, "old")?;
        let new = arg_str(args, "new")?;
        let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);

        let path = resolve(&ctx.workspaces, rel)?;
        if path.is_dir() {
            bail!("'{rel}' is a directory, not a file");
        }
        if !path.exists() {
            bail!("{}", not_found_help(ctx, &path, rel));
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading file '{rel}'"))?;

        let count = content.matches(old).count();
        if count == 0 {
            return Ok(format!(
                "string not found in {rel}; read the file and copy the exact text"
            ));
        }
        if count > 1 && !replace_all {
            return Ok(format!(
                "the old string appears {count} times in {rel}; add surrounding context to make it unique, or set replace_all=true"
            ));
        }

        let replaced = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };

        std::fs::write(&path, replaced.as_bytes())
            .with_context(|| format!("writing file '{rel}'"))?;
        super::dircache::reindex(ctx.workspaces.clone(), ctx.dir_cache.clone());

        let n = if replace_all { count } else { 1 };
        Ok(format!("edited {rel} ({n} replacement(s))"))
    }
}

/// Delete a workspace-relative file.
pub struct Delete;
impl Tool for Delete {
    fn name(&self) -> &'static str { "delete" }
    fn description(&self) -> &'static str {
        "Delete a workspace-relative file."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path. When multiple workspace roots are active, file listings prefix paths with [N] (e.g. \"[1]src/main.rs\") — copy that prefix exactly. A bare path with no prefix targets workspace [0]." }
            },
            "required": ["path"]
        })
    }
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let rel = arg_str(args, "path")?;
        let path = resolve(&ctx.workspaces, rel)?;
        if path.is_dir() {
            bail!("'{rel}' is a directory, not a file");
        }
        if !path.exists() {
            bail!("{}", not_found_help(ctx, &path, rel));
        }
        std::fs::remove_file(&path).with_context(|| format!("deleting file '{rel}'"))?;
        super::dircache::reindex(ctx.workspaces.clone(), ctx.dir_cache.clone());
        Ok(format!("Deleted {rel}."))
    }
}
