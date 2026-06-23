//! Per-session append-only SQLite log of every chat message.
//!
//! Lives at `<session-dir>/messages.sqlite`, separate from the working
//! `messages.json` (which `/compact` rewrites/truncates). This is the FULL
//! history — every user and assistant message with a unix-seconds timestamp,
//! captured at append time and never compacted, so it can be searched later.
//!
//! Writes are best-effort: callers ignore the error so a DB hiccup never
//! interrupts the chat.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::Connection;

use crate::dto::chat::Role;

/// Canonical lowercase role label stored in the DB.
fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Create the `messages` table if absent, then best-effort add the usage
/// columns so pre-usage DBs migrate forward. The CREATE includes the usage
/// columns for fresh DBs; the ALTERs cover existing DBs and intentionally
/// ignore the "duplicate column" error they raise once the columns exist.
fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS messages (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            role              TEXT NOT NULL,
            content           TEXT NOT NULL,
            created_at        INTEGER NOT NULL,
            prompt_tokens     INTEGER NOT NULL DEFAULT 0,
            completion_tokens INTEGER NOT NULL DEFAULT 0,
            cost              REAL NOT NULL DEFAULT 0
        );",
    )?;
    // Migrate older DBs (created before the usage columns existed). Errors here
    // are expected once the columns are present, so they're discarded.
    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN prompt_tokens INTEGER NOT NULL DEFAULT 0",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN completion_tokens INTEGER NOT NULL DEFAULT 0",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN cost REAL NOT NULL DEFAULT 0",
        [],
    );
    Ok(())
}

/// Append one message to the session's SQLite log, creating the DB + table on
/// first use. `session_dir` is the session directory (where messages.json
/// lives). `usage` is `(prompt_tokens, completion_tokens, cost)` for assistant
/// messages, or `None` (stored as zeros) for user messages. Best-effort —
/// callers ignore the error.
pub fn append(
    session_dir: &Path,
    role: Role,
    content: &str,
    usage: Option<(u64, u64, f64)>,
) -> Result<()> {
    let conn = Connection::open(session_dir.join("messages.sqlite"))?;
    ensure_schema(&conn)?;
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (pt, ct, cost) = usage.unwrap_or((0, 0, 0.0));
    conn.execute(
        "INSERT INTO messages
            (role, content, created_at, prompt_tokens, completion_tokens, cost)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![role_str(role), content, created_at, pt, ct, cost],
    )?;
    Ok(())
}

/// Return `(current_context_tokens, output_tokens, cost)` for the session.
///
/// - First value: the most recent assistant request's `prompt_tokens` (current
///   context size). OpenRouter reports the whole context window on each request,
///   so the latest row is the right number — summing would balloon across turns.
/// - Second value: cumulative `completion_tokens` (each turn adds new output).
/// - Third value: cumulative cost (each turn adds new spend).
///
/// Returns `(0, 0, 0.0)` if the DB is absent/unreadable (handled by the caller
/// via `unwrap_or`). A never-written session has no DB yet — `Connection::open`
/// creates an empty file, so `ensure_schema` is run first to make the query
/// valid against a clean schema.
pub fn totals(session_dir: &Path) -> Result<(u64, u64, f64)> {
    let conn = Connection::open(session_dir.join("messages.sqlite"))?;
    ensure_schema(&conn)?;
    let row: (i64, i64, f64) = conn.query_row(
        "SELECT
            COALESCE((SELECT prompt_tokens FROM messages WHERE prompt_tokens > 0 ORDER BY id DESC LIMIT 1), 0),
            COALESCE(SUM(completion_tokens), 0),
            COALESCE(SUM(cost), 0)
         FROM messages",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok((row.0 as u64, row.1 as u64, row.2))
}
