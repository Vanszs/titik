//! Global usage ledger: one sqlite at `~/.koma/usage.sqlite` that persists
//! every model-call's token/cost spend across ALL sessions and working dirs.
//!
//! A future `/usage` dashboard can draw heatmaps and top-model spend from
//! this single global file. The ledger is append-only; rows are never updated
//! or deleted.
//!
//! ## Table: `usage`
//!
//! | column          | type    | notes                                    |
//! |-----------------|---------|------------------------------------------|
//! | id              | INTEGER | PRIMARY KEY AUTOINCREMENT                |
//! | ts              | INTEGER | unix seconds (NOT NULL)                  |
//! | model_id        | TEXT    | e.g. `openai/gpt-4o`                     |
//! | role            | TEXT    | `"main"` or `"sub:<agent-name>"`         |
//! | session_uuid    | TEXT    | session id (empty when not in a session) |
//! | pwd_hash        | TEXT    | working-dir bucket key                   |
//! | tokens_in       | INTEGER | prompt tokens for this call              |
//! | tokens_cached   | INTEGER | cached prompt tokens (subset of in)      |
//! | tokens_out      | INTEGER | completion tokens for this call          |
//! | cost            | REAL    | USD cost for this call                   |
//!
//! All writes go through [`record_usage`], which is **non-fatal**: any DB
//! open/insert error is swallowed and logged to stderr; it never panics or
//! returns an `Err` that could interrupt a turn.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

/// Path of the global usage ledger: `~/.koma/usage.sqlite`.
pub fn usage_db_path() -> Option<std::path::PathBuf> {
    crate::model::store::base_dir()
        .ok()
        .map(|d| d.join("usage.sqlite"))
}

/// Unix-seconds timestamp, or 0 if the clock is before the epoch.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64
}

/// Open the global usage DB and ensure the `usage` table exists.
/// Returns `None` (non-fatal) when the path cannot be resolved or the DB
/// cannot be opened.
fn open() -> Option<Connection> {
    let path = usage_db_path()?;
    // Create parent dirs best-effort so the first call on a clean install works.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(&path)
        .map_err(|e| eprintln!("koma: usage ledger open error: {e}"))
        .ok()?;
    ensure_schema(&conn)
        .map_err(|e| eprintln!("koma: usage ledger schema error: {e}"))
        .ok()?;
    Some(conn)
}

/// Create the `usage` table if it does not already exist.
fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage (
            id           INTEGER PRIMARY KEY,
            ts           INTEGER NOT NULL,
            model_id     TEXT,
            role         TEXT,
            session_uuid TEXT,
            pwd_hash     TEXT,
            tokens_in    INTEGER,
            tokens_cached INTEGER,
            tokens_out   INTEGER,
            cost         REAL
        );",
    )
}

/// Record one model call's spend into the global usage ledger.
///
/// - `model_id`: the model that served this call (e.g. `"openai/gpt-4o"`).
/// - `role`: `"main"` for a main-model turn; `"sub:<agent-name>"` for a
///   sub-agent.
/// - `session_uuid`: the active session's UUID, or `""` when there is none.
/// - `pwd_hash`: the working-dir bucket key (`""` when unavailable).
/// - `tokens_in`: prompt tokens for this call.
/// - `tokens_cached`: cached prompt tokens (a subset of `tokens_in`; 0 when
///   the provider does not report caching).
/// - `tokens_out`: completion tokens for this call.
/// - `cost`: USD cost for this call.
///
/// **Non-fatal**: any DB error is printed to stderr and silently ignored.
/// The function never panics.
#[allow(clippy::too_many_arguments)]
pub fn record_usage(
    model_id: &str,
    role: &str,
    session_uuid: &str,
    pwd_hash: &str,
    tokens_in: u64,
    tokens_cached: u64,
    tokens_out: u64,
    cost: f64,
) {
    let Some(conn) = open() else { return };
    let ts = now_secs();
    if let Err(e) = conn.execute(
        "INSERT INTO usage
            (ts, model_id, role, session_uuid, pwd_hash,
             tokens_in, tokens_cached, tokens_out, cost)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            ts, model_id, role, session_uuid, pwd_hash,
            tokens_in as i64, tokens_cached as i64, tokens_out as i64, cost
        ],
    ) {
        eprintln!("koma: usage ledger insert error: {e}");
    }
}
