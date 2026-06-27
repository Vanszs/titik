//! View – live-session picker (`/swap`, `LiveSessionPicker` mode).
//!
//! Lists the currently-RUNNING sessions (one row per live
//! [`crate::app::state::SessionRuntime`]) so the user can switch which one is on
//! screen. Layout (top to bottom), matching the `--resume` session picker's flat
//! style:
//!
//! 1. Top+bottom rule title bar — ` live sessions ` on the TOP rule.
//! 2. Flat (borderless) session list — `<name>   ● working / ○ ready`, with the
//!    current foreground row marked `(current)`. The selected row is highlighted
//!    with `palette.sel_fg` on `palette.sel_bg`.
//! 3. One-line keybinding hint.
//!
//! Selection state lives in [`crate::app::mode::LiveSessionPicker`]; keystroke
//! handling lives in [`crate::controller::input::handle_live_picker`].

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use crate::app::mode::LiveSessionPicker;
use crate::view::theme::Palette;

/// Truncate `s` to at most `max` Unicode scalar values, appending `…` if cut.
fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        // Reserve one char for the ellipsis.
        let mut out: String = chars[..max.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }
}

/// Render the live-session picker for `picker` using the given colour `palette`.
///
/// `foreground` is the currently-on-screen session's Vec index, used to mark its
/// row `(current)`.
pub fn draw(frame: &mut Frame, picker: &LiveSessionPicker, foreground: usize, palette: &Palette) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // title: top+bottom rules
            Constraint::Min(1),    // flat session list
            Constraint::Length(1), // keybinding hint line
        ])
        .split(frame.area());

    // --- Title bar ---
    // Top+bottom rules only — title sits on the TOP rule, dim style.
    let title_block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(palette.dim))
        .title(Span::styled(
            " live sessions ",
            Style::default().fg(palette.dim),
        ))
        .padding(Padding::horizontal(1));

    let title_inner = title_block.inner(chunks[0]);
    let note = Line::from(Span::styled(
        "switch which running session is on screen — others keep cooking",
        Style::default().fg(palette.dim),
    ));
    frame.render_widget(title_block, chunks[0]);
    frame.render_widget(Paragraph::new(note), title_inner);

    // --- Session list (flat, no borders) ---
    // Render rows directly into the inset area (1 char horizontal margin).
    let inner = chunks[1].inner(Margin { horizontal: 1, vertical: 0 });
    let inner_w = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();
    for (i, entry) in picker.entries.iter().enumerate() {
        // Right column: working/ready marker + a (current) tag on the foreground.
        // NO emoji — house rule; the ●/○ glyphs are box-drawing-adjacent markers.
        let state_marker = if entry.working { "● working" } else { "○ ready  " };
        let current = if entry.idx == foreground { "  (current)" } else { "" };
        let right = format!("{state_marker}{current}");
        // Width available for the name: inner width minus right column minus two
        // separator spaces, clamped to at least 4 chars.
        let name_w = inner_w.saturating_sub(right.chars().count() + 2).max(4);
        let name = truncate(&entry.name, name_w);
        let row = format!("{name:<name_w$}  {right}");

        let style = if i == picker.selected {
            Style::default().fg(palette.sel_fg).bg(palette.sel_bg)
        } else {
            Style::default().fg(palette.fg)
        };
        lines.push(Line::styled(row, style));
    }

    // Scroll so the selected row stays visible within the inner height.
    let list_height = inner.height as usize;
    let scroll = picker
        .selected
        .saturating_sub(list_height.saturating_sub(1)) as u16;

    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);

    // --- Keybinding hint ---
    let hint = "↑↓ select · Enter switch · Esc cancel";
    let instructions = Paragraph::new(hint).style(Style::default().fg(palette.dim));
    frame.render_widget(instructions, chunks[2]);
}
