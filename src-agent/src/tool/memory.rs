//! Long-term memory tool: `remember` saves a memory into the per-PROJECT index
//! store so it persists across every session opened from the same working dir.
//!
//! The store is an index of pointers plus one file per memory (see
//! [`crate::model::memory`]): `remember` writes `<slug>.md` (frontmatter + body)
//! and refreshes `MEMORY.md` (the index). Only the index is injected into the
//! system prompt on rebuild; the model pulls a body on demand via `recall`.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use super::{Tool, ToolCtx};
use crate::model::memory::{slugify, write_memory};

/// Pull a required string argument out of the decoded JSON args.
fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing required string argument '{key}'"))
}

/// Pull an optional string argument out of the decoded JSON args.
fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Saves (creates or updates) a memory in the per-project index store.
pub struct Remember;

impl Tool for Remember {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn description(&self) -> &'static str {
        "Save a long-term memory for this project (shared across sessions). \
         Provide `description` (a one-line hook shown in the memory index) and \
         `content` (the full memory body). Optionally set `type` and a `slug` id; \
         if `slug` is omitted one is derived from the description. Saving to an \
         existing slug overwrites it (use this to edit/swap a memory). Returns the \
         slug; use `recall(slug)` later to read the full body, `forget(slug)` to \
         delete it."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "One-line hook for this memory, shown in the injected memory index."
                },
                "content": {
                    "type": "string",
                    "description": "The full memory body to store."
                },
                "type": {
                    "type": "string",
                    "description": "Optional category: project | preference | reference | fact (default: fact)."
                },
                "slug": {
                    "type": "string",
                    "description": "Optional id/filename for the memory. Omit to derive one from the description. Reusing an existing slug overwrites that memory."
                }
            },
            "required": ["description", "content"]
        })
    }

    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let description = arg_str(args, "description")?;
        let content = arg_str(args, "content")?;
        let kind = opt_str(args, "type").unwrap_or("");
        // An explicit slug takes precedence; otherwise derive it from the
        // description. Either way `write_memory` re-sanitizes it (defence in
        // depth) before it ever becomes a filename.
        let slug_seed = match opt_str(args, "slug").filter(|s| !s.trim().is_empty()) {
            Some(s) => s.to_string(),
            None => slugify(description)
                .with_context(|| "could not derive a slug from the description")?,
        };

        let memory_dir = match ctx.memory_dir.as_ref() {
            Some(d) => d,
            None => bail!("no active session to save memory to"),
        };

        let slug = write_memory(memory_dir, &slug_seed, description, kind, content)
            .with_context(|| format!("saving memory to '{}'", memory_dir.display()))?;

        Ok(format!("Saved memory '{slug}'"))
    }
}
