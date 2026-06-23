//! In-memory chat history and conversation compaction.
//!
//! `Conversation` is a thin wrapper around `Vec<ChatMessage>` that enforces
//! the invariant that `messages[0]` is always a `Role::System` message (once
//! `set_system` or `rebuild_system` has been called). All other messages are
//! user/assistant turns in chronological order.
//!
//! **Compaction** shrinks the history when it grows too large. The flow is:
//! 1. The controller calls `split_for_compaction(preserve_n)` to carve the
//!    history into two parts: an older slice to summarise and a recent tail to
//!    keep verbatim.
//! 2. The older slice is sent to the model for summarisation.
//! 3. The controller calls `apply_compaction(summary, kept_tail)` to rebuild
//!    the conversation as `[system, Assistant(summary), kept_tail…]`.
//!
//! Data flow in the broader app:
//! ```
//! keystroke -> Action -> state mutation (push_user / push_assistant)
//!          -> render (Conversation::messages())
//!          -> Session::save() -> messages.json
//! ```

use crate::dto::chat::{ChatMessage, Role, ToolCall};

/// In-memory chat history for one session.
///
/// The first element of the internal vec is always a `System` message after
/// `set_system` (or `rebuild_system`) has been called. Pushing user/assistant
/// messages always appends to the end.
pub struct Conversation {
    messages: Vec<ChatMessage>,
}

impl Conversation {
    /// Start a fresh conversation with an initial system prompt.
    #[allow(dead_code)]
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            messages: vec![ChatMessage::new(Role::System, system_prompt)],
        }
    }

    /// Wrap an existing vec verbatim (used on resume from disk). May be empty;
    /// the caller (`Session::load`) calls `rebuild_system()` immediately after,
    /// which seeds the system message via `set_system`.
    pub fn from_messages(messages: Vec<ChatMessage>) -> Self {
        Self { messages }
    }

    /// Append a user turn.
    pub fn push_user(&mut self, content: impl Into<String>) {
        self.messages.push(ChatMessage::new(Role::User, content));
    }

    /// Append an assistant turn (used for both streamed and non-streamed replies).
    pub fn push_assistant(&mut self, content: impl Into<String>) {
        self.messages.push(ChatMessage::new(Role::Assistant, content));
    }

    /// Append an assistant turn that requested tool calls. `content` is the
    /// assistant text accompanying the calls (often empty).
    pub fn push_assistant_with_tools(&mut self, content: String, tool_calls: Vec<ToolCall>) {
        self.messages
            .push(ChatMessage::assistant_with_tools(content, tool_calls));
    }

    /// Append a `tool`-role result message answering `tool_call_id`.
    pub fn push_tool(&mut self, tool_call_id: String, content: String) {
        self.messages
            .push(ChatMessage::tool_result(tool_call_id, content));
    }

    /// Borrow the full message list (system + turns). Passed directly to the
    /// wire-format `ChatRequest` without copying.
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Clone the full message list (used when saving to disk).
    pub fn history(&self) -> Vec<ChatMessage> {
        self.messages.clone()
    }

    /// Insert or replace the system message at index 0.
    ///
    /// "Absent" means the vec is empty or `messages[0].role != System`. In
    /// both cases a new `System` message is inserted at position 0. When a
    /// system message already exists its `content` is replaced in-place.
    /// This never appends a second system message.
    pub fn set_system(&mut self, content: impl Into<String>) {
        let content = content.into();
        if self
            .messages
            .first()
            .map(|m| m.role == Role::System)
            .unwrap_or(false)
        {
            // Fast path: system message is already at [0], just update it.
            self.messages[0].content = content;
        } else {
            // No system message present — prepend one.
            self.messages
                .insert(0, ChatMessage::new(Role::System, content));
        }
    }

    /// Split the conversation into two parts for compaction, skipping the
    /// system message.
    ///
    /// Given `messages = [system, m1, m2, … mN]` and `preserve_n`:
    ///
    /// - `body = messages[1..]` (all non-system messages, length `N`)
    /// - If `N <= preserve_n` there is nothing old enough to summarise:
    ///   returns `([], body)`.
    /// - Otherwise `split_at = N - preserve_n`:
    ///   - `to_summarize = body[..split_at]`  ← sent to the model as context
    ///   - `kept_tail    = body[split_at..]`  ← kept verbatim after compaction
    ///
    /// The system message is excluded from both halves; `apply_compaction`
    /// re-prepends it.
    pub fn split_for_compaction(
        &self,
        preserve_n: usize,
    ) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
        if self.messages.is_empty() {
            return (vec![], vec![]);
        }
        // Skip messages[0] (system prompt) — it is not subject to compaction.
        let body = &self.messages[1..];
        if body.len() <= preserve_n {
            // Not enough history to compact; return everything as kept_tail.
            return (vec![], body.to_vec());
        }
        let split_at = body.len() - preserve_n;
        let to_summarize = body[..split_at].to_vec();
        let kept_tail = body[split_at..].to_vec();
        (to_summarize, kept_tail)
    }

    /// Rebuild the conversation from a compaction snapshot.
    ///
    /// After this call `messages` is exactly:
    /// ```text
    /// [ system, Assistant("[summary of earlier conversation]\n<summary>"),
    ///   kept_tail[0], kept_tail[1], … ]
    /// ```
    ///
    /// The `kept_tail` is supplied by the caller (it came from
    /// `split_for_compaction`) and is NOT re-derived here. The system message
    /// is taken from `self.messages[0]`; if no system message exists yet a
    /// blank one is inserted first via `set_system`.
    pub fn apply_compaction(&mut self, summary: String, kept_tail: Vec<ChatMessage>) {
        // Guard: ensure a System message exists at [0] before we clone it.
        if !self
            .messages
            .first()
            .map(|m| m.role == Role::System)
            .unwrap_or(false)
        {
            self.set_system(String::new());
        }
        let system = self.messages[0].clone();
        let mut rebuilt = vec![
            system,
            // The summary is injected as an Assistant turn so models that
            // enforce strict user/assistant alternation don't choke on it.
            ChatMessage::new(
                Role::Assistant,
                format!("[summary of earlier conversation]\n{summary}"),
            ),
        ];
        rebuilt.extend(kept_tail);
        self.messages = rebuilt;
    }

    /// Pop all trailing `Assistant` messages (used before a resend so the
    /// model doesn't see its own previous partial reply as context).
    ///
    /// Returns the number of messages removed.
    pub fn pop_trailing_assistants(&mut self) -> usize {
        let mut removed = 0;
        while self
            .messages
            .last()
            .map(|m| m.role == Role::Assistant)
            .unwrap_or(false)
        {
            self.messages.pop();
            removed += 1;
        }
        removed
    }

    /// Return the content of the most-recent `User` message, if any.
    ///
    /// Used by the resend flow to replay the last user input.
    pub fn last_user_content(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
    }
}
