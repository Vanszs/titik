//! Async streaming bridge: spawn / abort / finalize a request task.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::app::state::{AppState, AppStateRest};
use crate::dto::chat::ChatMessage;
use crate::service::openrouter::OpenRouterClient;

/// Finalize a finished stream: commit any buffered assistant text, clear the
/// waiting flag + task handle, set the status line. `error` is Some on stream
/// failure; a save error is surfaced only if the stream itself succeeded.
pub(super) fn finish_stream(rest: &mut AppStateRest, error: Option<String>) {
    let mut save_err = None;
    if let Some(buf) = rest.take_stream() {
        if !buf.is_empty() {
            if let Some(sess) = rest.session.as_mut() {
                sess.conversation.push_assistant(buf);
                if let Err(e) = sess.save() {
                    save_err = Some(e.to_string());
                }
            }
        }
    }
    rest.waiting = false;
    rest.current_task = None;
    rest.status = match error.or(save_err) {
        Some(e) => format!("error: {e}"),
        None => "ready".into(),
    };
}

/// Abort the in-flight task and stop listening to it: aborts the task handle,
/// drops the active receiver (so any late events from the task vanish), and
/// clears the waiting flag.
pub(super) fn abort_current(rest: &mut AppStateRest) {
    if let Some(h) = rest.current_task.take() {
        h.abort();
    }
    rest.active_rx = None;
    rest.waiting = false;
}

/// Spawn a streaming task for `history`. Opens a fresh channel, stashes the
/// receiver in state, and hands the sender to the task — so this request's
/// events are isolated from any previous one (no generation tagging needed).
pub(super) fn start_stream_task(
    history: Vec<ChatMessage>,
    state: &mut AppState,
    client: &Option<Arc<OpenRouterClient>>,
    handle: &tokio::runtime::Handle,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    state.rest.active_rx = Some(rx);
    let c = Arc::clone(client.as_ref().unwrap());
    let jh = handle.spawn(async move {
        let _ = c.stream_complete(history, tx).await;
    });
    state.rest.current_task = Some(jh.abort_handle());
}
