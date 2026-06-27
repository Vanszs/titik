//! Core append and query operations on the `messages` table.

use std::path::Path;

use anyhow::Result;

use crate::dto::chat::Role;

use super::blobs::classify_blob;
use super::schema::{now_secs, open, role_str};

/// One archived message row, role + content, as stored in `messages`.
/// Used by [`fetch_messages_since`] for replaying history (P2/P3).
#[allow(dead_code)] // consumed by later phases (short-send summary/router)
#[derive(Debug, Clone)]
pub struct ArchivedMsg {
    pub id: i64,
    pub role: String,
    pub content: String,
}

/// Append one message to the session's SQLite log, creating the DB + tables on
/// first use, and return the inserted `messages.id`. `session_dir` is the
/// session directory (where messages.json lives). `usage` is
/// `(prompt_tokens, completion_tokens, cost)` for assistant messages, or `None`
/// (stored as zeros) for user messages. Best-effort — callers ignore the
/// result.
///
/// The message insert and the heavy-blob index run in one transaction: after
/// the row is written, [`classify_blob`] decides whether to record a `blobs`
/// row keyed by the new `msg_id`. The blob insert is `INSERT OR IGNORE`, so
/// re-indexing the same message id is a no-op. The `messages` table is only
/// ever inserted into — never updated or deleted.
pub fn append(
    session_dir: &Path,
    role: Role,
    content: &str,
    usage: Option<(u64, u64, f64)>,
) -> Result<i64> {
    let mut conn = open(session_dir)?;
    let created_at = now_secs();
    let (pt, ct, cost) = usage.unwrap_or((0, 0, 0.0));

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO messages
            (role, content, created_at, prompt_tokens, completion_tokens, cost)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![role_str(role), content, created_at, pt, ct, cost],
    )?;
    let msg_id = tx.last_insert_rowid();

    // Index heavy content in the SAME transaction so the blob can't be lost if
    // the process dies between the two writes. Idempotent via INSERT OR IGNORE
    // on the UNIQUE msg_id.
    if let Some((kind, token_est, snippet)) = classify_blob(role, content) {
        tx.execute(
            "INSERT OR IGNORE INTO blobs
                (id, msg_id, kind, token_est, snippet, created_at)
             VALUES (?1, ?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![msg_id, kind, token_est, snippet, created_at],
        )?;
    }

    tx.commit()?;
    Ok(msg_id)
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
    let conn = open(session_dir)?;
    let row: (i64, i64, f64) = conn.query_row(
        "SELECT
            COALESCE((SELECT prompt_tokens FROM messages WHERE role = 'assistant' ORDER BY id DESC LIMIT 1), 0),
            COALESCE(SUM(completion_tokens), 0),
            COALESCE(SUM(cost), 0)
         FROM messages",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok((row.0 as u64, row.1 as u64, row.2))
}

/// Fetch up to `limit` archived messages with `id > after_id`, ordered by id
/// ascending. Returns an empty vec if the DB is absent/unreadable. Best-effort.
#[allow(dead_code)] // consumed by later phases (short-send summary/router)
pub fn fetch_messages_since(session_dir: &Path, after_id: i64, limit: i64) -> Vec<ArchivedMsg> {
    fn inner(session_dir: &Path, after_id: i64, limit: i64) -> Result<Vec<ArchivedMsg>> {
        let conn = open(session_dir)?;
        let mut stmt = conn.prepare(
            "SELECT id, role, content FROM messages
             WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![after_id, limit], |r| {
            Ok(ArchivedMsg {
                id: r.get(0)?,
                role: r.get(1)?,
                content: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
    inner(session_dir, after_id, limit).unwrap_or_default()
}

/// Sorted ascending ids of all `user`-role messages in the archive. Used to
/// snap the summary fold boundary to a completed-exchange edge. Returns an empty
/// vec if the DB is absent/unreadable. Best-effort.
pub fn user_message_ids(session_dir: &Path) -> Vec<i64> {
    fn inner(session_dir: &Path) -> Result<Vec<i64>> {
        let conn = open(session_dir)?;
        // `role` is stored lowercase by `role_str` (Role::User => "user").
        let mut stmt =
            conn.prepare("SELECT id FROM messages WHERE role = 'user' ORDER BY id ASC")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
    inner(session_dir).unwrap_or_default()
}

/// Highest `messages.id` in the archive, or 0 when empty / unreadable.
/// Best-effort.
#[allow(dead_code)] // consumed by later phases (short-send summary/router)
pub fn max_message_id(session_dir: &Path) -> i64 {
    fn inner(session_dir: &Path) -> Result<i64> {
        let conn = open(session_dir)?;
        let id: i64 = conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM messages",
            [],
            |r| r.get(0),
        )?;
        Ok(id)
    }
    inner(session_dir).unwrap_or(0)
}
