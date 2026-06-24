/// State for the `/effort` reasoning-effort picker overlay.
///
/// `options` is the capability-derived list shown to the user (e.g.
/// `["default","off","low","high"]` for an effort model, or `["off","on"]` for
/// an on/off-only one). `selected` indexes `options`; `note` is a short
/// capability line shown dimmed in the footer (e.g. why a model has no control,
/// or that capabilities couldn't be fetched). Built in the `/effort` command
/// handler; keystrokes are handled by [`controller::input::handle_effort`].
pub struct EffortPickerState {
    /// The effort options offered for the current model, in display order.
    pub options: Vec<String>,
    /// Cursor within `options`.
    pub selected: usize,
    /// One-line capability note rendered dim in the footer.
    pub note: String,
}

impl EffortPickerState {
    /// Move the cursor up one row (clamps at 0).
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the cursor down one row (clamps at the last option).
    pub fn down(&mut self) {
        if self.selected + 1 < self.options.len() {
            self.selected += 1;
        }
    }

    /// The currently highlighted option, if any.
    pub fn selected_option(&self) -> Option<&String> {
        self.options.get(self.selected)
    }
}
