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

/// List the immediate entries of a workspace-relative directory.
pub struct DirList;
impl Tool for DirList {
    fn name(&self) -> &'static str { "dir_list" }
    fn description(&self) -> &'static str {
        "List files and directories directly under a workspace-relative path. Cannot access paths outside the workspace."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative directory path (defaults to \".\")"
                }
            }
        })
    }
    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let rel = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let dir = resolve(&ctx.workspace, rel)?;
        if !dir.is_dir() {
            bail!("'{rel}' is not a directory");
        }
        let mut entries: Vec<String> = Vec::new();
        for ent in std::fs::read_dir(&dir).with_context(|| format!("reading directory '{rel}'"))? {
            let ent = ent?;
            let mut name = ent.file_name().to_string_lossy().into_owned();
            // Append `/` to directory names. Use the dir-entry file type, which
            // does not follow symlinks; fall back to a stat when unavailable.
            let is_dir = match ent.file_type() {
                Ok(t) => t.is_dir(),
                Err(_) => ent.path().is_dir(),
            };
            if is_dir {
                name.push('/');
            }
            entries.push(name);
        }
        entries.sort();
        Ok(entries.join("\n"))
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
        Ok(format!("Wrote {} bytes to {}.", content.len(), rel))
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
        Ok(format!("Deleted {rel}."))
    }
}
