//! Domain model for the agent: sessions, conversations, settings, and storage.
//!
//! Module map:
//! - `agent_def`    — agent definitions (frontmatter `.md`): load / merge / persist.
//! - `conversation` — in-memory chat history + compaction helpers.
//! - `session`      — a single named session (id, path, settings, conversation).
//! - `settings`     — per-session `Settings` persisted to `settings.json`.
//! - `store`        — filesystem session registry (list / create / rename).
//! - `memory`       — reads the optional `MEMORY.md` from a session directory.
//! - `msglog`       — per-session append-only SQLite log of every chat message.

pub mod agent_def;
pub mod app_config;
pub mod catalogue;
pub mod conversation;
pub mod memory;
pub mod msglog;
pub mod session;
pub mod settings;
pub mod store;
