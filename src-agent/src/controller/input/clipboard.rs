//! Clipboard-image paste (Linux x86_64, Ctrl+V in Chat mode).
//!
//! Shells out to `wl-paste --type image/png` (Wayland) or
//! `xclip -selection clipboard -t image/png -o` (X11) on a plain
//! `std::thread`, using the same thread + channel + recv_timeout pattern as
//! `tool::shell::Bash`. The raw PNG bytes are sent back over a
//! `std::sync::mpsc` channel stored in `AppStateRest::clipboard_rx`; the
//! event-loop drain picks them up next tick and calls
//! `AppStateRest::try_attach_image_bytes`.
//!
//! If neither tool is present, the clipboard is empty, or the data is not an
//! image, an `Err(reason)` is sent instead and the drain surfaces a toast.
//!
//! Trigger: Ctrl+V in Chat mode (only when no request is in flight and the mode
//! is not already in a clipboard fetch).  Ctrl+V was chosen because it is the
//! conventional "paste" key; the terminal already delivers text pastes via the
//! bracketed-paste protocol (which koma routes through `handle_paste`), so
//! Ctrl+V would otherwise be a no-op here.

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::app::state::AppStateRest;

/// Maximum time to wait for the clipboard tool to respond (2 s). Paste should
/// be near-instant; this guards against a stalled `xclip`/`wl-paste`.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_millis(2_000);

/// Spawn a background thread that reads PNG bytes from the OS clipboard and
/// sends the result to `state.rest.clipboard_rx`.
///
/// A fetch is a no-op when one is already in flight (`clipboard_rx` is
/// `Some`). The caller (the Ctrl+V key handler) is responsible for checking
/// this before calling.
///
/// The thread tries `wl-paste` first (Wayland; environment variable
/// `WAYLAND_DISPLAY` present) then falls back to `xclip` (X11). If neither
/// is available the thread sends `Err("no clipboard tool found …")`.
pub fn request_clipboard_image(rest: &mut AppStateRest) {
    // Already fetching — ignore a duplicate Ctrl+V.
    if rest.clipboard_rx.is_some() {
        return;
    }

    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, String>>();
    rest.clipboard_rx = Some(rx);

    // Detect Wayland vs X11 once, before moving into the thread.
    let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();

    std::thread::spawn(move || {
        let result = fetch_clipboard_png(is_wayland);
        // Dropped receiver (app closing) — silently discard.
        let _ = tx.send(result);
    });
}

/// Try to read PNG bytes from the clipboard. Returns the raw bytes on success
/// or a human-readable error string on failure. Blocking — runs on a
/// background thread.
fn fetch_clipboard_png(is_wayland: bool) -> Result<Vec<u8>, String> {
    if is_wayland {
        match try_wl_paste() {
            Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
            Ok(_) => return Err("clipboard is empty or contains no PNG image".to_string()),
            Err(_) => {
                // wl-paste not available or failed: fall through to xclip.
            }
        }
    }
    // Try xclip (X11 or Wayland with XWayland).
    match try_xclip() {
        Ok(bytes) if !bytes.is_empty() => Ok(bytes),
        Ok(_) => Err("clipboard is empty or contains no PNG image".to_string()),
        Err(e) => Err(e),
    }
}

/// Run `wl-paste --type image/png` and return its stdout as raw bytes.
fn try_wl_paste() -> Result<Vec<u8>, String> {
    let child = Command::new("wl-paste")
        .args(["--type", "image/png"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("wl-paste not available: {e}"))?;

    let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(CLIPBOARD_TIMEOUT) {
        Ok(Ok(out)) if out.status.success() => Ok(out.stdout),
        Ok(Ok(_)) => Err("wl-paste exited with error (no PNG on clipboard)".to_string()),
        Ok(Err(e)) => Err(format!("wl-paste failed: {e}")),
        Err(_) => Err("wl-paste timed out".to_string()),
    }
}

/// Run `xclip -selection clipboard -t image/png -o` and return its stdout.
fn try_xclip() -> Result<Vec<u8>, String> {
    let child = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "image/png", "-o"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| {
            "no clipboard tool found (install wl-paste for Wayland or xclip for X11)".to_string()
        })?;

    let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(CLIPBOARD_TIMEOUT) {
        Ok(Ok(out)) if out.status.success() => Ok(out.stdout),
        Ok(Ok(_)) => Err("xclip exited with error (no PNG on clipboard)".to_string()),
        Ok(Err(e)) => Err(format!("xclip failed: {e}")),
        Err(_) => Err("xclip timed out".to_string()),
    }
}
