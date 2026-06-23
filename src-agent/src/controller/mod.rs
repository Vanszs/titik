//! Controller layer — translates raw terminal events into typed [`Action`]s
//! and [`Command`]s that the runtime can act on.
//!
//! - [`input`]   – maps a [`crossterm::event::KeyEvent`] to an [`input::Action`]
//! - [`command`] – parses a `/slash` line into a [`command::Command`]

pub mod command;
pub mod input;
