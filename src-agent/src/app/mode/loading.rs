//! App – startup LOADING splash state (Loading mode).
//!
//! Shown while a returning-into-Chat session warms ASYNCHRONOUSLY: the catalogue
//! fetch + the project-docs awareness summary run as background tasks (see the
//! non-blocking `runtime::warm_session` refactor) instead of blocking the UI
//! thread before the event loop starts. This state drives a btop-style animated
//! splash (see `view::loading`) with a per-step status marker and a skip.
//!
//! Lifecycle: built by `warm_session` with each step set to `Running`/`Skipped`
//! per what is needed/enabled/routable; the event-loop drain flips a step to
//! `Done`/`Failed` as each warm task reports; once `catalogue` AND `awareness`
//! are both terminal the loop switches `Mode::Chat`. `Esc` (handled in
//! `controller::input::handle_loading`) marks any non-terminal step `Skipped` and
//! transitions immediately — the background tasks keep running and still populate
//! `AppStateRest` via the drain.

/// Per-step warming status for the loading splash.
///
/// `Done` carries a short human detail (e.g. `"412 models"`, `"ready"`, `"no
/// docs"`) rendered dim next to the marker. `Skipped`/`Failed` are terminal
/// outcomes that, like `Done`, no longer gate the transition to Chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmStatus {
    /// Not started (and won't be, for this run) — rendered as a dim `·`.
    Pending,
    /// In flight — rendered as an animated braille spinner.
    Running,
    /// Finished successfully — rendered as `●` plus the carried detail.
    Done(String),
    /// Skipped (not needed, or the user pressed Esc before it finished).
    Skipped,
    /// Attempted but failed / returned nothing usable.
    Failed,
}

impl WarmStatus {
    /// Whether this step has reached a final outcome. The transition to Chat
    /// waits until the catalogue + awareness steps are both terminal; the
    /// workspace step never gates the transition.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, WarmStatus::Running)
    }
}

/// State backing the [`crate::app::mode::Mode::Loading`] splash.
///
/// `started` stamps the splash open time (the footer shows elapsed since it);
/// `frame` is bumped once per draw tick so the spinner animates. The three
/// `WarmStatus` fields track the three rows shown on screen.
#[derive(Debug, Clone)]
pub struct LoadingState {
    /// When the splash opened — drives the footer's `elapsed` readout.
    pub started: std::time::Instant,
    /// Spinner frame counter, incremented each draw tick (see the event loop).
    pub frame: u64,
    /// Workspace dir index (does NOT gate the transition).
    pub workspace: WarmStatus,
    /// Model catalogue fetch.
    pub catalogue: WarmStatus,
    /// Project-docs awareness summary (the slow one — the whole reason for Esc).
    pub awareness: WarmStatus,
}

impl LoadingState {
    /// True once both gating steps (catalogue + awareness) have reached a final
    /// outcome, so the loop may switch to Chat. The workspace step is excluded
    /// by design — a slow reindex must not hold the chat hostage.
    pub fn ready_to_enter(&self) -> bool {
        self.catalogue.is_terminal() && self.awareness.is_terminal()
    }
}
