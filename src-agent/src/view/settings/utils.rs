/// Format a per-token USD price string (e.g. `"0.00000015"`, as OpenRouter
/// reports it) as a per-MILLION-token dollar amount: `$0.15`. Returns `$?` when
/// the value is absent or unparseable, so a row always renders something.
pub(super) fn price_per_million(per_token: Option<&String>) -> String {
    match per_token.and_then(|s| s.trim().parse::<f64>().ok()) {
        Some(v) => format!("${:.2}", v * 1_000_000.0),
        None => "$?".to_string(),
    }
}

/// Truncate `s` to at most `max` chars, appending `…` if cut.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let cut = max.saturating_sub(1);
        chars[..cut].iter().collect::<String>() + "…"
    }
}
