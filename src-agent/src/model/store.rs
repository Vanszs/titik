//! Session store: list, create, rename sessions in the pwd-keyed layout.
//!
//! Sessions are bucketed by the working directory they were opened from. Every
//! session opened from the same canonical workdir shares one `pwd_hash` bucket,
//! which holds a shared `settings.json` (the per-dir model catalogue) plus one
//! sub-directory per session UUID. Which sessions belong to which bucket — and
//! their display names + timestamps — is tracked in the SQLite registry
//! (`session_registry`), NOT by scanning the filesystem.
//!
//! ```text
//! ~/.simple-coder/
//!     session.sqlite                               ← registry (uuid → pwd_hash, name, …)
//!     sessions/
//!         <pwd_hash>/                              ← one bucket per working dir
//!             settings.json                        ← shared LocalConfig (session_models)
//!             550e8400-e29b-41d4-a716-446655440000/  ← one dir per session UUID
//!                 settings.json                    ← per-session behavioural settings
//!                 messages.json
//!                 messages.sqlite
//!                 memory/
//!                     MEMORY.md
//! ```
//!
//! **Key operations:**
//! - `list_sessions` — registry rows for the CURRENT dir's `pwd_hash`, newest first.
//! - `create_session` — allocate a UUID dir under the cwd's bucket, register, save.
//! - `rename_session` — update the registry `name` only (no filesystem move).
//!
//! Pre-swap `sessions/<name>/` directories from the old layout are never
//! registered, so they are simply not listed (and never crash the list).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use anyhow::{anyhow, Result};
use uuid::Uuid;
use crate::config::APP_DIR_NAME;
use crate::dto::chat::{ChatMessage, Role};
use crate::model::conversation::Conversation;
use crate::model::session::Session;
use crate::model::session_registry;
use crate::model::settings::Settings;

/// Lightweight metadata about a session used in the session-list UI.
///
/// Loaded without deserialising the full message history — only `settings.json`
/// and the message count are read, keeping the list fast even for large histories.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    /// Number of non-System messages, counted best-effort (0 on read failure).
    pub message_count: usize,
    /// `true` when the session is currently open in a LIVE process (a fresh
    /// `session.lock` holding a still-running PID). Computed via [`is_locked`],
    /// so a stale lock from a crashed instance reads as unlocked. The picker
    /// shows a lock marker and refuses to enter a locked session.
    pub locked: bool,
}

/// Returns `~/.koma/` (the application data root).
pub fn base_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot resolve home directory"))?;
    Ok(home.join(APP_DIR_NAME))
}

/// Root of koma's throwaway scratch space (`<temp>/koma`). Bash + file tools
/// are permitted to read/write anywhere under here.
pub fn scratch_root() -> PathBuf {
    std::env::temp_dir().join("koma")
}

/// Per-session scratch dir (`<temp>/koma/<session_id>`).
pub fn scratch_dir(session_id: &str) -> PathBuf {
    scratch_root().join(session_id)
}

/// One-time, non-destructive migration: rename `~/.simple-coder` to `~/.koma`
/// if the new dir does not yet exist and the old one does.
///
/// Must be called ONCE at startup before any code reads `base_dir()`.
/// Never panics — any error is printed to stderr and silently ignored so the
/// app can proceed (it will create a fresh `~/.koma` on first use).
pub fn migrate_legacy_dir() {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => {
            eprintln!("koma: warning: cannot resolve home directory; skipping config migration");
            return;
        }
    };
    let old_dir = home.join(".simple-coder");
    let new_dir = home.join(APP_DIR_NAME); // ".koma"
    if new_dir.exists() {
        // New dir already exists — nothing to do.
        return;
    }
    if !old_dir.exists() {
        // Neither dir exists yet — fresh install, nothing to migrate.
        return;
    }
    match std::fs::rename(&old_dir, &new_dir) {
        Ok(()) => eprintln!("migrated config: ~/.simple-coder -> ~/.koma"),
        Err(e) => eprintln!("koma: warning: could not migrate ~/.simple-coder to ~/.koma: {e}"),
    }
}

/// Returns `~/.simple-coder/sessions/`.
pub fn sessions_dir() -> Result<PathBuf> {
    Ok(base_dir()?.join("sessions"))
}

/// Create `~/.simple-coder/sessions/` (and its parents) if they do not exist.
pub fn ensure_dirs() -> Result<()> {
    let sessions = sessions_dir()?;
    std::fs::create_dir_all(&sessions)?;
    Ok(())
}

// --- pwd-keyed layout paths --------------------------------------------------
//
// These helpers compute the bucket hash and the on-disk paths for the pwd-keyed
// layout; the registry (`session_registry`) tracks which sessions belong to
// which bucket.

/// Deterministic hash of a working directory, stable across runs.
///
/// Canonicalises `workdir` (resolving symlinks / `..`); if canonicalisation
/// fails (e.g. the dir doesn't exist yet) the path is used as-is so the call is
/// infallible. The canonical path string is hashed with UUID v5 over the OID
/// namespace, and the simple (hyphenless) hex form is returned. Same directory
/// → same hash every time.
pub fn pwd_hash(workdir: &Path) -> String {
    let canonical = std::fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());
    let path_str = canonical.to_string_lossy();
    Uuid::new_v5(&Uuid::NAMESPACE_OID, path_str.as_bytes())
        .simple()
        .to_string()
}

/// The bucket directory for a working dir: `~/.simple-coder/sessions/<pwd_hash>/`.
/// Shared by every session opened from that directory.
pub fn pwd_bucket_dir(pwd_hash: &str) -> Result<PathBuf> {
    Ok(sessions_dir()?.join(pwd_hash))
}

/// Shared per-dir settings path: `<pwd_bucket_dir>/settings.json`. Holds the
/// [`LocalConfig`](crate::model::settings::LocalConfig) (model setup) common to
/// all sessions in this working directory.
pub fn shared_settings_path(pwd_hash: &str) -> Result<PathBuf> {
    Ok(pwd_bucket_dir(pwd_hash)?.join("settings.json"))
}

/// The shared per-PROJECT memory directory: `<pwd_bucket_dir>/memory/`. Every
/// session opened from the same working directory shares ONE memory store here
/// (mirrors [`shared_settings_path`]), so memories saved in one session are
/// visible from every other session in the same project.
///
/// The directory (and its bucket parent) is created on access so callers can
/// read/write under it without a separate `create_dir_all`.
pub fn memory_dir(pwd_hash: &str) -> Result<PathBuf> {
    let dir = pwd_bucket_dir(pwd_hash)?.join("memory");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A single session's directory under its bucket:
/// `<pwd_bucket_dir>/<uuid>/`. Holds the per-session behavioural settings,
/// messages, memory, and agents.
pub fn session_dir(pwd_hash: &str, uuid: &str) -> Result<PathBuf> {
    Ok(pwd_bucket_dir(pwd_hash)?.join(uuid))
}

/// A session's image-attachment directory: `<session_dir>/images/`. Holds the
/// copied-in image bytes for every pasted/picked image attachment
/// (`images/NN-name.ext`). Lives INSIDE the session dir, so deleting a session
/// already removes its images — no separate cleanup is needed.
pub fn session_images_dir(pwd_hash: &str, uuid: &str) -> Result<PathBuf> {
    Ok(session_dir(pwd_hash, uuid)?.join("images"))
}

/// Create a session's `images/` dir (and parents) if absent. Best-effort, called
/// the same place the scratch dir is set up; a failure just means the first
/// ingest will retry the create.
pub fn ensure_session_images_dir(pwd_hash: &str, uuid: &str) {
    if let Ok(dir) = session_images_dir(pwd_hash, uuid) {
        let _ = std::fs::create_dir_all(&dir);
    }
}

/// Path to the SQLite session registry: `~/.simple-coder/session.sqlite`.
pub fn registry_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("session.sqlite"))
}

/// Path to the daemon's unix-domain socket: `~/.koma/daemon.sock`.
///
/// This socket is the koma-daemon's liveness oracle (whoever binds it IS the live
/// daemon) and the rendezvous point the thin TUI client connects to. Resolved
/// from the same [`base_dir`] (`~/.koma`) as every other config path.
pub fn daemon_sock_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("daemon.sock"))
}

/// Path to the daemon's PID file: `~/.koma/daemon.pid`.
///
/// Advisory only — recorded for diagnostics/`kill`. It is NOT the liveness oracle
/// (PIDs get reused, which would wedge spawn-or-attach); the bound socket at
/// [`daemon_sock_path`] is. Lives under the same [`base_dir`] (`~/.koma`).
#[allow(dead_code)] // wired in daemon stage 3+ (pid file written on daemonize)
pub fn daemon_pid_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("daemon.pid"))
}

/// List the sessions for the CURRENT working directory, most-recently updated
/// first.
///
/// The list is driven by the SQLite registry (`session_registry`), NOT a
/// filesystem scan: only sessions whose `pwd_hash` matches `std::env::current_dir()`
/// are returned, already ordered by `updated_at` descending. Old pre-swap
/// `sessions/<name>/` directories are never registered, so they simply don't
/// appear — and an absent registry (first run) yields an empty list rather than
/// an error. The System message is excluded from `message_count`.
pub fn list_sessions() -> Result<Vec<SessionMeta>> {
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let hash = pwd_hash(&workdir);

    let rows = session_registry::list_by_pwd(&hash)?;
    let mut metas: Vec<SessionMeta> = Vec::with_capacity(rows.len());

    for row in rows {
        let path = session_dir(&hash, &row.uuid)?;

        // Count non-System messages for the list view; 0 on any parse failure
        // (e.g. a session that's registered but never saved messages.json yet).
        let messages_path = path.join("messages.json");
        let message_count = match std::fs::read(&messages_path) {
            Ok(bytes) => serde_json::from_slice::<Vec<ChatMessage>>(&bytes)
                .map(|msgs| msgs.iter().filter(|m| m.role != Role::System).count())
                .unwrap_or(0),
            Err(_) => 0,
        };

        // The registry's updated_at (unix seconds) is the "modified" time; the
        // picker view formats it as an elapsed duration. Saturating add keeps a
        // garbage/negative timestamp from panicking.
        let modified = row
            .updated_at
            .try_into()
            .ok()
            .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap_or(SystemTime::UNIX_EPOCH);

        // Lock state for the picker. `is_locked` treats a stale lock (dead PID)
        // as unlocked and opportunistically clears it, so this is also the place
        // crashed-instance leftovers get swept on the next listing.
        let locked = is_locked(&path);

        metas.push(SessionMeta {
            id: row.uuid,
            name: row.name,
            path,
            modified,
            message_count,
            locked,
        });
    }

    Ok(metas)
}

/// Create a brand-new session with a UUID id, bucketed by the current working
/// directory's `pwd_hash`.
///
/// Layout: the session lives at `sessions/<pwd_hash>/<uuid>/`. Also creates
/// `memory/` inside the session directory so `load_memory` can scan it without
/// an error, registers the session in the SQLite registry (the rename/list
/// source of truth), and calls `rebuild_system` before the first save so the
/// system prompt is set correctly.
pub fn create_session() -> Result<Session> {
    // The launch cwd determines both the bucket (pwd_hash) and the seeded
    // workdir. Fall back to "." if the cwd can't be resolved.
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let hash = pwd_hash(&workdir);
    let uuid = Uuid::new_v4().to_string();
    let dir = session_dir(&hash, &uuid)?;
    // Pre-create memory/ so the user can drop MEMORY.md there immediately. This
    // also creates the session dir (and its bucket parent) as a side effect.
    std::fs::create_dir_all(dir.join("memory"))?;

    // Best-effort: create the per-session scratch dir so it is ready immediately.
    let scratch = scratch_dir(&uuid);
    if let Err(e) = std::fs::create_dir_all(&scratch) {
        eprintln!("koma: warning: could not create scratch dir {}: {e}", scratch.display());
    }

    // Pre-create the image-attachment dir so the first paste-ingest has a home.
    ensure_session_images_dir(&hash, &uuid);

    let workdir_str = workdir.display().to_string();
    let settings = Settings {
        name: uuid.clone(),
        // Seed the workdir path list with a single entry: the launch cwd.
        workdir: vec![workdir_str.clone()],
        ..Default::default()
    };
    let conversation = Conversation::from_messages(vec![]);
    let mut session = Session::new(uuid.clone(), dir, hash.clone(), settings, conversation);
    // Register before the first save so the row exists for list/rename. The
    // initial display name is the uuid (matches settings.name).
    session_registry::register(&uuid, &hash, &uuid, &workdir_str)?;
    session.rebuild_system();
    session.save()?;
    Ok(session)
}

/// Rename a session by updating its registry `name` only.
///
/// In the pwd-keyed layout the on-disk directory is the immutable session UUID;
/// the display name lives in the SQLite registry, so a rename is just a name
/// update there — NO filesystem move, NO collision handling. The session's
/// in-memory `name` / `settings.name` are updated to match (the `id`, `path`,
/// and `pwd_hash` are unchanged), then the session is saved.
pub fn rename_session(session: &mut Session, new_name: &str) -> Result<()> {
    let display = new_name.trim().to_string();
    session_registry::set_name(&session.id, &display)?;
    session.name = display.clone();
    session.settings.name = display;
    session.save()?;
    Ok(())
}

/// Convert an arbitrary string into a lowercase, hyphen-separated filesystem slug.
///
/// Algorithm:
/// 1. Walk each Unicode character of `name`.
/// 2. Alphanumeric characters are lowercased and kept.
/// 3. Every non-alphanumeric character becomes a space.
/// 4. The result is split on whitespace and joined with `'-'`, collapsing
///    consecutive non-alphanumeric runs into a single hyphen.
///
/// Returns `Err` if the result is empty (e.g. the input was all punctuation).
///
/// Examples: `"My Project!"` → `"my-project"`, `"  foo  bar  "` → `"foo-bar"`.
///
/// Retained for potential reuse / a friendlier on-disk layout; the pwd-keyed
/// rename no longer slugifies (directories are immutable UUIDs, the name lives
/// in the registry), so this is currently unused.
#[allow(dead_code)]
pub(crate) fn slugify(name: &str) -> Result<String> {
    let mut mapped = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            // to_lowercase() returns an iterator because some chars expand to
            // multiple code points (e.g. the German ß → ss).
            for lc in c.to_lowercase() {
                mapped.push(lc);
            }
        } else {
            // Treat any non-alphanumeric character as a word separator.
            mapped.push(' ');
        }
    }
    // split_whitespace collapses consecutive spaces, join reinserts hyphens.
    let slug = mapped.split_whitespace().collect::<Vec<_>>().join("-");
    if slug.is_empty() {
        Err(anyhow!("name contains no usable characters"))
    } else {
        Ok(slug)
    }
}

// --- Per-session locking ----------------------------------------------------
//
// A running instance marks its active session with a `session.lock` file inside
// the session directory; the file holds the owner's PID. The picker reads these
// to show a lock marker and to refuse re-entering a session that is already open
// (including this instance's own session, which it holds the lock for).
//
// Crash safety is by PID liveness, not by the file's mere presence: if the PID
// recorded in `session.lock` is no longer a live process, the lock is STALE and
// treated as unlocked (and the stale file is swept). This platform is Linux, so
// liveness is a `/proc/<pid>` existence check. All IO here is best-effort — a
// failed read/write/remove must never crash or block the TUI.

/// Path to a session's lock file: `<session_dir>/session.lock`.
fn lock_path(session_dir: &Path) -> PathBuf {
    session_dir.join("session.lock")
}

/// Whether `pid` refers to a live process on this (Linux) host.
///
/// Our own PID is alive by definition. Any other PID is alive iff `/proc/<pid>`
/// exists — the simple, Linux-correct check the spec calls for.
fn pid_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Write our PID into the session's lock file, overwriting any existing one.
///
/// Best-effort: IO errors are ignored (e.g. a read-only or vanished dir just
/// means the session won't appear locked — never a crash).
pub fn write_lock(session_dir: &Path) {
    let _ = std::fs::write(lock_path(session_dir), std::process::id().to_string());
}

/// Remove the session's lock file. Best-effort; a missing file or IO error is
/// ignored.
pub fn remove_lock(session_dir: &Path) {
    let _ = std::fs::remove_file(lock_path(session_dir));
}

/// Whether the session is locked by a LIVE process (this one included).
///
/// Reads the PID from `session.lock`:
/// - file missing / unreadable → NOT locked.
/// - PID parses and is alive   → locked.
/// - PID is dead or unparseable → STALE: treat as NOT locked and opportunistically
///   remove the stale file so it stops haunting future checks.
pub fn is_locked(session_dir: &Path) -> bool {
    let path = lock_path(session_dir);
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false, // no lock file (or unreadable) → not locked
    };
    match contents.trim().parse::<u32>() {
        Ok(pid) if pid_alive(pid) => true,
        // Dead PID or garbage in the file: stale lock. Sweep it (best-effort)
        // and report unlocked so a crashed instance never blocks a session.
        _ => {
            let _ = std::fs::remove_file(&path);
            false
        }
    }
}
