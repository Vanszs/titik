//! Seed-conversation builder for a sub-agent.
//!
//! A sub-agent runs against its OWN isolated [`Conversation`] — it never reads or
//! references the main session's history. [`build_seed`] assembles that fresh
//! conversation from the agent's persona prompt plus optional project context,
//! then seeds the task as the first user turn.

// Inert in Stage 1: used only by the (also-inert) spawn path until a later stage
// wires the sub-agent runtime into the binary.
#![allow(dead_code)]

use crate::model::agent_def::AgentDef;
use crate::model::conversation::Conversation;

/// Build the sub-agent's isolated seed conversation.
///
/// The system message is composed, in order:
/// 1. the agent's persona (`agent.prompt`, the markdown body of its definition);
/// 2. the project's `MEMORY.md` text, under a `# Project memory` header — only
///    when non-empty;
/// 3. the awareness blurb, under a `# Project context` header — only when
///    non-empty.
///
/// The `task` is then pushed as the first (and only) user turn. The returned
/// conversation is fully self-contained: it shares nothing with the main session
/// `Conversation`, so a sub-agent can never see — or pollute — the interactive
/// chat history.
pub fn build_seed(agent: &AgentDef, awareness: &str, memory_md: &str, task: &str) -> Conversation {
    let mut system = agent.prompt.trim_end().to_string();

    // Project memory (MEMORY.md) — verbatim, under its own header. Skipped when
    // empty so a memory-less project doesn't seed a dangling header.
    let memory_md = memory_md.trim();
    if !memory_md.is_empty() {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str("# Project memory\n");
        system.push_str(memory_md);
    }

    // Awareness blurb — the project-context digest, under its own header. Also
    // skipped when empty.
    let awareness = awareness.trim();
    if !awareness.is_empty() {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str("# Project context\n");
        system.push_str(awareness);
    }

    let mut convo = Conversation::from_messages(Vec::new());
    convo.set_system(system);
    convo.push_user(task);
    convo
}
