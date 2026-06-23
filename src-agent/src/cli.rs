//! CLI argument parsing.
//!
//! Currently only one flag exists: `--resume`, which opens the session picker
//! on startup instead of jumping straight into a new chat.
//!
//! `parse` accepts anything that yields `String` items so it can be called
//! with `std::env::args()` directly from `main`.

/// Parsed command-line options passed through to the runtime.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// When `true`, show the session picker on startup (`--resume` flag).
    pub resume: bool,
}

/// Parse command-line arguments into [`Opts`].
///
/// `--resume` may appear anywhere in the argument list; position is not
/// significant.  Unknown flags are silently ignored.
pub fn parse(args: impl IntoIterator<Item = String>) -> Opts {
    let resume = args.into_iter().any(|a| a == "--resume");
    Opts { resume }
}
