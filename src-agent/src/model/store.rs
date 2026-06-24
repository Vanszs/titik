//! Filesystem session registry: list, create, rename sessions.
//!
//! All sessions live under `~/.simple-coder/sessions/`. Each session is a
//! sub-directory whose name is both the session `id` and the filesystem slug:
//!
//! ```text
//! ~/.simple-coder/
//!     sessions/
//!         550e8400-e29b-41d4-a716-446655440000/   ← UUID (new session)
//!         my-project-notes/                        ← slug (after rename)
//!             settings.json
//!             messages.json
//!             memory/
//!                 MEMORY.md
//! ```
//!
//! **Key operations:**
//! - `list_sessions` — enumerate directories, sort by mtime descending.
//! - `create_session` — allocate a UUID directory, call `rebuild_system`, save.
//! - `rename_session` — slugify the new name, find a free directory, `fs::rename`.
//! - `slugify` — normalise a human name into a safe directory name.

use std::path::{Path, PathBuf};
use std::time::SystemTime;
use anyhow::{anyhow, Result};
use uuid::Uuid;
use crate::config::APP_DIR_NAME;
use crate::dto::chat::{ChatMessage, Role};
use crate::model::conversation::Conversation;
use crate::model::session::Session;
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

/// Returns `~/.simple-coder/` (the application data root).
pub fn base_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot resolve home directory"))?;
    Ok(home.join(APP_DIR_NAME))
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

/// List all sessions, sorted by directory mtime descending (most-recent first).
///
/// Unreadable directories are silently skipped so a single corrupt session
/// doesn't break the list. The System message is excluded from `message_count`.
pub fn list_sessions() -> Result<Vec<SessionMeta>> {
    let dir = sessions_dir()?;
    let mut metas: Vec<SessionMeta> = Vec::new();

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(metas), // sessions dir not created yet — return empty
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = match path.file_name() {
            Some(s) => s.to_string_lossy().into_owned(),
            None => continue,
        };

        // Prefer settings.name; fall back to the directory id.
        let settings_path = path.join("settings.json");
        let name = match Settings::load(&settings_path) {
            Ok(s) if !s.name.is_empty() => s.name,
            _ => id.clone(),
        };

        // Count non-System messages for the list view; 0 on any parse failure.
        let messages_path = path.join("messages.json");
        let message_count = match std::fs::read(&messages_path) {
            Ok(bytes) => serde_json::from_slice::<Vec<ChatMessage>>(&bytes)
                .map(|msgs| msgs.iter().filter(|m| m.role != Role::System).count())
                .unwrap_or(0),
            Err(_) => 0,
        };

        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        // Lock state for the picker. `is_locked` treats a stale lock (dead PID)
        // as unlocked and opportunistically clears it, so this is also the place
        // crashed-instance leftovers get swept on the next listing.
        let locked = is_locked(&path);

        metas.push(SessionMeta {
            id,
            name,
            path,
            modified,
            message_count,
            locked,
        });
    }

    // Most-recently modified session first.
    metas.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(metas)
}

/// Create a brand-new session with a UUID id.
///
/// Also creates `memory/` inside the session directory so `load_memory` can
/// scan it without an error. Calls `rebuild_system` before the first save so
/// the system prompt is set correctly.
pub fn create_session() -> Result<Session> {
    let id = Uuid::new_v4().to_string();
    let dir = sessions_dir()?.join(&id);
    // Pre-create memory/ so the user can drop MEMORY.md there immediately.
    std::fs::create_dir_all(dir.join("memory"))?;

    let settings = Settings {
        name: id.clone(),
        // Seed the workdir path list with a single entry: the launch cwd.
        workdir: std::env::current_dir()
            .map(|p| vec![p.display().to_string()])
            .unwrap_or_default(),
        ..Default::default()
    };
    let conversation = Conversation::from_messages(vec![]);
    let mut session = Session::new(id, dir, settings, conversation);
    session.rebuild_system();
    session.save()?;
    Ok(session)
}

/// Rename a session by slugifying `new_name` and moving its directory.
///
/// The new directory name is `slugify(new_name)`. If that directory already
/// exists (and is not the current session), a numeric suffix is appended:
/// `<slug>-2`, `<slug>-3`, … until a free slot is found. The session's `id`,
/// `path`, `name`, and `settings.name` are updated in-place, then saved.
pub fn rename_session(session: &mut Session, new_name: &str) -> Result<()> {
    let slug = slugify(new_name)?;
    let parent = sessions_dir()?;

    // Start with the bare slug; if taken (by a different session), increment.
    let mut target = parent.join(&slug);
    if target.exists() && target != session.path {
        let mut n = 2;
        loop {
            let candidate = parent.join(format!("{slug}-{n}"));
            if !candidate.exists() {
                target = candidate;
                break;
            }
            n += 1;
        }
    }

    // Only rename on disk if the path actually changed (avoids a no-op syscall
    // when the slug happens to match the current directory name).
    if target != session.path {
        std::fs::rename(&session.path, &target)?;
    }

    let final_id = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| slug.clone());

    session.id = final_id;
    session.path = target;
    // Keep the raw display name (untrimmed is already trimmed below) separate
    // from the slug-derived id so the UI can show "My Project" not "my-project".
    let display = new_name.trim().to_string();
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
