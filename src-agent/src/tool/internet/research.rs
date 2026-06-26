//! `research` tool: deep multi-source web research via the vendored Python
//! `scrapion_agent` package (a real Firefox driven by Playwright).
//!
//! This is the "full" internet tier: heavier and slower than `web_search` /
//! `web_fetch` (it launches a browser and visits several pages), so it is NEVER
//! advertised to the main chat model (see `crate::tool::main_tool_names`) and is
//! reachable only by the `researcher` sub-agent whose allow-list names it.
//!
//! ## Concurrency pattern
//!
//! `Tool::run` is synchronous but called from a tokio runtime thread. The
//! subprocess is launched and waited inside a freshly spawned `std::thread`
//! (which has no tokio context), and its `Output` is returned via an
//! `mpsc::channel` with `recv_timeout` — the same guard used by `shell::Bash`
//! and `internet::http_get_blocking`.

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::Duration;

use super::{Tool, ToolCtx};

/// Outer wait budget for the subprocess. scrapion launches Firefox and visits
/// several pages, so this is deliberately generous.
const RESEARCH_TIMEOUT_SECS: u64 = 180;

/// Per-result content cap (characters) in the formatted summary.
const PER_RESULT_CHARS: usize = 3_000;

/// Whole-output cap (characters) for the summary handed back to the model.
const MAX_OUTPUT_CHARS: usize = 16_000;

/// Deep web research via a real browser (the vendored `scrapion_agent`).
pub struct Research;

impl Tool for Research {
    fn name(&self) -> &'static str { "research" }

    fn description(&self) -> &'static str {
        "Deep web research via a real browser: searches the web, visits the most relevant pages, \
         and returns a detailed report. Slower/heavier than web_search/web_fetch — use for \
         thorough multi-source research."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The research question or topic to investigate."
                }
            },
            "required": ["query"]
        })
    }

    fn run(&self, _ctx: &ToolCtx, args: &Value) -> Result<String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required string argument 'query'"))?
            .to_string();

        // Gate: the heavy environment must be provisioned first.
        if !crate::internet::is_installed() {
            return Ok(
                "error: research environment not installed — run `koma --install-internet`"
                    .to_string(),
            );
        }

        // Resolve the venv Python + install dir; surface a readable error rather
        // than panicking if either path can't be built.
        let python = match crate::internet::venv_python() {
            Ok(p) => p,
            Err(e) => return Ok(format!("error: cannot locate research environment: {e}")),
        };
        let dir = match crate::internet::internet_dir() {
            Ok(p) => p,
            Err(e) => return Ok(format!("error: cannot locate research environment: {e}")),
        };

        // Run `python -m scrapion_agent --json "<query>"` with cwd = install dir,
        // inside a dedicated OS thread (no tokio context), guarded by recv_timeout.
        let (tx, rx) = mpsc::channel::<std::io::Result<std::process::Output>>();
        let query_arg = query.clone();
        std::thread::spawn(move || {
            let out = std::process::Command::new(&python)
                .arg("-m")
                .arg("scrapion_agent")
                .arg("--json")
                .arg(&query_arg)
                .current_dir(&dir)
                .output();
            let _ = tx.send(out);
        });

        let output = match rx.recv_timeout(Duration::from_secs(RESEARCH_TIMEOUT_SECS)) {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Ok(format!("error: research failed to launch: {e}")),
            // The thread still owns the child; we can't reap it here, but we
            // return a clear message so the turn doesn't stall.
            Err(_) => {
                return Ok(format!(
                    "error: research timed out after {RESEARCH_TIMEOUT_SECS}s"
                ))
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Non-zero exit: surface a stderr tail (never the raw bytes).
        if !output.status.success() {
            let code = output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            return Ok(format!(
                "error: research failed (exit {code}): {}",
                tail_chars(stderr.trim(), 500)
            ));
        }

        // The Python contract prints exactly one JSON report to stdout. Be
        // defensive: if it isn't valid JSON, surface a short prefix rather than
        // panicking.
        let report: Value = match serde_json::from_str(stdout.trim()) {
            Ok(v) => v,
            Err(_) => {
                let snippet = if stdout.trim().is_empty() {
                    tail_chars(stderr.trim(), 500)
                } else {
                    first_chars(stdout.trim(), 500)
                };
                return Ok(format!("error: research failed: {snippet}"));
            }
        };

        Ok(format_report(&query, &report))
    }
}

/// Format the scrapion JSON report into a capped markdown summary for the model.
///
/// Layout:
/// ```text
/// # Research: <query>
/// scrapes: <successful> ok, <failed> failed
///
/// ### <url>
/// <content, truncated to PER_RESULT_CHARS>
///
/// ### <url>
/// ...
/// ```
/// A top-level `error` field is surfaced verbatim. The whole string is capped at
/// [`MAX_OUTPUT_CHARS`] with a truncation note appended.
fn format_report(query: &str, report: &Value) -> String {
    // A reported error short-circuits the formatting.
    if let Some(err) = report.get("error").and_then(Value::as_str) {
        if !err.trim().is_empty() {
            return format!("error: research reported: {}", first_chars(err.trim(), 500));
        }
    }

    let successful = report
        .get("successful_scrapes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let failed = report
        .get("failed_scrapes")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut out = String::new();
    out.push_str(&format!("# Research: {query}\n"));
    out.push_str(&format!("scrapes: {successful} ok, {failed} failed\n"));

    // Per-result sections for each successful scrape.
    if let Some(results) = report.get("results").and_then(Value::as_array) {
        for r in results {
            let url = r.get("url").and_then(Value::as_str).unwrap_or("(unknown url)");
            let content = r.get("content").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!("\n### {url}\n"));
            if content.trim().is_empty() {
                out.push_str("(no content)\n");
            } else {
                out.push_str(&truncate_chars(content, PER_RESULT_CHARS));
                out.push('\n');
            }
        }
    }

    // List any URLs that failed to scrape, for transparency.
    if let Some(failed_urls) = report.get("failed_urls").and_then(Value::as_array) {
        let urls: Vec<&str> = failed_urls.iter().filter_map(Value::as_str).collect();
        if !urls.is_empty() {
            out.push_str("\n### failed urls\n");
            for u in urls {
                out.push_str(&format!("- {u}\n"));
            }
        }
    }

    // Whole-output cap.
    truncate_chars(&out, MAX_OUTPUT_CHARS)
}

/// Truncate a string to at most `max` chars, appending a truncation note when
/// content was dropped. Operates on `char` boundaries so multi-byte text is safe.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n…[truncated]");
    out
}

/// First `n` chars of `s` (char-boundary safe), no note appended.
fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Last `n` chars of `s` (char-boundary safe), no note appended.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}
