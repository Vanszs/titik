use std::path::PathBuf;
use std::time::SystemTime;
use anyhow::{anyhow, Result};
use uuid::Uuid;
use crate::config::APP_DIR_NAME;
use crate::dto::chat::{ChatMessage, Role};
use crate::model::conversation::Conversation;
use crate::model::session::Session;
use crate::model::settings::Settings;

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    pub message_count: usize, // non-system messages, best-effort
}

pub fn base_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot resolve home directory"))?;
    Ok(home.join(APP_DIR_NAME))
}

pub fn sessions_dir() -> Result<PathBuf> {
    Ok(base_dir()?.join("sessions"))
}

/// create base + sessions if missing
pub fn ensure_dirs() -> Result<()> {
    let sessions = sessions_dir()?;
    std::fs::create_dir_all(&sessions)?;
    Ok(())
}

/// sorted by modified desc, skip unreadable
pub fn list_sessions() -> Result<Vec<SessionMeta>> {
    let dir = sessions_dir()?;
    let mut metas: Vec<SessionMeta> = Vec::new();

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(metas), // no sessions dir yet
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

        // name: from settings.json if present, else id.
        let settings_path = path.join("settings.json");
        let name = match Settings::load(&settings_path) {
            Ok(s) if !s.name.is_empty() => s.name,
            _ => id.clone(),
        };

        // message_count: read messages.json, count non-System; on failure -> 0.
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

    metas.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(metas)
}

/// fresh uuid-v4 dir, saved
pub fn create_session() -> Result<Session> {
    let id = Uuid::new_v4().to_string();
    let dir = sessions_dir()?.join(&id);
    std::fs::create_dir_all(dir.join("memory"))?;

    let settings = Settings {
        name: id.clone(),
        ..Default::default()
    };
    let conversation = Conversation::from_messages(vec![]);
    let mut session = Session::new(id, dir, settings, conversation);
    session.rebuild_system();
    session.save()?;
    Ok(session)
}

pub fn rename_session(session: &mut Session, new_name: &str) -> Result<()> {
    let slug = slugify(new_name)?;
    let parent = sessions_dir()?;

    // Find a free target dir: <parent>/<slug>, then -2, -3, ...
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

    if target != session.path {
        std::fs::rename(&session.path, &target)?;
    }

    let final_id = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| slug.clone());

    session.id = final_id;
    session.path = target;
    let display = new_name.trim().to_string();
    session.name = display.clone();
    session.settings.name = display;
    session.save()?;
    Ok(())
}

/// lowercase chars, non-alphanumeric -> space, split_whitespace, join with '-'.
/// Err if the result contains no usable characters.
pub(crate) fn slugify(name: &str) -> Result<String> {
    let mut mapped = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                mapped.push(lc);
            }
        } else {
            mapped.push(' ');
        }
    }
    let slug = mapped.split_whitespace().collect::<Vec<_>>().join("-");
    if slug.is_empty() {
        Err(anyhow!("name contains no usable characters"))
    } else {
        Ok(slug)
    }
}
