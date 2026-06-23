# Architecture

A ratatui TUI chat client for the OpenRouter API. Sessions are persisted to disk; the app can resume any previous conversation. Configuration (API key, model) is stored per-session so multiple sessions can use different accounts simultaneously.

---

## The event loop

`app/runtime.rs::run_loop` is a single synchronous loop on the main thread. Each tick does three things in order:

1. **Drain the active stream** — pull all buffered `StreamEvent`s from `state.rest.active_rx` via `try_recv`.
2. **Read terminal input** — poll with an adaptive timeout, then drain every buffered event (paste / fast typing lands without lag).
3. **Render** — draw only when `dirty = true` (set by any state change).

**Adaptive timeout:** 8 ms while a request is in flight (`state.rest.waiting`), 100 ms when idle. At 8 ms the loop can process up to 125 stream-drain + render cycles per second, keeping token display at 60fps or better. At 100 ms idle the UI is effectively free on CPU.

**Drain-all-on-tick:** after the first `poll(timeout)` returns true, the loop immediately calls `poll(Duration::ZERO)` in a second inner loop to pull every additional event that arrived in the same tick. Pasted text and rapid keystrokes are never queued across ticks.

```
tick
 ├─ drain active_rx  (try_recv loop)
 ├─ poll(8ms | 100ms)
 │   └─ inner: poll(0) loop → read each buffered event
 └─ draw if dirty
```

---

## Data flow

```
KeyEvent
  → controller/input.rs::handle_key()   →  Action
  → app/runtime.rs::apply_action()      →  state mutation
  → view/mod.rs::draw()                 →  terminal frame
```

This is standard MVC: the controller translates raw input into an intent (`Action`), the model/state layer is mutated by the runtime, and the view reads state and renders.

Slash commands follow the same path. When the user types a `/`-prefixed line and presses Enter, `handle_chat` routes to `controller/command.rs::parse()`, which returns a `Command` value wrapped in `Action::Slash`. The runtime then calls `apply_slash(cmd, ...)` to act on it.

---

## The async/streaming bridge

There is one `tokio::runtime::Runtime` created in `run()`. The main event loop is synchronous; async work is spawned onto that runtime via `handle.spawn(...)`.

**One channel per request.** `start_stream_task` opens a fresh `tokio::sync::mpsc::unbounded_channel` for every new request. The receiver (`rx`) is stored in `state.rest.active_rx`; the sender (`tx`) is moved into the spawned task. The task calls `OpenRouterClient::stream_complete(messages, tx)` and emits `StreamEvent`s:

| Event | Meaning |
|---|---|
| `Token(String)` | Append text to the in-progress streaming buffer |
| `Done` | Stream finished cleanly; commit the buffer as an assistant message |
| `Error(String)` | Stream failed; show the error in the status line |
| `Compacted { summary, kept_tail }` | `/compact` result; replace conversation history |

Each tick the loop does `state.rest.active_rx.take()` (not borrow — take, so it can mutate other fields of `rest` in the match arms), drains all pending events, then puts the receiver back if the stream is still open.

**Cancellation without a generation counter.** `abort_current` aborts the task's `AbortHandle` and sets `state.rest.active_rx = None`, dropping the receiver. Any events the aborted task sends after that point hit a closed channel and are silently discarded (`let _ = tx.send(...)` in the task). This happens on Ctrl+C interrupt, `/new`, and quit — no generation tagging is needed because the channel itself is the identity of the request.

`finish_stream` is called on `Done` or `Error`. It takes the buffered partial reply (`rest.take_stream()`), pushes it to the conversation as an assistant message, and saves to disk.

---

## Mode state machine

The app is always in exactly one of three modes (`app/mode.rs`):

```
┌──────────────┐   SaveCreds    ┌──────┐
│  KeyInput    │ ─────────────► │ Chat │
│  (creds form)│ ◄───────────── │      │
└──────────────┘  /new or Esc   └──────┘
       ▲                            ▲
       │  session has no key        │ PickerSelect (key present)
       │                            │
┌──────────────┐                    │
│SessionPicker │ ───────────────────┘
│  (--resume)  │
└──────────────┘
```

**Transitions:**

- **First run** (no `--resume`): starts in `KeyInput` with `first_run = true`. Esc quits (there is no Chat to return to). `SaveCreds` transitions to `Chat`.
- **`--resume`**: starts in `SessionPicker`. Selecting a session with a stored key goes directly to `Chat`. Selecting one without a key opens `KeyInput` with `from_picker = true`, so Esc returns to the picker (`CancelKeyInputToPicker`) rather than a broken Chat with no client.
- **`/new` from Chat**: aborts any in-flight request, creates a new session directory, opens `KeyInput` with `first_run = false` and `from_picker = false`. Esc restores the previous session via `prev_session` (`CancelKeyInput`).

---

## On-disk layout

```
~/.simple-coder/
└── sessions/
    └── <id>/                    ← UUID on creation; slug after /rename
        ├── settings.json        ← api_key, model, name, compaction.preserve_n
        ├── messages.json        ← Vec<ChatMessage> (full transcript)
        └── memory/
            └── MEMORY.md        ← optional; loaded into system prompt at startup
```

`settings.json` stores the API key per-session by design: different sessions can use different OpenRouter accounts without a global config file.

The system prompt is assembled at startup (and on session load/resume) by `resources::build_system_prompt`. The base instructions and personality addendum are embedded into the binary at compile time via `include_dir!` from `src-misc/system-prompt.txt` and `src-misc/system-personality.txt`. If `memory/MEMORY.md` exists and is non-empty, it is appended under a `# Memory` heading. The assembled string is always re-inserted as `messages[0]` on load, so changes to the embedded prompt or the memory file take effect on next resume.

---

## Module map

| Layer | Path | Responsibility |
|---|---|---|
| DTOs | `dto/` | Wire types for OpenRouter (`ChatRequest`, `StreamChunk`) and the core `ChatMessage`/`Role` |
| Model | `model/` | `Session`, `Conversation`, `Settings`, `store` (filesystem registry), `memory` loader |
| Service | `service/` | `OpenRouterClient` (streaming + one-shot HTTP), `StreamEvent` enum |
| Controller | `controller/` | `input.rs` (key → `Action`), `command.rs` (`/slash` → `Command`) |
| View | `view/` | `draw` dispatcher; sub-modules `chat`, `key_input`, `session_picker` |
| App | `app/` | `state.rs` (`AppState`/`AppStateRest`), `mode.rs` (`Mode` + form types), `runtime.rs` (event loop) |
| Resources | `resources.rs` | Compile-time prompt embedding, `build_system_prompt` |

---

## How to add a feature

**New slash command** — three steps:
1. Add a variant to `Command` in `controller/command.rs`.
2. Add a match arm to `parse()` in the same file.
3. Add a match arm to `apply_slash()` in `app/runtime.rs`.

**New key binding** — add a match arm (or `is_ctrl` check) in the relevant handler inside `controller/input.rs` (`handle_chat`, `handle_key_input`, or `handle_picker`). Return the appropriate `Action` (add a new `Action` variant if needed, then handle it in `apply_action`).

**New settings field** — add the field to `Settings` in `model/settings.rs` with `#[serde(default)]` and a `Default` implementation so existing `settings.json` files deserialise without error.

---

## Build and run

```sh
# First run — prompts for API key and model, then opens chat
cargo run -p agent

# Session picker — resume a previous conversation
cargo run -p agent -- --resume
```

The binary is the `agent` crate. No environment variables are required; all configuration is entered interactively and stored in `~/.simple-coder/`.
