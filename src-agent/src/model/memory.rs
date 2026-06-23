use std::path::Path;

/// Reads <session_dir>/memory/MEMORY.md if present and non-empty (trimmed).
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
