//! Terminal RAII guard: raw mode + alternate screen, restored on drop.

use std::io::stdout;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

/// RAII guard for terminal state. Entering enables raw mode + the alternate
/// screen; dropping (normal return, `?`-error after creation, or panic-unwind)
/// always restores the terminal.
pub(super) struct TerminalGuard;

impl TerminalGuard {
    pub(super) fn enter() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        if let Err(e) = execute!(stdout(), EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), crossterm::cursor::Show);
    }
}
