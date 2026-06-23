use include_dir::{include_dir, Dir};

static MISC: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../src-misc");

const FALLBACK_SYSTEM: &str = "You are a precise, concise coding assistant.";
const FALLBACK_PERSONALITY: &str = "Be direct. No filler. No emoji.";

pub fn system_prompt() -> &'static str {
    MISC.get_file("system-prompt.txt")
        .and_then(|f| f.contents_utf8())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(FALLBACK_SYSTEM)
}

pub fn system_personality() -> &'static str {
    MISC.get_file("system-personality.txt")
        .and_then(|f| f.contents_utf8())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(FALLBACK_PERSONALITY)
}

/// prompt + "\n\n" + personality + optional "\n\n# Memory\n" + memory
pub fn build_system_prompt(memory: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(system_prompt());
    s.push_str("\n\n");
    s.push_str(system_personality());
    if let Some(mem) = memory {
        let mem = mem.trim();
        if !mem.is_empty() {
            s.push_str("\n\n# Memory\n");
            s.push_str(mem);
        }
    }
    s
}
