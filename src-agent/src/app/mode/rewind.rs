//! State for the message-rewind picker (double-Esc "edit a previous message").
//!
//! Lists the conversation's prior USER messages, NEWEST-FIRST, so the top row
//! is the most-recent message. Selecting one (Enter) rewinds the conversation
//! to just before it and loads its text into the composer. Selection state lives
//! here; keystroke handling lives in [`controller::input::handle_rewind`].

/// A single rewindable user message: its index in the conversation's message
/// vec (so truncation knows exactly where to cut) and a clone of its text.
pub struct RewindEntry {
    /// Index of this message in `Conversation::messages()` (the vec position).
    /// Truncation keeps `messages[0..idx]`, dropping this message and all after.
    /// Carried out on Enter so the runtime knows the exact cut position.
    pub vec_index: usize,
    /// The user message's text content, shown (truncated) in the list and
    /// loaded verbatim into the composer on select.
    pub content: String,
}

/// State for the message-rewind picker.
///
/// `entries` holds the conversation's user messages NEWEST-FIRST (entry 0 is the
/// most-recent user message). `selected` is an index into `entries`. The list is
/// not searchable — it is just a navigable list.
pub struct RewindState {
    /// User messages, newest-first. Always non-empty when this mode is active
    /// (the open path refuses to enter with zero user messages).
    pub entries: Vec<RewindEntry>,
    /// Cursor position within `entries`.
    pub selected: usize,
}

impl RewindState {
    /// Build the picker from a conversation's messages, keeping only `User`-role
    /// turns and reversing so the newest is first. Returns `None` when there are
    /// no user messages (nothing to rewind to).
    pub fn from_messages(messages: &[crate::dto::chat::ChatMessage]) -> Option<Self> {
        let mut entries: Vec<RewindEntry> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == crate::dto::chat::Role::User)
            .map(|(idx, m)| RewindEntry {
                vec_index: idx,
                content: m.content.clone(),
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        // Newest user message first (top of the list).
        entries.reverse();
        Some(Self {
            entries,
            selected: 0,
        })
    }

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

    /// Return a reference to the currently highlighted entry, or `None` if the
    /// list is somehow empty. Read by the input handler on Enter to resolve the
    /// rewind target's vec index.
    pub fn selected_entry(&self) -> Option<&RewindEntry> {
        self.entries.get(self.selected)
    }
}
