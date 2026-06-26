//! Auxiliary types used by [`super::AppStateRest`] and the rest of the app.
//!
//! - [`AgentMode`]       – tool-approval policy (auto vs. normal)
//! - [`ToastKind`]       – visual style of a transient toast box
//! - [`TranscriptCache`] – per-frame rendered-lines cache
//! - [`CataloguePending`] – debounced model-catalogue fetch request

use ratatui::text::Line;
use crate::view::theme::Palette;

/// Tool-approval policy for the agentic loop.
///
/// - `Auto`: every requested tool runs immediately (no prompt) — the original
///   behaviour.
/// - `Normal`: *risky* tools (write/delete) pause the turn for a `y/n` user
///   approval; *safe* tools (read/dir_list/dir_cache_update) still run inline.
///
/// Toggled with Shift+Tab or `/mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    #[default]
    Auto,
    Normal,
}

impl AgentMode {
    /// Short display label for the header / status line.
    pub fn label(self) -> &'static str {
        match self {
            AgentMode::Auto => "auto",
            AgentMode::Normal => "normal",
        }
    }
    /// The opposite mode (for the toggle key / command).
    pub fn toggled(self) -> Self {
        match self {
            AgentMode::Auto => AgentMode::Normal,
            AgentMode::Normal => AgentMode::Auto,
        }
    }
}

/// Visual style of the transient toast box.
///
/// - `Error`: red box titled "error" — failures (the original behaviour).
/// - `Info`: neutral accent box titled "info" — non-failure notices (e.g. the
///   post-compaction summary). Rendered multi-line / wrapped, never red so an
///   informational message doesn't read as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Error,
    Info,
}

/// Per-frame cache of the transcript's rendered visual lines.
///
/// Markdown rendering (pulldown-cmark + syntect highlighting) and span-wrapping
/// are expensive and would otherwise re-run for every committed message on every
/// redraw (every streamed token, every scroll). This caches each NON-system
/// message's fully-rendered visual lines so they are computed once and reused
/// across frames; only NEW messages are rendered. The cache is keyed by the wrap
/// width + palette, so a resize or theme change forces a full rebuild; a shrink
/// of the message list (compaction / resend) also forces a rebuild.
#[derive(Default)]
pub struct TranscriptCache {
    pub width: usize,
    pub palette: Option<Palette>,
    /// One entry per NON-system message, in order; each is that message's
    /// rendered visual lines (bullet+indent applied, no separator).
    pub blocks: Vec<Vec<Line<'static>>>,
}

/// A debounced, pending model-catalogue (`GET {endpoint}/models`) fetch.
///
/// Created/refreshed by [`super::AppStateRest::request_catalogue`] on each omnisearch
/// keystroke or provider change. `due` is pushed ~300 ms into the future every
/// time the same request is re-issued, so a burst of typing collapses into a
/// single fetch fired once the user pauses. The event-loop tick reads `due`; when
/// `now >= due` (and nothing is already in flight) it takes this and spawns the
/// fetch against `endpoint`/`api_key`.
#[derive(Debug, Clone)]
pub struct CataloguePending {
    /// The endpoint to fetch `/models` from.
    pub endpoint: String,
    /// Bearer token for that endpoint (may be empty for a keyless catalogue).
    pub api_key: String,
    /// Earliest instant the fetch may fire (debounce gate).
    pub due: std::time::Instant,
}
