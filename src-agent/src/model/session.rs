//! A single chat session: identity, filesystem path, settings, and conversation.
//!
//! A `Session` owns everything that belongs to one named conversation on disk:
//!
//! ```text
//! ~/.simple-coder/sessions/<id>/
//!     settings.json   ← Settings (model, api_key, compaction…)
//!     messages.json   ← Vec<ChatMessage> (the full history)
//!     memory/
//!         MEMORY.md   ← optional long-term context (see model::memory)
//! ```
//!
//! **Load path:** `store::load` (or `Session::load` directly) reads both JSON
//! files, then immediately calls `rebuild_system()` so the live system prompt
//! (embedded binary + MEMORY.md) always overwrites any stale system message
//! that was stored in `messages.json`.
//!
//! **Save path:** `Session::save` writes `settings.json` and `messages.json`
//! atomically enough for a TUI — no WAL, no rename-over, just `write`.

use std::path::{Path, PathBuf};
use anyhow::Result;
use crate::dto::chat::ChatMessage;
use crate::model::conversation::Conversation;
use crate::model::memory::{load_agents, load_memory};
use crate::model::settings::Settings;
use crate::resources;

/// One named chat session.
///
/// `id` is the directory name under `~/.simple-coder/sessions/` (a UUID for
/// new sessions, or a slug after `store::rename_session`). `name` is the
/// human-readable label shown in the session list — it defaults to `id` when
/// `settings.name` is empty.
pub struct Session {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub settings: Settings,
    pub conversation: Conversation,
}

impl Session {
    /// Construct a `Session` from its parts.
    ///
    /// `name` is derived from `settings.name`, falling back to `id` when the
    /// name is blank. This is the only place that enforces the fallback.
    pub fn new(
        id: String,
        path: PathBuf,
        settings: Settings,
        conversation: Conversation,
    ) -> Self {
        let name = if settings.name.is_empty() {
            id.clone()
        } else {
            settings.name.clone()
        };
        Self {
            id,
            name,
            path,
            settings,
            conversation,
        }
    }

    fn settings_path(&self) -> PathBuf {
        self.path.join("settings.json")
    }

    fn messages_path(&self) -> PathBuf {
        self.path.join("messages.json")
    }

    /// Load a session from `dir` on disk.
    ///
    /// Steps:
    /// 1. Derive `id` from the directory name.
    /// 2. Read `settings.json` (or use defaults if absent).
    /// 3. Read `messages.json` verbatim. A missing or unparseable file yields
    ///    an empty vec; no placeholder system message is inserted here.
    /// 4. Call `rebuild_system()` to seed/overwrite `messages[0]` with the
    ///    embedded system prompt + live MEMORY.md. This ensures the stored
    ///    system message (which may be stale) is always replaced on resume.
    pub fn load(dir: &Path) -> Result<Self> {
        let id = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        let settings_path = dir.join("settings.json");
        let settings = if settings_path.exists() {
            Settings::load(&settings_path)?
        } else {
            Settings {
                name: id.clone(),
                ..Default::default()
            }
        };

        // Read messages.json verbatim. If missing OR the parsed vec is empty,
        // start with an empty conversation (no placeholder System seeding here).
        let messages_path = dir.join("messages.json");
        let messages: Vec<ChatMessage> = match std::fs::read(&messages_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Vec::new(),
        };

        let conversation = Conversation::from_messages(messages);

        let name = if settings.name.is_empty() {
            id.clone()
        } else {
            settings.name.clone()
        };

        let mut session = Self {
            id,
            name,
            path: dir.to_path_buf(),
            settings,
            conversation,
        };

        // Overwrite the stored system message with the live one so that
        // changes to the embedded prompt or MEMORY.md take effect on resume.
        session.rebuild_system();
        Ok(session)
    }

    /// Persist `settings.json` and `messages.json` to `self.path`.
    ///
    /// Creates `self.path` if it does not exist (needed for a brand-new
    /// session before its first save).
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.path)?;
        self.settings.save(&self.settings_path())?;
        let json = serde_json::to_vec_pretty(self.conversation.messages())?;
        std::fs::write(self.messages_path(), json)?;
        Ok(())
    }

    /// The session's working directory: the `workdir` setting if set, else the
    /// process's current dir.
    pub fn workdir(&self) -> std::path::PathBuf {
        if self.settings.workdir.trim().is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            std::path::PathBuf::from(self.settings.workdir.trim())
        }
    }

    /// Rebuild the system prompt and push it into the conversation.
    ///
    /// Called on session load and after the user edits `MEMORY.md` at runtime.
    /// Reads the session's `memory/MEMORY.md` (via `load_memory`), passes the
    /// result to `resources::build_system_prompt` which stitches together the
    /// embedded base prompt and the optional memory section, then calls
    /// `Conversation::set_system` to insert or replace `messages[0]`.
    pub fn rebuild_system(&mut self) {
        let mem = load_memory(&self.path);
        let agents = load_agents(&self.workdir());
        let sys = resources::build_system_prompt(mem.as_deref(), agents.as_deref());
        self.conversation.set_system(sys);
    }
}
