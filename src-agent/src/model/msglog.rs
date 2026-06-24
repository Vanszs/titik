//! Per-session append-only SQLite log of every chat message.
//!
//! Lives at `<session-dir>/messages.sqlite`, separate from the working
//! `messages.json` (which `/compact` rewrites/truncates). This is the FULL
//! history — every user and assistant message with a unix-seconds timestamp,
//! captured at append time and never compacted, so it can be searched later.
//!
//! Writes are best-effort: callers ignore the error so a DB hiccup never
//! interrupts the chat.
//!
//! ## "Short-send" storage (Phase 1)
//!
//! Beyond the append-only `messages` table this archive also carries two
//! side tables that nothing reads yet (filled here, consumed by later phases):
//!
//! - `blobs` — one row per "heavy" message (long text, a code fence, or a
//!   sizeable tool output). Keyed by `msg_id` (UNIQUE), so re-indexing the same
//!   message is idempotent. Stores a cheap token estimate + a short snippet so a
//!   summary can *reference* the bulky content without re-sending it.
//! - `summary` — a single row (id = 1) holding a rolling summary of the
//!   archived history plus the id-range it covers / the live-send start id.
//!
//! Indexing happens inside `append`'s transaction (append + classify in one
//! commit). It only ever *inserts*; the `messages` table is append-only and is
//! never updated or deleted.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

use crate::dto::chat::Role;

/// A heavy message is one whose token estimate clears this bar (~1600 chars).
const HEAVY_TOKEN_EST: i64 = 400;
/// Lower bar applied only to tool outputs (they're worth indexing sooner).
const TOOL_HEAVY_TOKEN_EST: i64 = 150;
/// How many leading characters of a heavy message to keep as a preview snippet.
const SNIPPET_CHARS: usize = 120;

/// One archived message row, role + content, as stored in `messages`.
/// Used by [`fetch_messages_since`] for replaying history (P2/P3).
#[allow(dead_code)] // consumed by later phases (short-send summary/router)
#[derive(Debug, Clone)]
pub struct ArchivedMsg {
    pub id: i64,
    pub role: String,
    pub content: String,
}

/// A pointer into the `blobs` side table: enough metadata to *reference* a
/// heavy message (its kind, size estimate, and preview) without loading the
/// full content. Consumed by the summary builder (P2/P3).
#[allow(dead_code)] // consumed by later phases (short-send summary/router)
#[derive(Debug, Clone)]
pub struct BlobRef {
    pub id: i64,
    pub msg_id: i64,
    pub kind: String,
    pub token_est: i64,
    pub snippet: String,
}

/// The single rolling-summary row (id = 1). `covers_up_to` is the highest
/// message id folded into `text`; `sent_start` is the first message id that is
/// still sent live (un-summarised). Read by P2/P3.
#[allow(dead_code)] // consumed by later phases (short-send summary/router)
#[derive(Debug, Clone)]
pub struct SummaryRow {
    pub text: String,
    pub covers_up_to: i64,
    pub sent_start: i64,
}

/// Canonical lowercase role label stored in the DB.
fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Open the session's SQLite archive and run migrations. Centralises the path
/// join so every entry point hits the same file + schema.
fn open(session_dir: &Path) -> Result<Connection> {
    let conn = Connection::open(session_dir.join("messages.sqlite"))?;
    ensure_schema(&conn)?;
    Ok(conn)
}

/// Unix-seconds timestamp, or 0 if the clock is before the epoch (won't happen
/// in practice; keeps the call infallible).
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64
}

/// Create the `messages` table if absent, then best-effort add the usage
/// columns so pre-usage DBs migrate forward. The CREATE includes the usage
/// columns for fresh DBs; the ALTERs cover existing DBs and intentionally
/// ignore the "duplicate column" error they raise once the columns exist.
///
/// Also creates the Phase-1 side tables (`blobs`, `summary`). All statements
/// are `IF NOT EXISTS`, so running this against a DB that already has the
/// `messages` table (or already has the side tables) is a no-op.
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
        );

        CREATE TABLE IF NOT EXISTS blobs (
            id         INTEGER PRIMARY KEY,
            msg_id     INTEGER NOT NULL UNIQUE,
            kind       TEXT NOT NULL,
            token_est  INTEGER NOT NULL,
            snippet    TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS summary (
            id           INTEGER PRIMARY KEY CHECK(id = 1),
            text         TEXT NOT NULL,
            covers_up_to INTEGER NOT NULL,
            sent_start   INTEGER NOT NULL,
            updated_at   INTEGER NOT NULL
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

/// Decide whether `content` is a "heavy blob" worth indexing, and if so derive
/// its `(kind, token_est, snippet)`. Returns `None` for ordinary messages.
///
/// - `token_est` is an approximate token count: `chars / 4`.
/// - Heavy when the estimate clears [`HEAVY_TOKEN_EST`], OR the content carries
///   a triple-backtick code fence, OR it's a tool output past
///   [`TOOL_HEAVY_TOKEN_EST`].
/// - `kind`: `"code"` if it has a ``` fence, else `"tool_output"` for tool
///   messages, else `"large_text"`.
/// - `snippet`: the leading [`SNIPPET_CHARS`] chars with newlines collapsed to
///   spaces, trimmed — a short preview for the summary to reference.
fn classify_blob(role: Role, content: &str) -> Option<(&'static str, i64, String)> {
    let token_est = (content.chars().count() / 4) as i64;
    let has_fence = content.contains("```");
    let is_tool = matches!(role, Role::Tool);

    let heavy =
        token_est >= HEAVY_TOKEN_EST || has_fence || (is_tool && token_est >= TOOL_HEAVY_TOKEN_EST);
    if !heavy {
        return None;
    }

    let kind = if has_fence {
        "code"
    } else if is_tool {
        "tool_output"
    } else {
        "large_text"
    };

    // Collapse newlines (and stray carriage returns) to spaces, then take the
    // first SNIPPET_CHARS chars and trim. Char-based so multibyte content can't
    // split a code point.
    let collapsed: String = content
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(SNIPPET_CHARS)
        .collect();
    let snippet = collapsed.trim().to_string();

    Some((kind, token_est, snippet))
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
            COALESCE(SUM(prompt_tokens), 0),
            COALESCE(SUM(completion_tokens), 0),
            COALESCE(SUM(cost), 0)
         FROM messages",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok((row.0 as u64, row.1 as u64, row.2))
}

/// Read the single rolling-summary row, or `None` if it hasn't been written
/// yet (or the DB is unreadable — handled by the caller). Best-effort.
#[allow(dead_code)] // consumed by later phases (short-send summary/router)
pub fn read_summary(session_dir: &Path) -> Option<SummaryRow> {
    let conn = open(session_dir).ok()?;
    conn.query_row(
        "SELECT text, covers_up_to, sent_start FROM summary WHERE id = 1",
        [],
        |r| {
            Ok(SummaryRow {
                text: r.get(0)?,
                covers_up_to: r.get(1)?,
                sent_start: r.get(2)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// Upsert the single rolling-summary row (id = 1). Best-effort — callers ignore
/// the error. Overwrites the previous summary text + bookkeeping wholesale.
#[allow(dead_code)] // consumed by later phases (short-send summary/router)
pub fn write_summary(
    session_dir: &Path,
    text: &str,
    covers_up_to: i64,
    sent_start: i64,
) -> Result<()> {
    let conn = open(session_dir)?;
    conn.execute(
        "INSERT INTO summary (id, text, covers_up_to, sent_start, updated_at)
         VALUES (1, ?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
            text         = excluded.text,
            covers_up_to = excluded.covers_up_to,
            sent_start   = excluded.sent_start,
            updated_at   = excluded.updated_at",
        rusqlite::params![text, covers_up_to, sent_start, now_secs()],
    )?;
    Ok(())
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

/// Return the raw `messages.content` for a single id, or `None` if absent /
/// unreadable. Lets a summary expand a `blobs` reference back to its full text
/// on demand. Best-effort.
#[allow(dead_code)] // consumed by later phases (short-send summary/router)
pub fn fetch_blob_content(session_dir: &Path, msg_id: i64) -> Option<String> {
    let conn = open(session_dir).ok()?;
    conn.query_row(
        "SELECT content FROM messages WHERE id = ?1",
        rusqlite::params![msg_id],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// List every indexed blob reference, ordered by `msg_id` ascending. Returns an
/// empty vec if the DB is absent/unreadable. Best-effort.
#[allow(dead_code)] // consumed by later phases (short-send summary/router)
pub fn list_blobs(session_dir: &Path) -> Vec<BlobRef> {
    fn inner(session_dir: &Path) -> Result<Vec<BlobRef>> {
        let conn = open(session_dir)?;
        let mut stmt = conn.prepare(
            "SELECT id, msg_id, kind, token_est, snippet FROM blobs ORDER BY msg_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(BlobRef {
                id: r.get(0)?,
                msg_id: r.get(1)?,
                kind: r.get(2)?,
                token_est: r.get(3)?,
                snippet: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
    inner(session_dir).unwrap_or_default()
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
