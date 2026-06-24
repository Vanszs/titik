//! Long-term memory tool: append a fact to `MEMORY.md` so it persists across
//! sessions and is re-injected into the system prompt on every rebuild.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use super::{Tool, ToolCtx};

/// Pull a required string argument out of the decoded JSON args.
fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing required string argument '{key}'"))
}

/// Appends a bullet-point fact to `<session_dir>/memory/MEMORY.md`.
pub struct Remember;

impl Tool for Remember {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn description(&self) -> &'static str {
        "Save an important fact, decision, or user preference to long-term memory. \
         It persists across sessions and appears in your '# Memory' section next time. \
         Use this when the user asks you to remember something."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The fact to remember, stated concisely as a standalone sentence."
                }
            },
            "required": ["content"]
        })
    }

    fn run(&self, ctx: &ToolCtx, args: &Value) -> Result<String> {
        let content = arg_str(args, "content")?;
        let memory_dir = match ctx.memory_dir.as_ref() {
            Some(d) => d,
            None => bail!("no active session to save memory to"),
        };

        std::fs::create_dir_all(memory_dir)
            .with_context(|| format!("creating memory directory '{}'", memory_dir.display()))?;

        let memory_file = memory_dir.join("MEMORY.md");

        // Open with create + append so that a brand-new file just starts with
        // the bullet, and an existing file gets the entry added at the end.
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
