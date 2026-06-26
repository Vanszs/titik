//! CLI argument parsing.
//!
//! Flags:
//! - `--resume`            — open the session picker on startup instead of a new chat.
//! - `--install-internet`  — provision the Python research environment and exit.
//! - `--uninstall-internet`— remove the Python research environment and exit.
//! - `--force`             — modifier for `--install-internet`: force a reinstall
//!   even when the environment is already present.
//!
//! `parse` accepts anything that yields `String` items so it can be called
//! with `std::env::args()` directly from `main`.

/// Parsed command-line options passed through to the runtime.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// When `true`, show the session picker on startup (`--resume` flag).
    pub resume: bool,
    /// When `true`, provision the Python internet-research environment then exit.
    pub install_internet: bool,
    /// When `true`, remove the Python internet-research environment then exit.
    pub uninstall_internet: bool,
    /// Modifier for `--install-internet`: overwrite an existing install.
    pub force: bool,
}

/// Parse command-line arguments into [`Opts`].
///
/// All flags may appear anywhere in the argument list; position is not
/// significant. Unknown flags are silently ignored.
pub fn parse(args: impl IntoIterator<Item = String>) -> Opts {
    let mut opts = Opts::default();
    for arg in args {
        match arg.as_str() {
            "--resume"             => opts.resume = true,
            "--install-internet"   => opts.install_internet = true,
            "--uninstall-internet" => opts.uninstall_internet = true,
            "--force"              => opts.force = true,
            _                      => {}
        }
    }
    opts
}
