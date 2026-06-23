use crate::dto::chat::{ChatMessage, Role};

pub struct Conversation {
    messages: Vec<ChatMessage>,
}

impl Conversation {
    #[allow(dead_code)]
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            messages: vec![ChatMessage::new(Role::System, system_prompt)],
        }
    }

    /// Wrap an existing vec verbatim (used on resume). May be empty; the
    /// caller (Session::load) calls rebuild_system() afterward which seeds
    /// the system message via set_system.
    pub fn from_messages(messages: Vec<ChatMessage>) -> Self {
        Self { messages }
    }

    pub fn push_user(&mut self, content: impl Into<String>) {
        self.messages.push(ChatMessage::new(Role::User, content));
    }

    pub fn push_assistant(&mut self, content: impl Into<String>) {
        self.messages.push(ChatMessage::new(Role::Assistant, content));
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub fn history(&self) -> Vec<ChatMessage> {
        self.messages.clone()
    }

    /// Set the system message. "Absent" means: messages is empty OR
    /// messages[0].role != Role::System. In both cases insert a new System
    /// message at index 0. Otherwise replace messages[0].content. Never append.
    pub fn set_system(&mut self, content: impl Into<String>) {
        let content = content.into();
        if self
            .messages
            .first()
            .map(|m| m.role == Role::System)
            .unwrap_or(false)
        {
            self.messages[0].content = content;
        } else {
            self.messages
                .insert(0, ChatMessage::new(Role::System, content));
        }
    }

    /// (to_summarize, kept_tail), excluding the system message.
    pub fn split_for_compaction(
        &self,
        preserve_n: usize,
    ) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
        if self.messages.is_empty() {
            return (vec![], vec![]);
        }
        let body = &self.messages[1..];
        if body.len() <= preserve_n {
            return (vec![], body.to_vec());
        }
        let split_at = body.len() - preserve_n;
        let to_summarize = body[..split_at].to_vec();
        let kept_tail = body[split_at..].to_vec();
        (to_summarize, kept_tail)
    }

    /// Rebuild from a snapshot: messages = [system, Assistant(summary), kept_tail...].
    /// kept_tail is supplied by the caller (NOT re-derived).
    pub fn apply_compaction(&mut self, summary: String, kept_tail: Vec<ChatMessage>) {
        // Determine the system message: current messages[0] if it is System,
        // else build a fresh one via set_system first.
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
            ChatMessage::new(
                Role::Assistant,
                format!("[summary of earlier conversation]\n{summary}"),
            ),
        ];
        rebuilt.extend(kept_tail);
        self.messages = rebuilt;
    }

    /// Remove trailing Assistant messages (for resend). Returns count removed.
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

    /// Content of the last User message, if any.
    pub fn last_user_content(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
    }
}
