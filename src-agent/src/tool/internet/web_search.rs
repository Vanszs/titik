//! `web_search` tool: query DuckDuckGo and return titles, URLs, and snippets.
//!
//! Uses the DDG HTML endpoint (`html.duckduckgo.com/html/`) and parses results
//! with the `scraper` crate.  Page-1 only.
//! TODO: vqd paging for additional result pages.

use anyhow::Result;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use scraper::{Html, Selector};
use serde_json::{json, Value};
use std::time::Duration;
use super::{http_get_blocking, Tool, ToolCtx};

const DEFAULT_REGION: &str = "wt-wt";
const TOP_N: usize = 8;

/// Search DuckDuckGo and return result titles, URLs, and snippets.
pub struct WebSearch;

impl Tool for WebSearch {
    fn name(&self) -> &'static str { "web_search" }

    fn description(&self) -> &'static str {
        "Search the web (DuckDuckGo) for a query and return result titles, URLs, and snippets. \
        Use to discover pages, then web_fetch the most relevant URL."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query."
                },
                "region": {
                    "type": "string",
                    "description": "DDG region code (kl parameter, e.g. 'us-en', 'de-de'). Defaults to 'wt-wt' (no region)."
                }
            },
            "required": ["query"]
        })
    }

    fn run(&self, _ctx: &ToolCtx, args: &Value) -> Result<String> {
        let query = args.get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required string argument 'query'"))?;

        let region = args.get("region")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_REGION);

        let encoded_query = utf8_percent_encode(query, NON_ALPHANUMERIC).to_string();
        let url = format!(
            "https://html.duckduckgo.com/html/?q={}&kl={}",
            encoded_query, region
        );

        let (status, body) = match http_get_blocking(&url, Duration::from_secs(20)) {
            Ok(v) => v,
            Err(e) => return Ok(format!("error: {e}")),
        };

        // DDG returns 202 or a specific body pattern when rate-limiting.
        if status == 202 || body.contains("duckduckgo.com/t/") && body.len() < 1000 {
            return Ok("web_search: DuckDuckGo rate-limited, try again shortly".to_string());
        }

        if !(200..300).contains(&status) {
            return Ok(format!("error: HTTP {status} from DuckDuckGo"));
        }

        let results = parse_ddg_results(&body);

        if results.is_empty() {
            return Ok(format!(
                "web_search: no results found for query: {query}\n\
                (DuckDuckGo may have returned a captcha or an empty page)"
            ));
        }

        let mut out = String::new();
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!(
                "{}. {}\n   {}\n   {}\n",
                i + 1,
                r.title,
                r.url,
                r.snippet
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Parse DuckDuckGo HTML results page into structured results.
///
/// DDG result structure (HTML endpoint):
///   - Result anchors: `a.result__a`  (title text + href with DDG redirect)
///   - Snippets: `.result__snippet`
///
/// DDG wraps real URLs in a redirect:
///   `//duckduckgo.com/l/?uddg=<encoded>&rut=...`
/// We decode the `uddg` param to recover the real URL.
fn parse_ddg_results(html: &str) -> Vec<SearchResult> {
    let document = Html::parse_document(html);

    let title_sel = match Selector::parse("a.result__a") {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let snippet_sel = match Selector::parse(".result__snippet") {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let titles: Vec<(String, String)> = document
        .select(&title_sel)
        .map(|el| {
            let title = el.text().collect::<String>().trim().to_string();
            let raw_href = el.value().attr("href").unwrap_or("").to_string();
            let real_url = decode_ddg_redirect(&raw_href);
            (title, real_url)
        })
        .collect();

    let snippets: Vec<String> = document
        .select(&snippet_sel)
        .map(|el| el.text().collect::<String>().trim().to_string())
        .collect();

    titles
        .into_iter()
        .zip(snippets.into_iter().chain(std::iter::repeat(String::new())))
        .take(TOP_N)
        .filter(|((title, _url), _snippet)| !title.is_empty())
        .map(|((title, url), snippet)| SearchResult { title, url, snippet })
        .collect()
}

/// Decode a DDG redirect URL to the real target URL.
///
/// DDG format: `//duckduckgo.com/l/?uddg=<percent-encoded-url>&rut=...`
/// or already a direct URL (`https://...`).
fn decode_ddg_redirect(href: &str) -> String {
    // Already an absolute URL — return as-is.
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }

    // Normalise `//duckduckgo.com/l/...` to a parseable form.
    let normalised = if href.starts_with("//") {
        format!("https:{href}")
    } else if href.starts_with('/') {
        format!("https://duckduckgo.com{href}")
    } else {
        href.to_string()
    };

    // Extract the `uddg` query parameter.
    if let Ok(parsed) = url::Url::parse(&normalised) {
        for (key, value) in parsed.query_pairs() {
            if key == "uddg" {
                return value.into_owned();
            }
        }
    }

    // Fallback: return the normalised URL.
    normalised
}
