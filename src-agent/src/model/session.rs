use std::path::{Path, PathBuf};
use anyhow::Result;
use crate::dto::chat::ChatMessage;
use crate::model::conversation::Conversation;
use crate::model::memory::load_memory;
use crate::model::settings::Settings;
use crate::resources;

pub struct Session {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub settings: Settings,
    pub conversation: Conversation,
}

impl Session {
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

        // Seed/overwrite the system message via set_system so the embedded
        // prompt + live MEMORY.md always win over a stale stored system message.
        session.rebuild_system();
        Ok(session)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.path)?;
        self.settings.save(&self.settings_path())?;
        let json = serde_json::to_vec_pretty(self.conversation.messages())?;
        std::fs::write(self.messages_path(), json)?;
        Ok(())
    }

    pub fn rebuild_system(&mut self) {
        let mem = load_memory(&self.path);
        let sys = resources::build_system_prompt(mem.as_deref());
        self.conversation.set_system(sys);
    }
}
