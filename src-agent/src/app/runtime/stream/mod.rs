//! Async streaming bridge: spawn / abort / finalize a request task.

mod turn;
mod tools;
mod spawn;
mod run;

pub(super) use turn::{finish_stream, advance_turn};
pub(super) use tools::{process_tools, run_tool};
pub(super) use run::{abort_current, start_stream_task};
pub(crate) use tools::resume_after_subagents;
pub(crate) use spawn::{spawn_or_queue, try_start_pending, SpawnOutcome};
#[allow(unused_imports)]
pub(crate) use spawn::spawn_task;

/// Hard cap on tool-call rounds within a single user turn. Once exceeded the
/// turn is stopped so a misbehaving model can't loop indefinitely.
pub(super) const MAX_AGENT_STEPS: usize = 40;

/// Pick the assistant message content + display-reasoning for a FINAL turn.
/// Normally content is the answer and `reasoning` rides along (rendered gray).
/// But when the model left content empty and streamed its answer into the
/// reasoning channel (e.g. deepseek-v4-flash with reasoning on), promote the
/// reasoning to BE the content so it shows in the foreground and persists.
/// Returns (content, reasoning_to_attach). Empty content with no reasoning -> ("", None).
pub(super) fn final_answer(content: String, reasoning: Option<String>) -> (String, Option<String>) {
    if content.trim().is_empty() {
        match reasoning {
            Some(r) if !r.trim().is_empty() => (r, None), // reasoning becomes the answer
            _ => (String::new(), None),
        }
    } else {
        (content, reasoning) // normal: content is answer, reasoning rendered gray
    }
}
