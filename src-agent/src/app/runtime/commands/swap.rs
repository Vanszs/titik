//! The `/swap` command: open the live-session picker to switch which running
//! session is on screen.

use anyhow::Result;

use crate::app::mode::{LiveSessionEntry, LiveSessionPicker, Mode};
use crate::app::state::AppState;

/// Handle the `/swap` command: snapshot the currently-LIVE sessions and open the
/// live-session picker over them.
///
/// Builds one [`LiveSessionEntry`] per entry in `state.rest.sessions` (its Vec
/// index, display name, and working flag), defaulting the cursor to the current
/// foreground's position so the picker opens on the session already on screen.
/// Switching is handled by the picker's Enter (see the input handler + the
/// `LiveSwitch` action); this only opens the mode. Nothing is aborted and no lock
/// is touched here.
pub(super) fn handle_swap(state: &mut AppState) -> Result<()> {
    let entries: Vec<LiveSessionEntry> = state
        .rest
        .sessions
        .iter()
        .enumerate()
        .map(|(idx, runtime)| LiveSessionEntry {
            idx,
            name: runtime
                .session
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "(no session)".to_string()),
            working: runtime.is_working(),
        })
        .collect();

    // Default the cursor to the current foreground's row.
    let selected = state.rest.foreground.min(entries.len().saturating_sub(1));

    state.mode = Mode::LiveSessionPicker(Box::new(LiveSessionPicker { entries, selected }));
    Ok(())
}
