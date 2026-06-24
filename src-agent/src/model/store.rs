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

use std::path::PathBuf;
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

        metas.push(SessionMeta {
            id,
            name,
            path,
            modified,
            message_count,
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
