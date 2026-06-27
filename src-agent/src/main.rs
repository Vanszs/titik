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

    // --- headless path: run the koma-daemon event loop (no TUI) ---
    // Owns the agent runtime with no terminal; a TUI attaches as a thin client in
    // a later stage. Stays in this branch (loops forever) until Ctrl-C.
    if opts.daemon {
        return app::run_daemon(opts);
    }

    // --- normal path: launch the TUI ---
    app::run(opts)
}
