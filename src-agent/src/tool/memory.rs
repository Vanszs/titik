//! Long-term memory tool: append, update, or remove facts in `MEMORY.md` so
//! they persist across sessions and are re-injected into the system prompt on
//! every rebuild.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use super::{Tool, ToolCtx};

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

/// Appends, updates, or removes a fact in `<session_dir>/memory/MEMORY.md`.
pub struct Remember;

impl Tool for Remember {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn description(&self) -> &'static str {
        "Save, update, or remove a memory. \
         To ADD a memory, provide `content`. \
         To UPDATE or REMOVE existing memory, provide `old` (the exact text currently in memory to find): \
         it is replaced with `content`, or removed when `content` is empty."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The memory text to save or the replacement text. Leave empty when removing."
                },
                "old": {
                    "type": "string",
                    "description": "Exact text currently in MEMORY.md to find. When provided, the first occurrence is replaced with `content` (or removed if `content` is empty)."
                }
            },
            "required": ["content"]
        })
    }

    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let content = arg_str(args, "content")?;
        let old = opt_str(args, "old").filter(|s| !s.is_empty());

        let memory_dir = match ctx.memory_dir.as_ref() {
            Some(d) => d,
            None => bail!("no active session to save memory to"),
        };

        std::fs::create_dir_all(memory_dir)
            .with_context(|| format!("creating memory directory '{}'", memory_dir.display()))?;

        let memory_file = memory_dir.join("MEMORY.md");

        if let Some(old_text) = old {
            // Edit / remove mode: read, find, replace or delete, write back.
            let existing = if memory_file.exists() {
                std::fs::read_to_string(&memory_file)
                    .with_context(|| format!("reading '{}'", memory_file.display()))?
            } else {
                String::new()
            };

            if !existing.contains(old_text) {
                return Ok(format!("not found in memory: \"{}\"", old_text));
            }

            let updated = if content.trim().is_empty() {
                // Remove mode: delete the first occurrence of old_text.
                // If it was an entire line (with optional trailing newline), also
                // remove the resulting blank line so MEMORY.md stays tidy.
                let replaced = existing.replacen(old_text, "", 1);
                // Collapse any double blank lines that may have appeared.
                replaced.replace("\n\n\n", "\n\n")
            } else {
                // Replace mode: swap old_text for the trimmed new content.
                existing.replacen(old_text, content.trim(), 1)
            };

            std::fs::write(&memory_file, updated.as_bytes())
                .with_context(|| format!("writing '{}'", memory_file.display()))?;

            if content.trim().is_empty() {
                Ok("Removed from memory".to_string())
            } else {
                Ok("Updated memory".to_string())
            }
        } else {
            // Append mode (original behavior, byte-for-byte compatible).
            // No '# Memory' heading is written here — build_system_prompt already
            // adds that heading when it injects MEMORY.md into the system prompt.
            use std::io::Write as IoWrite;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&memory_file)
                .with_context(|| format!("opening '{}' for append", memory_file.display()))?;

            writeln!(file, "- {}", content.trim())
                .with_context(|| format!("writing to '{}'", memory_file.display()))?;

            Ok(format!("Saved to memory: {}", content.trim()))
        }
    }
}
