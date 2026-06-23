//! Sandboxed filesystem tools: list / read / write / delete.
//!
//! Every path argument is resolved through [`super::resolve`], which pins it
//! inside the session workspace — a tool can never read or write outside it.
//! These structs implement [`Tool`] and are advertised to the model via
//! [`super::all_tools`]; the agentic loop dispatches the model's requested calls
//! through [`Tool::run`].

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use super::{resolve, Tool, ToolCtx};

/// Pull a required string argument out of the decoded JSON args.
fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing required string argument '{key}'"))
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
                    "description": "Workspace-relative directory paths to list. Use [\".\"] for the workspace root."
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

        let cache = ctx.dir_cache.read().map_err(|_| anyhow::anyhow!("dir cache unavailable"))?;
        let mut out = String::new();
        for dir in &dirs {
            // sandbox the path even though we read from the cache, to reject escapes
            let _ = resolve(&ctx.workspace, dir)?;
            let children = cache.children(dir);
            let label = if dir.is_empty() || dir == "." { "." } else { dir.as_str() };
            out.push_str(&format!("{label}:\n"));
            if children.is_empty() {
                out.push_str("  (empty or not indexed)\n");
            } else {
                for c in &children {
                    out.push_str("  ");
                    out.push_str(c);
                    out.push('\n');
                }
            }
            out.push('\n');
        }
        Ok(out.trim_end().to_string())
    }
}

/// Read a workspace-relative file, returning line-numbered content.
pub struct Read;
impl Tool for Read {
    fn name(&self) -> &'static str { "read" }
    fn description(&self) -> &'static str {
        "Read a workspace-relative file. Returns line-numbered content."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path" }
            },
            "required": ["path"]
        })
    }
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let rel = arg_str(args, "path")?;
        let path = resolve(&ctx.workspace, rel)?;
        if path.is_dir() {
            bail!("'{rel}' is a directory, not a file");
        }
        if !path.exists() {
            bail!("'{rel}' does not exist");
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading file '{rel}'"))?;

        const MAX_LINES: usize = 2000;
        const MAX_BYTES: usize = 50 * 1024;

        let mut out = String::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        // The loop only ever emits or breaks, so `i` equals the number of lines
        // already emitted at the top of each iteration.
        for (i, line) in content.lines().enumerate() {
            if i >= MAX_LINES {
                truncated = true;
                break;
            }
            // 1-indexed line numbers, right-aligned in a 6-wide field.
            let rendered = format!("{:>6}\t{}\n", i + 1, line);
            if bytes + rendered.len() > MAX_BYTES && i > 0 {
                truncated = true;
                break;
            }
            bytes += rendered.len();
            out.push_str(&rendered);
        }
        if truncated {
            out.push_str("… (output truncated)\n");
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
                "path": { "type": "string", "description": "Workspace-relative file path" },
                "content": { "type": "string", "description": "Full file content to write" }
            },
            "required": ["path", "content"]
        })
    }
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let rel = arg_str(args, "path")?;
        let content = arg_str(args, "content")?;
        let path = resolve(&ctx.workspace, rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directories for '{rel}'"))?;
        }
        std::fs::write(&path, content.as_bytes())
            .with_context(|| format!("writing file '{rel}'"))?;
        super::dircache::reindex(ctx.workspace.clone(), ctx.dir_cache.clone());
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
                "path": { "type": "string", "description": "Workspace-relative file path" },
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

        let path = resolve(&ctx.workspace, rel)?;
        if path.is_dir() {
            bail!("'{rel}' is a directory, not a file");
        }
        if !path.exists() {
            bail!("'{rel}' does not exist");
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
        super::dircache::reindex(ctx.workspace.clone(), ctx.dir_cache.clone());

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
                "path": { "type": "string", "description": "Workspace-relative file path" }
            },
            "required": ["path"]
        })
    }
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let rel = arg_str(args, "path")?;
        let path = resolve(&ctx.workspace, rel)?;
        if path.is_dir() {
            bail!("'{rel}' is a directory, not a file");
        }
        if !path.exists() {
            bail!("'{rel}' does not exist");
        }
        std::fs::remove_file(&path).with_context(|| format!("deleting file '{rel}'"))?;
        super::dircache::reindex(ctx.workspace.clone(), ctx.dir_cache.clone());
        Ok(format!("Deleted {rel}."))
    }
}
