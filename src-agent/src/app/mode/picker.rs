//! State for the `--resume` session picker.
//!
//! Filtering and selection state live in [`PickerState`]; keystroke handling
//! lives in [`controller::input`].

use crate::model::store::SessionMeta;

/// State for the `--resume` session picker.
///
/// `all` holds every discovered session; `filtered_idx` is a subset of
/// indices into `all` that match the current `query`.  `selected` is an
/// index into `filtered_idx` (not into `all`).
pub struct PickerState {
    /// The user's live search string (updated on every keypress).
    pub query: String,
    /// All available sessions, unfiltered, in discovery order.
    pub all: Vec<SessionMeta>,
    /// Indices into `all` of sessions that match the current `query`.
    /// Empty query → all sessions included (same order as `all`).
    pub filtered_idx: Vec<usize>,
    /// Cursor position within `filtered_idx` (not within `all`).
    pub selected: usize,
}

impl PickerState {
    /// Initialise the picker with all known sessions and run the first filter
    /// pass (which with an empty query just includes everything).
    pub fn new(all: Vec<SessionMeta>) -> Self {
        let mut s = Self {
            query: String::new(),
            all,
            filtered_idx: vec![],
            selected: 0,
        };
        s.refilter();
        s
    }

    /// Rebuild `filtered_idx` from `all` using the current `query`.
    ///
    /// Matching is case-insensitive substring search on both the session name
    /// and session id.  After filtering, `selected` is clamped to the last
    /// valid index so it never points outside the filtered list — for example
    /// when a filter narrows the list below the previous cursor position.
    pub fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered_idx = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                q.is_empty()
                    || m.name.to_lowercase().contains(&q)
                    || m.id.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();

        // Clamp `selected` so it remains a valid index after the list shrinks.
        // `saturating_sub(1)` handles the empty-list case (gives 0, which is
        // the only safe value when `filtered_idx` is empty).
        if self.selected >= self.filtered_idx.len() {
            self.selected = self.filtered_idx.len().saturating_sub(1);
        }
    }

    /// Move the cursor up one row (clamps at 0).
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the cursor down one row (clamps at the last filtered entry).
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.filtered_idx.len() {
            self.selected += 1;
        }
    }

    /// Return a reference to the metadata of the currently highlighted session,
    /// or `None` if the filtered list is empty.
    pub fn selected_meta(&self) -> Option<&SessionMeta> {
        self.filtered_idx
            .get(self.selected)
            .and_then(|&i| self.all.get(i))
    }
}
