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
//! | `Usage`          | `/usage` cost and token dashboard             |
//!
//! Mode-specific state is stored inline in the variant so the type system
//! ensures the runtime can only access data that is relevant to the active
//! mode.  [`KeyInputForm`], [`PickerState`], [`SettingsState`], and
//! [`EffortPickerState`] live here; `Chat` carries no extra state beyond
//! `AppStateRest`.

mod key_input;
mod effort;
mod loading;
mod picker;
pub mod settings;
pub mod agents;
pub mod editor;

pub use agents::{AgentEditField, AgentScope, AgentSubMode, AgentsState};
pub use effort::EffortPickerState;
pub use key_input::KeyInputForm;
pub use loading::{LoadingState, WarmStatus};
pub use picker::PickerState;
pub use settings::{
    filter_models, SettingField, SettingsState, PICKER_MAX,
    SETTING_CATEGORIES,
};

// ── Usage dashboard nav state ────────────────────────────────────────────────

/// Which top-level view is active in the `/usage` dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UsageView {
    /// View A: global stats across all sessions (heatmap, KPI, top models…).
    #[default]
    Global,
    /// View B: current-session detail (models used, hourly heatmap, totals).
    Session,
}

/// Date-range selection for View A's KPI strip and panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UsageRange {
    /// Data from midnight UTC today onwards.
    #[default]
    Today,
    /// Last 7 days.
    Week,
    /// Last 365 days.
    Year,
}

impl UsageRange {
    /// How far back (in seconds from now) the range extends.
    #[allow(dead_code)]
    pub fn since_secs(self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        match self {
            // Floor to midnight UTC so "today" always starts at 00:00:00.
            Self::Today => now - now % 86400,
            Self::Week  => now - 7 * 86400,
            Self::Year  => now - 365 * 86400,
        }
    }

    /// Short label shown in the range tab bar.
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Week  => "week",
            Self::Year  => "year",
        }
    }
}

/// Which metric drives the heatmap cell intensity and sparkline scaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UsageMetric {
    /// Intensity proportional to USD cost.
    #[default]
    Cost,
    /// Intensity proportional to token count (in + out).
    Tokens,
}

/// Navigation / display state for the `/usage` dashboard.
///
/// Boxed in the `Mode::Usage` variant to keep the enum small (consistent with
/// `Settings` and `Agents`).
#[derive(Debug, Clone, Default)]
pub struct UsageNavState {
    /// Which top-level view (Global / Session) is shown.
    pub view: UsageView,
    /// Active date range for View A.
    pub range: UsageRange,
    /// Metric that drives heatmap intensity and sparkline scaling.
    pub metric: UsageMetric,
}

// ── Mode enum ────────────────────────────────────────────────────────────────

/// The mutually-exclusive UI modes of the application.
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
    /// Startup warming splash: a btop-style animated loading screen shown while a
    /// returning-into-Chat session warms ASYNCHRONOUSLY (catalogue fetch + project
    /// awareness summary run as background tasks instead of blocking the UI thread
    /// before the event loop starts). The inner [`LoadingState`] tracks the three
    /// step markers + the spinner frame; the loop switches to `Chat` once the
    /// catalogue + awareness steps are terminal (or the user presses Esc to skip).
    Loading(LoadingState),
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
    /// Cost and token usage dashboard (`/usage`): full-screen Bloomberg-terminal-
    /// style view with two tabs (Global / Session), range selector (1-4), metric
    /// toggle (m), and ESC to exit. The inner [`UsageNavState`] holds the active
    /// view, range, and metric selections. Boxed to keep `Mode` small.
    Usage(Box<UsageNavState>),
}
