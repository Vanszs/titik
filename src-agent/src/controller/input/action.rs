//! The [`Action`] enum: all observable effects a key-press can request from
//! the runtime.  The runtime matches on this value and performs actual state
//! mutations and I/O.

use crate::controller::command::Command;

/// All observable effects a key-press can request from the runtime.
///
/// The runtime match on this value and performs actual state mutations and I/O.
pub enum Action {
    /// Key was recognised but requires no runtime response.
    None,
    /// Exit the application cleanly.
    Quit,
    // --- Chat actions ---
    /// User confirmed a non-slash message; inner string is the trimmed input.
    Submit(String),
    /// User entered a `/slash` command; inner value is the parsed [`Command`].
    Slash(Command),
    /// Abort an in-flight API request (Ctrl+C / Esc while `waiting = true`).
    Interrupt,
    /// Re-send the last user message (Ctrl+R while idle).
    Resend,
    /// Double-Esc while idle in Chat — open the message-rewind picker. The
    /// runtime builds the [`RewindState`] from the active conversation's user
    /// messages and swaps into `Mode::MessageRewind`. A no-op when there is no
    /// session or no prior user message.
    OpenRewind,
    /// Esc/Ctrl+C in the message-rewind picker — discard it and return to Chat
    /// unchanged (the conversation is untouched).
    RewindCancel,
    /// Enter in the message-rewind picker — rewind the conversation to just
    /// before the highlighted user message and load its text into the composer.
    RewindSelect,
    /// Approve the paused risky tool call (`y` in the approval modal): run it
    /// and resume the tool-approval state machine.
    ApproveTool,
    /// Deny the paused risky tool call (`n`/Esc in the approval modal): feed
    /// `"denied by user"` back as its result and resume the machine.
    DenyTool,
    // --- KeyInput actions ---
    /// Setup wizard finished; carry the entered endpoint, api key, and model out
    /// so the runtime can build a provider-agnostic config from them.
    SaveCreds { endpoint: String, api_key: String, model: String },
    /// Esc on a credentials form that was NOT opened from the picker — return
    /// to the normal Chat view.
    CancelKeyInput,
    /// Esc from a KeyInput that was opened from the --resume picker: go back to
    /// the picker rather than pinning a no-client Chat.
    CancelKeyInputToPicker,
    /// Esc/Ctrl+C in the session picker opened via /resume (an active session
    /// exists) — discard the picker and return to the unchanged Chat. The
    /// --resume startup picker has no session, so it Quits instead.
    CancelPickerToChat,
    // --- Picker actions ---
    /// Enter on the session picker — open the highlighted session.
    PickerSelect,
    // --- Settings actions ---
    /// Esc on the settings dashboard (while navigating) — apply every draft and
    /// return to Chat. The apply path reads the drafts back out of
    /// `state.mode`, mirroring [`Action::PickerSelect`].
    SaveSettings,
    // --- Effort picker actions ---
    /// Enter on the `/effort` picker — store the chosen effort, rebuild the
    /// client so it takes effect, and return to Chat. Inner string is the chosen
    /// option (`"default"` stores `""`).
    SaveEffort(String),
    /// Esc on the `/effort` picker — discard the selection and return to Chat.
    EffortCancel,
    // --- Agents dashboard actions ---
    /// Confirm CREATE: write a new agent from the drafts, reload, back to Browse.
    CreateAgent,
    /// Confirm EDIT: overwrite the selected agent from the drafts, reload, back
    /// to Browse.
    SaveAgent,
    /// Confirm DELETE: remove the selected file-backed agent, reload, back to
    /// Browse.
    DeleteAgent,
    /// Esc from the agents dashboard (Browse, LIST focused) — discard any drafts
    /// and return to Chat.
    CloseAgents,
    /// Fetch the provider-endpoint list for the given model id (the inner
    /// `String`) on a background task. Emitted by the model modal when an
    /// OpenRouter model is selected (search) or an existing model is opened for
    /// edit; the modal's loading flags are already set by the caller. The
    /// runtime opens a fresh `endpoints_rx` channel and spawns the fetch.
    FetchModelEndpoints(String),
    // --- Usage dashboard actions ---
    /// Esc on the usage dashboard — return to Chat.
    CloseUsage,
    // --- Loading splash actions ---
    /// Esc on the startup loading splash — skip the remaining warm steps and drop
    /// straight into Chat. The background warm tasks keep running; their results
    /// still populate `state.rest.*` via the `warm_rx` drain. The handler already
    /// marked any non-terminal step `Skipped` for correctness; the runtime just
    /// swaps the mode to `Chat`.
    SkipLoading,
}
