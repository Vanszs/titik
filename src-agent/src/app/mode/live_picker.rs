//! State for the LIVE-session picker (`/swap`, `Mode::LiveSessionPicker`).
//!
//! Unlike the `--resume` [`super::PickerState`] (which lists saved sessions on
//! disk), this lists the currently-LIVE sessions held in
//! `AppStateRest::sessions` — each one is a running [`crate::app::state::SessionRuntime`]
//! that keeps streaming / running tools in the background. Selecting one (Enter)
//! switches the foreground to it WITHOUT aborting anything or touching any lock.
//!
//! It is a fixed snapshot built when `/swap` opens: `entries` carries each live
//! session's Vec index, display name, and working flag at that moment. (A session
//! finishing while the picker is open just means a stale `working` dot until the
//! next open — harmless.) Selection state lives here; keystroke handling lives in
//! [`crate::controller::input::handle_live_picker`].

/// One row in the live-session picker: a snapshot of a single live session.
pub struct LiveSessionEntry {
    /// Index of this session in `AppStateRest::sessions`. Carried out on Enter so
    /// the switch sets `foreground = idx` directly. Stable for this stage —
    /// `sessions` is only ever appended to (in `/new`), never reordered/removed —
    /// so the index stays valid for the picker's lifetime.
    pub idx: usize,
    /// Display name of the session (its [`crate::model::session::Session::name`]),
    /// or a placeholder when the slot has no session yet.
    pub name: String,
    /// Whether the session had work in flight when the snapshot was taken
    /// (`SessionRuntime::is_working()`), shown as a ● working / ○ ready marker.
    pub working: bool,
}

/// State for the live-session picker.
///
/// `entries` is the snapshot of live sessions (in `sessions` order); `selected`
/// is an index into `entries`, defaulted to the current foreground's position so
/// the picker opens on the session already on screen. The list is not searchable
/// — it is a short navigable list (one row per live session).
pub struct LiveSessionPicker {
    /// One entry per live session, in `AppStateRest::sessions` order.
    pub entries: Vec<LiveSessionEntry>,
    /// Cursor position within `entries`.
    pub selected: usize,
}

impl LiveSessionPicker {
    /// Move the cursor up one row (clamps at 0).
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the cursor down one row (clamps at the last entry).
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    /// Return the currently highlighted entry, or `None` if the list is empty.
    /// Read by the input handler on Enter to resolve the target session index.
    pub fn selected_entry(&self) -> Option<&LiveSessionEntry> {
        self.entries.get(self.selected)
    }
}
