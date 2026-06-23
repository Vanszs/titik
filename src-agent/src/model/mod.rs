//! Domain model for the agent: sessions, conversations, settings, and storage.
//!
//! Module map:
//! - `conversation` — in-memory chat history + compaction helpers.
//! - `session`      — a single named session (id, path, settings, conversation).
//! - `settings`     — per-session `Settings` persisted to `settings.json`.
//! - `store`        — filesystem session registry (list / create / rename).
//! - `memory`       — reads the optional `MEMORY.md` from a session directory.

pub mod app_config;
pub mod conversation;
pub mod memory;
pub mod session;
pub mod settings;
pub mod store;
