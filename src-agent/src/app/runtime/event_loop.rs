//! Central event loop: drain stream events, poll terminal input, redraw.

use std::io::{stdout, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

use crate::app::mode::Mode;
use crate::app::state::AppState;
use crate::controller;
use crate::dto::chat::Role;
use crate::service::{openrouter::OpenRouterClient, StreamEvent};
use crate::view;

use super::actions::apply_action;
use super::stream::{advance_turn, finish_stream};
use super::Term;

/// Leave the alternate screen + disable mouse capture, then print the full
/// conversation as plain text so the user can select/copy with the terminal's
/// native selection. Raw mode stays on (we read a single key to return), so
/// lines are terminated with `\r\n`.
fn enter_select(rest: &crate::app::state::AppStateRest) -> Result<()> {
    execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    let mut out = stdout();
    if let Some(sess) = rest.session.as_ref() {
        for m in sess.conversation.messages() {
            let label = match m.role {
                Role::System | Role::Tool => continue,
                Role::User => "you",
                Role::Assistant => "ai",
            };
            write!(out, "\r\n{label}:\r\n")?;
            for line in m.content.split('\n') {
                write!(out, "{line}\r\n")?;
            }
        }
    }
    write!(out, "\r\n-- copy with your mouse, then press any key to return --\r\n")?;
    out.flush()?;
    Ok(())
}

/// Re-enter the alternate screen + mouse capture and force a full repaint.
fn exit_select(terminal: &mut Term) -> Result<()> {
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;
    Ok(())
}

/// The central event loop. Each tick: redraw if dirty, drain the active
/// request's events, then drain all buffered terminal input. Rendering is
/// dirty-flagged and polling is adaptive (8ms streaming / 100ms idle) so an
/// idle UI is effectively free while streaming stays at >=60fps.
pub(super) fn run_loop(
    terminal: &mut Term,
    state: &mut AppState,
    handle: &tokio::runtime::Handle,
    client: &mut Option<Arc<OpenRouterClient>>,
) -> Result<()> {
    let mut dirty = true; // paint once on entry
    loop {
        // Perform a pending /select hand-off: drop to the normal terminal and
        // dump the conversation, then suppress TUI painting until a key returns.
        if state.rest.select_pending {
            state.rest.select_pending = false;
            enter_select(&state.rest)?;
            state.rest.select_active = true;
        }

        if dirty && !state.rest.select_active {
            terminal.draw(|f| view::draw(f, state))?;
            dirty = false;
        }

        // 1. Drain the active stream's events. take() the receiver so the match
        //    arms can mutate other fields of state.rest without a borrow
        //    conflict; put it back if the stream is still open.
        if let Some(mut rx) = state.rest.active_rx.take() {
            let mut still_streaming = true;
            while let Ok(event) = rx.try_recv() {
                dirty = true;
                match event {
                    StreamEvent::Token(t) => {
                        state.rest.append_token(&t);
                        state.rest.status = "streaming".into();
                    }
                    StreamEvent::Usage { prompt_tokens, completion_tokens, cost } => {
                        // Stash for the assistant-commit step; do NOT break —
                        // usage arrives just before Done.
                        state.rest.pending_usage = Some((prompt_tokens, completion_tokens, cost));
                    }
                    StreamEvent::ToolCalls(calls) => {
                        // Stash the requested tool calls; do NOT break — Done
                        // follows and `advance_turn` consumes them there.
                        state.rest.pending_tool_calls = calls;
                    }
                    StreamEvent::Done => {
                        // Drive the turn: commit the assistant message and either
                        // end the turn or run tools + continue (which spawns the
                        // next task into a fresh active_rx).
                        advance_turn(state, client, handle);
                        still_streaming = false;
                        break;
                    }
                    StreamEvent::Error(e) => {
                        // Surface the error and halt the whole turn (drop any
                        // half-stashed tool calls / step count / approval machine).
                        finish_stream(&mut state.rest, Some(e));
                        state.rest.agent_steps = 0;
                        state.rest.pending_tool_calls.clear();
                        state.rest.awaiting_approval = false;
                        state.rest.tool_idx = 0;
                        state.rest.tool_results.clear();
                        still_streaming = false;
                        break;
                    }
                    StreamEvent::Compacted { summary, kept_tail } => {
                        if let Some(sess) = state.rest.session.as_mut() {
                            sess.conversation.apply_compaction(summary, kept_tail);
                            sess.rebuild_system();
                            let _ = sess.save();
                        }
                        // Refresh the project-awareness summary post-compaction:
                        // the project is often better understood after a compact,
                        // and this also satisfies the "applies on compaction"
                        // requirement. Best-effort; gated by `awareness_enabled`
                        // inside `summarize`. Clone the inputs out first so the
                        // `block_on` doesn't hold a borrow of `state.rest`.
                        let aware_inputs = match (client.as_ref(), state.rest.session.as_ref()) {
                            (Some(c), Some(sess)) if sess.settings.awareness_enabled => Some((
                                Arc::clone(c),
                                sess.settings.clone(),
                                sess.workdir(),
                            )),
                            _ => None,
                        };
                        if let Some((c, settings, workdir)) = aware_inputs {
                            let summary = handle.block_on(crate::app::awareness::summarize(
                                &c, &settings, &workdir,
                            ));
                            state.rest.awareness_summary = summary;
                        }
                        state.rest.waiting = false;
                        state.rest.current_task = None;
                        state.rest.status = "ready".into();
                        still_streaming = false;
                        break;
                    }
                }
            }
            if still_streaming {
                state.rest.active_rx = Some(rx);
            }
        }

        // 2. Input. 8ms poll while streaming so tokens flush at >=60fps; 100ms
        //    idle (poll still wakes instantly on a keypress, so typing latency
        //    is 0). Drain EVERY buffered event each tick so paste / fast typing
        //    don't lag.
        let timeout = if state.rest.waiting {
            Duration::from_millis(8)
        } else {
            Duration::from_millis(100)
        };
        if event::poll(timeout)? {
            while event::poll(Duration::ZERO)? {
                match event::read()? {
                    Event::Key(key) => {
                        if state.rest.select_active {
                            // Any key returns from /select copy mode.
                            exit_select(terminal)?;
                            state.rest.select_active = false;
                            dirty = true;
                        } else {
                            let action = controller::input::handle_key(state, key);
                            apply_action(action, state, client, handle)?;
                            dirty = true;
                        }
                    }
                    Event::Mouse(m) => {
                        // Wheel scrolls the chat transcript only.
                        if matches!(state.mode, Mode::Chat) {
                            match m.kind {
                                MouseEventKind::ScrollUp => {
                                    for _ in 0..3 { state.rest.scroll_up(); }
                                    dirty = true;
                                }
                                MouseEventKind::ScrollDown => {
                                    for _ in 0..3 { state.rest.scroll_down(); }
                                    dirty = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    Event::Resize(_, _) => dirty = true,
                    Event::Paste(text) => {
                        if !state.rest.select_active {
                            controller::input::handle_paste(state, &text);
                            dirty = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        // Auto-dismiss an expired error toast.
        if state.rest.tick_toast() {
            dirty = true;
        }

        if state.rest.should_quit {
            break;
        }
    }
    Ok(())
}
