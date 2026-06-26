//! `web_fetch` tool: fetch a URL and return readable markdown content.
//!
//! The blocking HTTP request is run on a freshly spawned `std::thread` so it
//! never touches the tokio runtime context (which would panic on `blocking`).

use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;
use super::{http_get_blocking, looks_like_cloudflare, Tool, ToolCtx};

/// Fetch a web page and return its main content as markdown.
pub struct WebFetch;

impl Tool for WebFetch {
    fn name(&self) -> &'static str { "web_fetch" }

    fn description(&self) -> &'static str {
        "Fetch a web page by URL and return its main readable content as markdown. \
        Use when you already have a specific URL to read."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The full URL to fetch (must start with http:// or https://)."
                }
            },
            "required": ["url"]
        })
    }

    fn run(&self, _ctx: &ToolCtx, args: &Value) -> Result<String> {
        let url = args.get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required string argument 'url'"))?;

        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(format!("error: url must start with http:// or https://, got: {url}"));
        }

        let (status, body) = match http_get_blocking(url, Duration::from_secs(30)) {
            Ok(v) => v,
            Err(e) => return Ok(format!("error: {e}")),
        };

        if looks_like_cloudflare(status, &body) {
            return Ok(format!("blocked: Cloudflare challenge, skipped {url}"));
        }

        if !(200..300).contains(&status) {
            return Ok(format!("error: HTTP {status} for {url}"));
        }

        // Try dom_smoothie readability extraction; fall back to full HTML on failure.
        let readable_html = extract_readable(&body, url);

        // Convert HTML to markdown.
        let markdown = html2md::rewrite_html(&readable_html, false);

        // Trim and cap at ~20 000 chars.
        const MAX_CHARS: usize = 20_000;
        let trimmed = markdown.trim();
        let (content, truncated) = if trimmed.chars().count() > MAX_CHARS {
            let cut: String = trimmed.chars().take(MAX_CHARS).collect();
            (cut, true)
        } else {
            (trimmed.to_string(), false)
        };

        let mut out = format!("source: {url}\n\n{content}");
        if truncated {
            out.push_str("\n\n... (content truncated at 20000 chars)");
        }
        Ok(out)
    }
}

/// Try dom_smoothie readability on the HTML; return the article content HTML on
/// success, or the original HTML unchanged when readability fails.
fn extract_readable(html: &str, url: &str) -> String {
    // dom_smoothie requires an absolute URL; the caller already validated it.
    match dom_smoothie::Readability::new(html, Some(url), None) {
        Ok(mut r) => match r.parse() {
            Ok(article) => article.content.to_string(),
            Err(_) => html.to_string(),
        },
        Err(_) => html.to_string(),
    }
}
