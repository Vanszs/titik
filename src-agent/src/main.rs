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
//! | `--install-internet [--force]` | provision Python research env then exit |
//! | `--uninstall-internet`         | remove Python research env then exit |
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
mod model;
mod resources;
mod service;
mod tool;
mod view;

fn main() -> anyhow::Result<()> {
    let opts = cli::parse(std::env::args());

    // --- short-circuit: provisioner modes (no TUI) ---

    if opts.install_internet {
        return match internet::install(opts.force) {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        };
    }

    if opts.uninstall_internet {
        return match internet::uninstall() {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        };
    }

    // --- normal path: launch the TUI ---
    app::run(opts)
}
