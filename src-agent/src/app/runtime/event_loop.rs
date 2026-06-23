//! Central event loop: drain stream events, poll terminal input, redraw.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, MouseEventKind};

use crate::app::mode::Mode;
use crate::app::state::AppState;
use crate::controller;
use crate::service::{openrouter::OpenRouterClient, StreamEvent};
use crate::view;

use super::actions::apply_action;
use super::stream::finish_stream;
use super::Term;

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
        if dirty {
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
                    StreamEvent::Done => {
                        finish_stream(&mut state.rest, None);
                        still_streaming = false;
                        break;
                    }
                    StreamEvent::Error(e) => {
                        finish_stream(&mut state.rest, Some(e));
                        still_streaming = false;
                        break;
                    }
                    StreamEvent::Compacted { summary, kept_tail } => {
                        if let Some(sess) = state.rest.session.as_mut() {
                            sess.conversation.apply_compaction(summary, kept_tail);
                            let _ = sess.save();
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
                        let action = controller::input::handle_key(state, key);
                        apply_action(action, state, client, handle)?;
                        dirty = true;
                    }
                    Event::Mouse(m) => {
                        // Wheel scrolls the chat transcript only.
                        if matches!(state.mode, Mode::Chat) {
                            match m.kind {
                                MouseEventKind::ScrollUp => {
                                    state.rest.scroll_up();
                                    dirty = true;
                                }
                                MouseEventKind::ScrollDown => {
                                    state.rest.scroll_down();
                                    dirty = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    Event::Resize(_, _) => dirty = true,
                    _ => {}
                }
            }
        }

        if state.rest.should_quit {
            break;
        }
    }
    Ok(())
}
