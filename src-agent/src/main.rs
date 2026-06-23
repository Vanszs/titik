//! Binary entry point for the simple-coders-agent TUI.
//!
//! Parses CLI arguments via [`cli::parse`], then hands control to
//! [`app::run`] which initialises the terminal, enters the event loop,
//! and returns when the user quits.
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
mod model;
mod resources;
mod service;
mod tool;
mod view;

fn main() -> anyhow::Result<()> {
    let opts = cli::parse(std::env::args());
    app::run(opts)
}
