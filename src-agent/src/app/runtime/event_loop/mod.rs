//! Central event loop: drain stream events, poll terminal input, redraw.

mod drains;
mod sessions;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, MouseEventKind};

use crate::app::mode::{Mode, WarmStatus};
use crate::app::state::AppState;
use crate::controller;
use crate::service::{openrouter::OpenRouterClient, StreamEvent, WarmEvent};
use crate::view;

use super::actions::apply_action;
use super::Term;

use drains::{apply_compaction_result, enter_select, exit_select};
use sessions::service_all_sessions;

/// Minimum on-screen duration for the `/compact` animation. Cosmetic and short:
/// a fast compaction is held this long (via a deferred apply) so the spinner +
/// progress bar don't merely flash. Deliberately ~1s — long enough to read, not
/// long enough to feel like a stall.
pub(super) const MIN_COMPACT_ANIM: Duration = Duration::from_millis(1000);

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

        // 1. Service EVERY session: drain each session's stream events, deferred
        //    tool-task results, and sub-agent channels, and advance its turn state.
        //    Render-agnostic + foreground-independent so a background session keeps
        //    streaming + running tools while a different one is on screen. This
        //    stage there is exactly ONE session, so this is the same set of
        //    mutations the old inline foreground-only drains performed. The
        //    foreground-only / global drains (harness verdict, endpoints, warm,
        //    clipboard, splash, input, redraw) stay below.
        if service_all_sessions(state, client, handle) {
            dirty = true;
        }

        // 1b. Drain the advisory prompt-classifier (PC) channel. This is fully
        //     independent of streaming: a BLOCK verdict only raises a toast; the
        //     turn already proceeded and is never cancelled here. Take() the
        //     receiver so the match can mutate state.rest; put it back unless the
        //     PC task has finished (channel closed) or delivered its verdict.
        if let Some(mut hrx) = state.rest.fg_mut().harness_rx.take() {
            let mut keep = true;
            while let Ok(event) = hrx.try_recv() {
                if let StreamEvent::HarnessVerdict { allow, reason } = event {
                    if !allow {
                        let reason = if reason.is_empty() { "flagged".into() } else { reason };
                        state.rest.set_toast(format!("harness flagged: {reason}"));
                        dirty = true;
                    }
                    // One verdict per turn; stop listening on this channel.
                    keep = false;
                    break;
                }
            }
            if keep {
                state.rest.fg_mut().harness_rx = Some(hrx);
            }
        }

        // 1b-2. Drain the per-model provider-endpoints channel. Fully independent
        //       of streaming and the harness channel: the background fetch sends
        //       exactly one EndpointsLoaded / EndpointsError, which is folded into
        //       the open model modal — but ONLY when its `model_id` still matches
        //       the modal's `endpoints_for` (the stale-guard, so a rapid
        //       re-selection can't show a previous model's providers). Take() the
        //       receiver so the match can mutate the mode; put it back unless the
        //       fetch resolved (or the channel closed).
        if let Some(mut erx) = state.rest.endpoints_rx.take() {
            let mut keep = true;
            while let Ok(ev) = erx.try_recv() {
                match ev {
                    StreamEvent::EndpointsLoaded { model_id, endpoints } => {
                        if let Mode::Settings(s) = &mut state.mode {
                            if let Some(m) = s.model_modal.as_mut() {
                                if m.endpoints_for.as_deref() == Some(model_id.as_str()) {
                                    m.endpoints = Some(endpoints);
                                    m.endpoints_loading = false;
                                }
                            }
                        }
                        dirty = true;
                        keep = false;
                    }
                    StreamEvent::EndpointsError { model_id, .. } => {
                        if let Mode::Settings(s) = &mut state.mode {
                            if let Some(m) = s.model_modal.as_mut() {
                                if m.endpoints_for.as_deref() == Some(model_id.as_str()) {
                                    // Empty list => "no providers found" display.
                                    m.endpoints = Some(Vec::new());
                                    m.endpoints_loading = false;
                                }
                            }
                        }
                        dirty = true;
                        keep = false;
                    }
                    _ => {}
                }
            }
            if keep {
                state.rest.endpoints_rx = Some(erx);
            }
        }

        // 1b-3. Drain the startup-warming channel. Fully independent of streaming:
        //        the background catalogue + awareness tasks each send one
        //        [`WarmEvent`]. ALWAYS fold the result into `state.rest.*` (the
        //        cache / summary) regardless of the current mode — a result that
        //        lands AFTER an Esc-to-chat must still populate them — and update
        //        the live `LoadingState` step marker only while still in
        //        `Mode::Loading`. Take() the receiver so the arms can mutate the
        //        mode + rest; put it back unless the channel has closed (both warm
        //        tasks finished and dropped their senders → `Disconnected`).
        if let Some(mut wrx) = state.rest.warm_rx.take() {
            let mut keep = true;
            loop {
                match wrx.try_recv() {
                    Ok(WarmEvent::WarmCatalogue { endpoint, models }) => {
                        // Key the on-demand cache to the endpoint it was fetched
                        // for; the omnisearch filters locally only while
                        // `models_cache_endpoint` matches the active endpoint.
                        state.rest.models_cache = Some(models);
                        state.rest.models_cache_endpoint = Some(endpoint.clone());
                        // Clear the in-flight guard for this endpoint so a later
                        // endpoint change can fetch again.
                        if state.rest.catalogue_fetching.as_deref() == Some(endpoint.as_str()) {
                            state.rest.catalogue_fetching = None;
                        }
                        dirty = true;
                    }
                    Ok(WarmEvent::WarmCatalogueFailed { endpoint }) => {
                        // TERMINAL empty result for this endpoint: record an empty
                        // catalogue keyed to it so the omnisearch degrades to manual
                        // model-id entry and does NOT retry in a loop (the
                        // request_catalogue no-op guard sees a matching endpoint).
                        state.rest.models_cache = Some(Vec::new());
                        state.rest.models_cache_endpoint = Some(endpoint.clone());
                        if state.rest.catalogue_fetching.as_deref() == Some(endpoint.as_str()) {
                            state.rest.catalogue_fetching = None;
                        }
                        dirty = true;
                    }
                    Ok(WarmEvent::WarmAwareness(summary)) => {
                        let had = summary.is_some();
                        // Always populate the summary (appended to the system
                        // message on every request), even if we've already skipped
                        // to chat.
                        state.rest.fg_mut().awareness_summary = summary;
                        if let Mode::Loading(s) = &mut state.mode {
                            // Some → ready; None → "no docs" (treated as a benign
                            // terminal Done detail, not a hard failure).
                            s.awareness = if had {
                                WarmStatus::Done("ready".into())
                            } else {
                                WarmStatus::Done("no docs".into())
                            };
                        }
                        dirty = true;
                    }
                    // Channel drained for now: keep listening on later ticks.
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    // Both warm tasks finished and dropped their senders: done.
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        keep = false;
                        break;
                    }
                }
            }
            if keep {
                state.rest.warm_rx = Some(wrx);
            }
        }

        // 1b-3b. Fire a DEBOUNCED, on-demand model-catalogue fetch. The model
        //        omnisearch arms `catalogue_pending` (via `request_catalogue`) on
        //        each keystroke / provider change, pushing `due` ~300ms forward so a
        //        typing burst collapses into one request. Fire here — where `handle`
        //        + `client` are in scope — once `due` passes and nothing is already
        //        in flight. Reuse the shared `warm_rx` channel (no new channel): the
        //        drain above folds the result into the per-endpoint cache. On
        //        failure send `WarmCatalogueFailed { endpoint }` so the drain records
        //        a terminal empty result (no infinite re-fetch on a dead endpoint).
        if let Some(pending) = state.rest.catalogue_pending.as_ref() {
            if state.rest.catalogue_fetching.is_none()
                && std::time::Instant::now() >= pending.due
            {
                // Take the pending request and mark its endpoint in-flight.
                let pending = state.rest.catalogue_pending.take().unwrap();
                let endpoint = pending.endpoint;
                let api_key = pending.api_key;
                state.rest.catalogue_fetching = Some(endpoint.clone());
                // Open a fresh warm channel for this fetch and stash its receiver.
                // Senders aren't stored in state (only the receiver), so this is the
                // only way to obtain one. This is safe wrt the awareness warm task:
                // the omnisearch (the sole `request_catalogue` caller) only runs in
                // Chat-mode modals / the first-run wizard, by which point the startup
                // awareness task has already resolved + closed its channel — so no
                // live awareness send can be stranded on a replaced receiver.
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                state.rest.warm_rx = Some(rx);
                // Reuse the pinned client, or build a keyless one (the first-run
                // wizard fetches before any client is pinned — `Conn` carries the
                // endpoint+key, so a keyless client is enough). The fetch is just
                // `GET {endpoint}/models`; on error send WarmCatalogueFailed so the
                // drain records a terminal empty result (no infinite re-fetch).
                let c = match client.as_ref() {
                    Some(c) => Arc::clone(c),
                    None => super::build_client(),
                };
                handle.spawn(async move {
                    let conn = crate::service::openrouter::Conn {
                        endpoint: &endpoint,
                        api_key: &api_key,
                    };
                    let ev = match c.list_models(conn).await {
                        Ok(models) => WarmEvent::WarmCatalogue { endpoint, models },
                        Err(_) => WarmEvent::WarmCatalogueFailed { endpoint },
                    };
                    // A dropped receiver (app closing) makes this a no-op.
                    let _ = tx.send(ev);
                });
                dirty = true;
            }
        }

        // 1b-3d. Drain the clipboard-image fetch result (Ctrl+V). The background
        //        thread sends Ok(bytes) (PNG data) or Err(reason) (tool absent / no image).
        //        On Ok: ingest into the session images dir + insert marker. On Err: toast.
        //        One send per Ctrl+V; clear the receiver once drained.
        if let Some(rx) = state.rest.clipboard_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(bytes)) => {
                    // Ingest the bytes; basename "pasted.png" + explicit png mime.
                    let attached = state.rest.try_attach_image_bytes(bytes, "image/png", "pasted.png");
                    if attached {
                        state.rest.set_toast_info("image attached from clipboard".to_string());
                    } else {
                        state.rest.set_toast("clipboard image: no active session or ingest failed".to_string());
                    }
                    state.rest.clipboard_rx = None;
                    dirty = true;
                }
                Ok(Err(reason)) => {
                    state.rest.set_toast(format!("clipboard image: {reason}"));
                    state.rest.clipboard_rx = None;
                    dirty = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still waiting — keep the receiver for the next tick.
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Thread exited without sending (shouldn't happen, but clean up).
                    state.rest.clipboard_rx = None;
                    dirty = true;
                }
            }
        }

        // 1b-4. Loading splash: workspace step, transition, and animation. While in
        //        `Mode::Loading` the splash is driven entirely from the loop tick.
        if let Mode::Loading(s) = &mut state.mode {
            // Workspace step: mark Done once the background reindex has SETTLED
            // (indexing flag cleared). Poll the cache readiness each tick; this
            // never gates the transition (a slow reindex must not hold up chat).
            if matches!(s.workspace, WarmStatus::Running) {
                let settled = state
                    .rest
                    .fg()
                    .dir_cache
                    .read()
                    .map(|c| !c.indexing)
                    .unwrap_or(false);
                if settled {
                    s.workspace = WarmStatus::Done(String::new());
                }
            }
            // TRANSITION: once the catalogue + awareness steps are both terminal
            // (Done / Skipped / Failed) switch into Chat. The session/chat state was
            // already set up by the activation path; we only swap the mode. The
            // workspace step is intentionally excluded from this gate.
            if s.ready_to_enter() {
                state.mode = Mode::Chat;
                dirty = true;
            } else {
                // ANIMATION: still loading — advance the spinner and force a redraw
                // each tick so the braille frames actually cycle. Paired with the
                // fast (8ms) poll cadence below (which also wakes on `Mode::Loading`)
                // so the loop never idle-sleeps the spinner.
                s.frame = s.frame.wrapping_add(1);
                dirty = true;
            }
        }

        // 1c. Deferred compaction apply. A fast compaction stashes its result and
        //     an `apply_at` instant so the animation holds for a short minimum
        //     (cosmetic). Apply once that instant passes — driven by the loop tick,
        //     never by sleeping, so input/animation stay responsive meanwhile.
        if let Some(apply_at) = state.rest.compact_apply_at {
            if std::time::Instant::now() >= apply_at {
                if let Some((summary, kept_tail)) = state.rest.compact_pending.take() {
                    apply_compaction_result(state, client, handle, summary, kept_tail);
                }
                state.rest.compact_apply_at = None;
                dirty = true;
            }
        }

        // When a background reindex has SETTLED (not indexing), warn once about
        // any workspace root missing on disk. Keyed on the missing set CHANGING
        // vs what we last warned, so it fires exactly once per change and does
        // not depend on catching the brief indexing=true window (an all-missing
        // reindex can finish before the loop ever observes it).
        let (indexing_now, missing_now) = match state.rest.fg().dir_cache.read() {
            Ok(c) => (c.indexing, c.missing_roots.clone()),
            Err(_) => (true, state.rest.warned_missing_roots.clone()),
        };
        if !indexing_now && missing_now != state.rest.warned_missing_roots {
            if !missing_now.is_empty() {
                state.rest.set_toast_info(format!(
                    "workspace root(s) not found on disk:\n{}\nfix the path in /settings",
                    missing_now.join("\n")
                ));
                dirty = true;
            }
            state.rest.warned_missing_roots = missing_now;
        }

        // Status-line "comet" activity clock. Shimmer is active whenever the app
        // is in a WORKING wait that isn't paused on a y/n approval. Reconcile
        // `work_since` against that on the rising/falling edge here (the single
        // place that sees the settled `waiting`/`awaiting_approval` for the tick),
        // rather than threading set/clear through every scattered mutation site:
        //  - rising edge (active && None)   → stamp `now` so the elapsed counter
        //    and the travelling head start from this moment.
        //  - falling edge (!active && Some) → clear it; idle / approval renders the
        //    status statically with no comet and no timer.
        let shimmer_active = state.rest.fg().waiting && !state.rest.fg().awaiting_approval;
        match (shimmer_active, state.rest.work_since.is_some()) {
            (true, false) => state.rest.work_since = Some(std::time::Instant::now()),
            (false, true) => state.rest.work_since = None,
            _ => {}
        }

        // While a compaction animation is in flight, mark every tick dirty so the
        // spinner/elapsed/bar actually advance (rendering is otherwise only
        // event-driven). The 8ms `waiting` poll above sets the frame cadence.
        // The same applies while the comet shimmer is active: it must keep
        // travelling even when NO stream events arrive (first-token latency, tool
        // exec, the summarizer fold), so force a redraw each tick then too.
        // Similarly, while any sub-agent is running (background `/task` agents
        // that don't set `waiting`), force redraws so the in-chat spinner animates.
        let has_running_subagents = state
            .rest
            .fg()
            .subagents
            .iter()
            .any(|s| matches!(s.status, crate::app::subagent::SubAgentStatus::Running));
        if state.rest.compact_anim_start.is_some() || shimmer_active || has_running_subagents {
            dirty = true;
        }

        // 2. Input poll cadence. While WORKING (waiting), poll fast so two things
        //    stay smooth: tokens flush at >=60fps when a stream is live, and the
        //    comet redraws at ~12fps (80ms) even when nothing streams (the 8ms
        //    poll is the upper bound on the redraw interval the comet needs). Idle
        //    falls back to 100ms (poll still wakes instantly on a keypress, so
        //    typing latency is 0) so a fully idle UI never busy-spins. Drain EVERY
        //    buffered event each tick so paste / fast typing don't lag.
        // Also poll fast while the loading splash is up so its braille spinner
        // animates smoothly (the per-tick `frame`++ above needs the loop to wake
        // at the fast cadence, not idle-sleep for 100ms between frames). And while a
        // debounced catalogue fetch is pending, so its ~300ms `due` fires promptly
        // rather than waiting out a 100ms idle sleep (treat it like the splash).
        let timeout = if state.rest.fg().waiting
            || state.rest.catalogue_pending.is_some()
            || matches!(state.mode, Mode::Loading(_))
            || has_running_subagents
        {
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
