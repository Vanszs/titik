//! Controller – `/slash` command parser.
//!
//! When the user types a line starting with `/` in Chat mode,
//! [`controller::input`] calls [`parse`] to turn the raw text into a
//! [`Command`] value.  The runtime then routes that value to the appropriate
//! service logic (compaction, new session, rename, etc.).
//!
//! Supported commands: `/compact`, `/new`, `/rename [session] <name>`,
//! `/help`, `/quit` (aliases: `/q`, `/exit`).

/// A parsed in-chat slash command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Compact the conversation history to save context window space.
    Compact,
    /// Start a fresh session (discards current chat).
    New,
    /// Rename the current session.  Holds the new name string.
    Rename(String),
    /// Print available commands to the chat view.
    Help,
    /// Exit the application.
    Quit,
    /// An unrecognised command verb; holds the raw verb for display.
    Unknown(String),
}

/// Parse a slash-command from `line`.
///
/// `line` is the raw user input — already known to start with `/`.
/// The verb is matched case-insensitively; the remainder preserves original
/// casing so that session names are not lowercased.
///
/// `/rename session <name>` and `/rename <name>` are both accepted: the
/// optional literal word `"session"` is stripped from the remainder before
/// the name is extracted.
pub fn parse(line: &str) -> Command {
    let trimmed = line.trim();
    let without = trimmed.strip_prefix('/').unwrap_or(trimmed);

    // Split off the verb (first whitespace-delimited token).
    let head = without.split_whitespace().next().unwrap_or("").to_string();
    let head_lc = head.to_lowercase();

    // `rest` is sliced from the original-cased `without` so that everything
    // after the verb keeps its capitalisation (important for session names).
    let rest = without[head.len()..].trim_start();

    match head_lc.as_str() {
        "compact" => Command::Compact,
        "new" => Command::New,
        "help" => Command::Help,
        "quit" | "q" | "exit" => Command::Quit,
        "rename" => {
            // Accept "/rename session <name>" as well as "/rename <name>".
            // Strip the optional literal "session" prefix from the remainder.
            let name = rest.strip_prefix("session").map(str::trim).unwrap_or(rest);
            Command::Rename(name.trim().to_string())
        }
        other => Command::Unknown(other.to_string()),
    }
}
