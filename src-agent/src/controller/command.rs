#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Compact,
    New,
    Rename(String),
    Help,
    Quit,
    Unknown(String),
}

/// `line` is the raw input (already known to start with '/').
pub fn parse(line: &str) -> Command {
    let trimmed = line.trim();
    let without = trimmed.strip_prefix('/').unwrap_or(trimmed);
    let head = without.split_whitespace().next().unwrap_or("").to_string();
    let head_lc = head.to_lowercase();
    // rest is sliced from the original-cased `without` (preserve case for names)
    let rest = without[head.len()..].trim_start();
    match head_lc.as_str() {
        "compact" => Command::Compact,
        "new" => Command::New,
        "help" => Command::Help,
        "quit" | "q" | "exit" => Command::Quit,
        "rename" => {
            // tolerate "/rename session <name>" and "/rename <name>"
            let name = rest.strip_prefix("session").map(str::trim).unwrap_or(rest);
            Command::Rename(name.trim().to_string())
        }
        other => Command::Unknown(other.to_string()),
    }
}
