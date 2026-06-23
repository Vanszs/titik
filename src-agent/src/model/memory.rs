//! Per-session long-term memory loaded from `MEMORY.md`.
//!
//! Each session directory contains an optional `memory/MEMORY.md` file that
//! contributors can use to inject persistent context into the system prompt
//! (e.g. project notes, user preferences, ongoing task state). The file is
//! re-read on every `Session::rebuild_system` call, so edits take effect
//! without restarting the TUI.
//!
//! File layout:
//! ```text
//! ~/.simple-coder/sessions/<id>/
//!     memory/
//!         MEMORY.md   ← loaded here
//!     messages.json
//!     settings.json
//! ```

use std::path::Path;

/// Read `<session_dir>/memory/MEMORY.md` and return its trimmed contents.
///
/// Returns `None` if the file does not exist, cannot be read, or is blank
/// after trimming — the caller (`Session::rebuild_system`) treats `None` as
/// "no extra memory context" and omits the section from the system prompt.
pub fn load_memory(session_dir: &Path) -> Option<String> {
    let p = session_dir.join("memory").join("MEMORY.md");
    let s = std::fs::read_to_string(&p).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}
