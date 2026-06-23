#[derive(Debug, Clone, Default)]
pub struct Opts {
    pub resume: bool,
}

/// `--resume` anywhere in args enables resume mode.
pub fn parse(args: impl IntoIterator<Item = String>) -> Opts {
    let resume = args.into_iter().any(|a| a == "--resume");
    Opts { resume }
}
