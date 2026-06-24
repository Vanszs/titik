//! App – UI mode enum and associated state types.
//!
//! The app is always in exactly one of five modes, represented by [`Mode`]:
//!
//! | Variant          | Meaning                                       |
//! |-----------------|-----------------------------------------------|
//! | `KeyInput`       | Credentials form (api key + model)            |
//! | `SessionPicker`  | `--resume` session list with live search      |
//! | `Chat`           | Normal conversation view                      |
//! | `Settings`       | In-app `/settings` dashboard                  |
//! | `Effort`         | `/effort` reasoning-effort picker overlay     |
//!
//! Mode-specific state is stored inline in the variant so the type system
//! ensures the runtime can only access data that is relevant to the active
//! mode.  [`KeyInputForm`], [`PickerState`], [`SettingsState`], and
//! [`EffortPickerState`] live here; `Chat` carries no extra state beyond
//! `AppStateRest`.

mod key_input;
mod effort;
mod picker;
pub mod settings;
pub mod agents;

pub use agents::{AgentEditField, AgentScope, AgentSubMode, AgentsState};
pub use effort::EffortPickerState;
pub use key_input::KeyInputForm;
pub use picker::PickerState;
pub use settings::{
    SettingField, SettingsState, PICKER_MAX,
    SETTING_CATEGORIES,
};

/// The three mutually-exclusive UI modes of the application.
pub enum Mode {
    /// Credentials form: collects api key and model name before a session can
    /// start.  The inner [`KeyInputForm`] holds the in-progress field values
    /// and tracks which field is focused.
    KeyInput(KeyInputForm),
    /// `--resume` session picker: shows saved sessions and a live search bar.
    SessionPicker(PickerState),
    /// Normal chat view: messages are rendered and the user types in the
    /// input bar.  All chat-specific state lives in `AppStateRest`.
    Chat,
    /// In-app settings dashboard (`/settings`): edit per-session credentials,
    /// the session name, and the global theme/accent. The inner
    /// [`SettingsState`] holds working drafts that are applied on save.
    ///
    /// Boxed: `SettingsState` is much larger than the other variants (it carries
    /// every draft plus the path-list/picker working state), so storing it inline
    /// would bloat `Mode` everywhere. The box keeps the enum small.
    Settings(Box<SettingsState>),
    /// In-app agent definitions manager (`/agents`): create / modify / delete
    /// the `.md` frontmatter agent files. The inner [`AgentsState`] holds the
    /// loaded registry snapshot, the LIST/DETAIL cursor, the sub-mode state
    /// machine, and the per-field working drafts. Boxed to keep `Mode` small,
    /// consistent with `Settings`.
    Agents(Box<AgentsState>),
    /// Reasoning/thinking-effort picker (`/effort`): a small overlay listing the
    /// effort options the current model supports. The inner [`EffortPickerState`]
    /// holds the option list, the cursor, and a one-line capability note. Boxed
    /// to keep `Mode` small and consistent with `Settings`.
    Effort(Box<EffortPickerState>),
}
