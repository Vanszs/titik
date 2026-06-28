//! CLI argument parsing.
//!
//! Flags:
//! - `--resume`                    — open the session picker on startup instead of a new chat.
//! - `--internet-fullmode-install` — provision the Python full-mode (browser) environment and exit.
//! - `--internet-fullmode-uninstall` — remove the Python full-mode environment and exit.
//! - `--force`                     — modifier for `--internet-fullmode-install`: force a reinstall
//!   even when the environment is already present.
//! - `--ipc-selftest`              — round-trip the daemon IPC transport end-to-end, then exit.
//! - `--daemon-selftest`           — drive the full daemon stack (bind/accept/per-client/loop) end-to-end, then exit.
//! - `--daemon`                    — run the headless koma-daemon event loop (no terminal).
//! - `--attach`                    — run as a thin client that attaches to a running daemon.
//!
//! `parse` accepts anything that yields `String` items so it can be called
//! with `std::env::args()` directly from `main`.

/// Parsed command-line options passed through to the runtime.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// When `true`, show the session picker on startup (`--resume` flag).
    pub resume: bool,
    /// When `true`, provision the Python full-mode (browser) environment then exit.
    pub internet_fullmode_install: bool,
    /// When `true`, remove the Python full-mode (browser) environment then exit.
    pub internet_fullmode_uninstall: bool,
    /// Modifier for `--internet-fullmode-install`: overwrite an existing install.
    pub force: bool,
    /// When `true`, run the daemon IPC transport self-test then exit
    /// (`--ipc-selftest` flag).
    pub ipc_selftest: bool,
    /// When `true`, run the END-TO-END daemon self-test (bind + accept loop +
    /// per-client tasks + real `daemon_loop`: a client attaches, submits, observes
    /// the resulting delta, then quits the daemon) then exit (`--daemon-selftest`).
    pub daemon_selftest: bool,
    /// When `true`, run the headless koma-daemon event loop with no terminal
    /// (`--daemon` flag). Owns the agent runtime; a TUI attaches as a client.
    pub daemon: bool,
    /// When `true`, run as a thin client that attaches to a running daemon
    /// (`--attach` flag): connect to `~/.koma/daemon.sock`, render the daemon's
    /// foreground session from streamed snapshots/deltas, and forward input.
    pub attach: bool,
}

/// Parse command-line arguments into [`Opts`].
///
/// All flags may appear anywhere in the argument list; position is not
/// significant. Unknown flags are silently ignored.
pub fn parse(args: impl IntoIterator<Item = String>) -> Opts {
    let mut opts = Opts::default();
    for arg in args {
        match arg.as_str() {
            "--resume"                       => opts.resume = true,
            "--internet-fullmode-install"    => opts.internet_fullmode_install = true,
            "--internet-fullmode-uninstall"  => opts.internet_fullmode_uninstall = true,
            "--force"                        => opts.force = true,
            "--ipc-selftest"                 => opts.ipc_selftest = true,
            "--daemon-selftest"              => opts.daemon_selftest = true,
            "--daemon"                       => opts.daemon = true,
            "--attach"                       => opts.attach = true,
            _                                => {}
        }
    }
    opts
}
