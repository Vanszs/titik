//! Daemon-side socket setup: bind the unix listener and accept client connections.
//!
//! # Bind is the liveness oracle (critique #3)
//!
//! Whether the daemon is "alive" is decided by who currently HOLDS the bound unix
//! socket — NOT by reading a PID file and probing `/proc`. PIDs get reused, which
//! would wedge spawn-or-attach into talking to an unrelated process. So the real
//! liveness test (added in the spawn-or-attach stage) is: try to `connect` to the
//! socket — success means a daemon is live; connection refused / no listener means
//! it is not, and this process may [`bind`] and become the daemon. [`bind`] here
//! is the other half of that contract: it removes any stale socket file left by a
//! crashed daemon, then binds fresh. The spawn/attach decision logic that uses it
//! lands in a later stage; this module only provides the primitives.

use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

use crate::model::store;

/// Bind the daemon's unix-domain listener at `path` (typically
/// [`store::daemon_sock_path`]).
///
/// Steps:
/// 1. Ensure the parent dir (`~/.koma`) exists — `bind` fails if it does not.
/// 2. Unlink any stale socket file already at `path`. A leftover socket from a
///    crashed daemon would otherwise make `bind` fail with `AddrInUse` even though
///    nobody is listening. (Removing it is safe precisely because bind — not the
///    file's existence — is the liveness oracle; a still-live daemon keeps the
///    socket and the caller would have connected to it instead of binding.)
/// 3. Bind and return the listener. Holding it is what makes this process the live
///    daemon.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    // 1. ~/.koma (or whatever parent the path has) must exist before bind.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 2. Remove a stale socket file; ignore "not found" (nothing to clean up).
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // 3. Bind fresh — this is the liveness oracle.
    UnixListener::bind(path)
}

/// Convenience over [`store::daemon_sock_path`] + [`bind`]: resolve the canonical
/// daemon socket path and bind it.
#[allow(dead_code)] // wired in daemon stage 3+ (spawn-or-attach)
pub fn bind_default() -> anyhow::Result<UnixListener> {
    let path = store::daemon_sock_path()?;
    Ok(bind(&path)?)
}

/// Await the next client connection on `listener`, returning just the stream
/// (the peer's anonymous unix address is not needed — clients are identified by
/// the [`super::proto::ClientRequest::Attach`] handshake, not their socket addr).
#[allow(dead_code)] // wired in daemon stage 3+ (accept loop)
pub async fn accept(listener: &UnixListener) -> std::io::Result<UnixStream> {
    let (stream, _addr) = listener.accept().await?;
    Ok(stream)
}
