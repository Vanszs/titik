//! Static resource embedding — system-prompt files baked into the binary.
//!
//! At compile time, [`include_dir!`] embeds the entire `src-misc/` directory
//! next to the crate root.  At runtime, [`system_prompt`], [`system_personality`],
//! and [`system_tools`] read from that in-memory directory.
//!
//! Expected files in `src-misc/`:
//! - `system-prompt.txt`      — the primary system instruction block
//! - `system-personality.txt` — tone/style addendum appended after the prompt
//! - `system-tools.txt`       — tool-usage guidance appended at the very bottom
//!
//! All files are optional: if absent or empty, hard-coded fallback strings
//! are used so the binary is always usable out of the box.
//!
//! [`build_system_prompt`] assembles the final string sent as the `system`
//! role message to the OpenRouter API:
//! ```text
//! <prompt>
//!
//! <personality>
//!
//! # Project Instructions
//! <agents>       ← only present when AGENT.md / AGENTS.md exists in workdir
//!
//! # Memory
//! <memory>       ← only present when the session has saved memory
//!
//! <tools>        ← only present when system-tools.txt is non-empty
//! ```

use include_dir::{include_dir, Dir};

/// The `src-misc/` directory, embedded at compile time.
static MISC: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../src-misc");

const FALLBACK_SYSTEM: &str = "You are a precise, concise coding assistant.";
const FALLBACK_PERSONALITY: &str = "Be direct. No filler. No emoji.";

/// Return the system prompt text from `src-misc/system-prompt.txt`.
///
/// Falls back to [`FALLBACK_SYSTEM`] if the file is missing, not valid UTF-8,
/// or entirely whitespace.
pub fn system_prompt() -> &'static str {
    MISC.get_file("system-prompt.txt")
        .and_then(|f| f.contents_utf8())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(FALLBACK_SYSTEM)
}

/// Return the personality/tone addendum from `src-misc/system-personality.txt`.
///
/// Falls back to [`FALLBACK_PERSONALITY`] on missing / empty file.
pub fn system_personality() -> &'static str {
    MISC.get_file("system-personality.txt")
        .and_then(|f| f.contents_utf8())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(FALLBACK_PERSONALITY)
}

/// Return the tool-usage guidance from `src-misc/system-tools.txt`.
///
/// Appended at the bottom of the system prompt so the model knows how to use
/// the filesystem tools. Empty string if the file is missing/blank.
pub fn system_tools() -> &'static str {
    MISC.get_file("system-tools.txt")
        .and_then(|f| f.contents_utf8())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("")
}

/// Assemble the full system prompt string for the OpenRouter `system` field.
///
/// Structure: `prompt + "\n\n" + personality [+ "\n\n# Project Instructions\n" + agents]
/// [+ "\n\n# Memory\n" + memory]`.
/// Both optional sections are omitted when their argument is `None` or blank.
pub fn build_system_prompt(memory: Option<&str>, agents: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(system_prompt());
    s.push_str("\n\n");
    s.push_str(system_personality());
    if let Some(ag) = agents {
        let ag = ag.trim();
        if !ag.is_empty() {
            // Project-level instructions from AGENT.md / AGENTS.md in the workdir.
            s.push_str("\n\n# Project Instructions\n");
            s.push_str(ag);
        }
    }
    if let Some(mem) = memory {
        let mem = mem.trim();
        if !mem.is_empty() {
            // Append a named markdown section so the model can distinguish
            // memory content from the base instructions.
            s.push_str("\n\n# Memory\n");
            s.push_str(mem);
        }
    }
    let tools = system_tools();
    if !tools.is_empty() {
        s.push_str("\n\n");
        s.push_str(tools);
    }
    s
}
