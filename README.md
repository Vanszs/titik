# koma

A terminal UI (TUI) agent written in **Rust** that provides a chat‑style interface for interacting with LLMs.

## Entry point
`src-agent/src/main.rs` parses CLI arguments (`cli::parse`) and forwards them to `app::run`, which sets up the terminal, runs the event loop, and exits when the user quits.

## Core concepts

| Module | Role |
|--------|------|
| **app** | High‑level application orchestration (terminal init, runtime loop). |
| **cli** | Command‑line argument parsing. |
| **controller** | Translates terminal key events into `Action`s. |
| **view** | Renders the current `AppState` to the terminal. |
| **model** | Domain data: sessions, conversations, per‑session `Settings`, global `AppConfig`, and persistent logs. |
| **service** | External services (e.g., OpenRouter API). |
| **tool** | Helper utilities (filesystem cache, etc.). |
| **dto** | Data transfer objects for chat messages (`ChatMessage`, `Role`, `ToolCall`). |
| **config** | Loading/saving of the global user config (`~/.koma/config.json`). |
| **resources** | Static assets (icons, help text). |

### `model` sub‑modules
- **app_config.rs** – Global preferences (theme, accent). Defaults are used on any read error.
- **conversation.rs** – In‑memory chat history guaranteeing a system message at index 0. Provides push helpers, compaction (`split_for_compaction`, `apply_compaction`), and utilities for resending.
- **session.rs** – Represents a named session with its own `Settings`, `Conversation`, and persistent files (`settings.json`, `messages.json`).
- **settings.rs** – Per‑session configuration (model, temperature, etc.).
- **store.rs** – Filesystem registry for sessions under `~/.koma/`.
- **msglog.rs** – Append‑only SQLite log of every chat message.
- **memory.rs** – Optional `MEMORY.md` file loaded from a session directory to seed context.

## Data flow (simplified)

```
terminal event
   → controller::input::handle_key (KeyEvent → Action)
   → app::runtime (Action → mutate state, possibly async API call)
   → view::draw (AppState → rendered Frame)
```

## Building & running

```bash
cargo run --release   # builds the binary and starts the TUI
```

The binary reads the global config, loads or creates a session directory, and then enters the interactive loop.

---

*This README captures the high‑level architecture and key components discovered from the source code.*
