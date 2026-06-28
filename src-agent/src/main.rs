//! Binary entry point for the simple-coders-agent TUI.
//!
//! Parses CLI arguments via [`cli::parse`], handles any short-circuit modes
//! (provisioner flags), then hands control to [`app::run`] which initialises
//! the terminal, enters the event loop, and returns when the user quits.
//!
//! # Short-circuit modes (exit before TUI)
//!
//! | Flag | Action |
//! |---|---|
//! | `--internet-fullmode-install [--force]` | provision Python full-mode (browser) env then exit |
//! | `--internet-fullmode-uninstall`         | remove Python full-mode env then exit |
//! | `--ipc-selftest`                        | round-trip the daemon IPC transport then exit |
//! | `--daemon`                              | run the headless koma-daemon event loop (no TUI) |
//!
//! Data flow overview:
//! ```text
//! terminal event
//!     → controller::input::handle_key  (KeyEvent → Action)
//!     → app::runtime (Action → state mutation + optional async API call)
//!     → view::draw   (AppState → rendered Frame)
//! ```

mod app;
mod cli;
mod config;
mod controller;
mod dto;
mod internet;
mod ipc;
mod model;
mod resources;
mod service;
mod tool;
mod view;

fn main() -> anyhow::Result<()> {
    // Migrate legacy config dir (~/.simple-coder -> ~/.koma) before anything
    // reads base_dir(), so every entry path (TUI, --internet-fullmode-install,
    // --resume) sees the migrated directory.
    model::store::migrate_legacy_dir();

    let opts = cli::parse(std::env::args());

    // --- short-circuit: `koma daemon <verb>` management CLI (no TUI) ---
    // Mirrors how the provisioner / self-test flags short-circuit BEFORE the TUI, but
    // this is a positional SUBCOMMAND (status/kill/restart/clean) — the operator
    // control surface for the headless daemon (#118). It must work even when the TUI
    // can't start, so it runs first and exits the process directly. A bare/unknown verb
    // (`DaemonCli::Usage`) prints usage and exits non-zero. The default launch path is
    // unchanged — `daemon` is the only token that diverts here.
    if let Some(sub) = opts.subcommand {
        match sub {
            cli::DaemonCli::Run(verb) => {
                return match app::run_daemon_subcommand(verb) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("error: {e:#}");
                        std::process::exit(1);
                    }
                };
            }
            cli::DaemonCli::Usage => {
                std::process::exit(app::print_daemon_usage());
            }
        }
    }

    // --- short-circuit: provisioner modes (no TUI) ---

    if opts.internet_fullmode_install {
        return match internet::install(opts.force) {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        };
    }

    if opts.internet_fullmode_uninstall {
        return match internet::uninstall() {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        };
    }

    // --- short-circuit: IPC transport self-test (no TUI) ---
    // Exercises frame.rs + server.rs + client.rs end-to-end; never returns
    // (always exits the process with OK/FAIL status).
    if opts.ipc_selftest {
        ipc::selftest::run();
    }

    // --- short-circuit: END-TO-END daemon self-test (no TUI) ---
    // Drives the full stage-5 stack (bind + accept loop + per-client tasks + the
    // real daemon_loop hub) over a real socket: a client attaches, submits, sees
    // the resulting delta, then quits the daemon. Never returns (OK/FAIL exit).
    if opts.daemon_selftest {
        app::run_daemon_selftest();
    }

    // --- headless path: run the koma-daemon event loop (no TUI) ---
    // Owns the agent runtime with no terminal; a TUI attaches as a thin client via
    // `--attach`. Stays in this branch (loops forever) until QuitDaemon / Ctrl-C.
    if opts.daemon {
        return app::run_daemon(opts);
    }

    // --- thin-client path: attach to a running daemon and render its state ---
    // Connects to ~/.koma/daemon.sock, renders the daemon's foreground session from
    // streamed snapshots/deltas, and forwards input. Detaching (Ctrl-C) leaves the
    // daemon running. The daemon path is opt-in; normal `koma` is unaffected.
    if opts.attach {
        return app::client_run(opts);
    }

    // --- normal path: launch the LOCAL TUI (fully functional standalone) ---
    app::run(opts)
}
