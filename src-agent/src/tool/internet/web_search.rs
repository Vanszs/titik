//! `web_search` tool: query DuckDuckGo and return titles, URLs, and snippets.
//!
//! Uses the DDG HTML endpoint (`html.duckduckgo.com/html/`) and parses results
//! with the `scraper` crate.  Page-1 only.
//! TODO: vqd paging for additional result pages.
//!
//! ## Rate-limit hardening
//!
//! DDG returns HTTP 202 when it detects bot-like requests.  This module works
//! around that by:
//!   - Rotating across a pool of realistic desktop browser User-Agent strings.
//!   - Adding browser-like request headers (Accept, Accept-Language, Referer,
//!     Sec-Fetch-*, Origin, Upgrade-Insecure-Requests).
//!   - Appending a pseudo-random `fbid` URL parameter (mirrors what scrapion
//!     does to defeat per-session fingerprinting).
//!   - Retrying up to 3 times with a different UA + fresh fbid after any 202
//!     or zero-result response, with short jittered backoff sleeps (safe here
//!     because `run` is called from the off-UI async-defer thread).

use anyhow::Result;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use scraper::{Html, Selector};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use super::{Tool, ToolCtx};

const DEFAULT_REGION: &str = "wt-wt";
const TOP_N: usize = 8;

/// Realistic desktop browser User-Agent pool (8 entries).
/// Mix of Firefox + Chrome on Windows / macOS / Linux — all plausibly current.
const UA_POOL: &[&str] = &[
    // Chrome 124 — Windows
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
    // Chrome 124 — macOS
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
    // Chrome 124 — Linux
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
    // Firefox 125 — Windows
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:125.0) \
     Gecko/20100101 Firefox/125.0",
    // Firefox 125 — macOS
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.4; rv:125.0) \
     Gecko/20100101 Firefox/125.0",
    // Firefox 125 — Linux
    "Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:125.0) \
     Gecko/20100101 Firefox/125.0",
    // Chrome 123 — Windows (slightly older, adds variety)
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36",
    // Edge 124 — Windows (Chromium-based, distinct UA)
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 Edg/124.0.0.0",
];

/// Derive a pseudo-random `u64` seed from `SystemTime` nanoseconds.
/// No `rand` crate needed — the nanos component varies per call.
fn nanos_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs().wrapping_mul(0x9e37_79b9_7f4a_7c15)))
        .unwrap_or(42)
}

/// Pick a UA from the pool using the given seed, offset by `skip` so that
/// consecutive retry attempts always pick a *different* UA.
fn pick_ua(seed: u64, skip: usize) -> &'static str {
    let idx = ((seed ^ (skip as u64).wrapping_mul(0x517c_c1b7_2722_0a95))
        % UA_POOL.len() as u64) as usize;
    UA_POOL[idx]
}

/// Derive a pseudo-random `fbid` value (9-digit decimal, matching scrapion's
/// pattern) from the seed.
fn make_fbid(seed: u64) -> u64 {
    // Keep it in the 100_000_000 – 999_999_999 range.
    100_000_000 + (seed % 900_000_000)
}

/// DDG-specific blocking GET.
///
/// Spawns a dedicated OS thread (no tokio context), builds a
/// `reqwest::blocking::Client` with browser-like headers there, performs the
/// GET, and returns `(status_code, body)` over an mpsc channel.
/// Mirrors the `http_get_blocking` pattern exactly — std::thread + recv_timeout.
///
/// `ua`: the User-Agent to use for this attempt.
/// `fbid`: the pseudo-random fbid query parameter value.
fn ddg_get(query_url: &str, ua: &str, timeout: Duration) -> Result<(u16, String), String> {
    const MAX_BODY_BYTES: usize = 5 * 1024 * 1024; // 5 MiB

    let url_owned = query_url.to_string();
    let ua_owned = ua.to_string();

    let (tx, rx) = mpsc::channel::<Result<(u16, String), String>>();

    std::thread::spawn(move || {
        let result = (|| -> Result<(u16, String), String> {
            // Build per-request headers.
            let mut default_headers = reqwest::header::HeaderMap::new();

            default_headers.insert(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
                    .parse()
                    .map_err(|e| format!("header parse: {e}"))?,
            );
            default_headers.insert(
                reqwest::header::ACCEPT_LANGUAGE,
                "en-US,en;q=0.9"
                    .parse()
                    .map_err(|e| format!("header parse: {e}"))?,
            );
            default_headers.insert(
                reqwest::header::REFERER,
                "https://duckduckgo.com/"
                    .parse()
                    .map_err(|e| format!("header parse: {e}"))?,
            );
            default_headers.insert(
                reqwest::header::ORIGIN,
                "https://duckduckgo.com"
                    .parse()
                    .map_err(|e| format!("header parse: {e}"))?,
            );
            default_headers.insert(
                "Sec-Fetch-Site".parse::<reqwest::header::HeaderName>()
                    .map_err(|e| format!("header name: {e}"))?,
                "same-origin"
                    .parse()
                    .map_err(|e| format!("header parse: {e}"))?,
            );
            default_headers.insert(
                "Sec-Fetch-Mode".parse::<reqwest::header::HeaderName>()
                    .map_err(|e| format!("header name: {e}"))?,
                "navigate"
                    .parse()
                    .map_err(|e| format!("header parse: {e}"))?,
            );
            default_headers.insert(
                "Sec-Fetch-Dest".parse::<reqwest::header::HeaderName>()
                    .map_err(|e| format!("header name: {e}"))?,
                "document"
                    .parse()
                    .map_err(|e| format!("header parse: {e}"))?,
            );
            default_headers.insert(
                "Sec-Fetch-User".parse::<reqwest::header::HeaderName>()
                    .map_err(|e| format!("header name: {e}"))?,
                "?1"
                    .parse()
                    .map_err(|e| format!("header parse: {e}"))?,
            );
            default_headers.insert(
                "Upgrade-Insecure-Requests".parse::<reqwest::header::HeaderName>()
                    .map_err(|e| format!("header name: {e}"))?,
                "1"
                    .parse()
                    .map_err(|e| format!("header parse: {e}"))?,
            );

            let client = reqwest::blocking::Client::builder()
                .timeout(timeout)
                .user_agent(ua_owned.clone())
                .default_headers(default_headers)
                .build()
                .map_err(|e| format!("client build error: {e}"))?;

            let resp = client
                .get(&url_owned)
                .send()
                .map_err(|e| format!("request failed: {e}"))?;

            let status = resp.status().as_u16();

            let bytes = resp
                .bytes()
                .map_err(|e| format!("failed to read body: {e}"))?;

            let capped = if bytes.len() > MAX_BODY_BYTES {
                &bytes[..MAX_BODY_BYTES]
            } else {
                &bytes[..]
            };

            let body = String::from_utf8_lossy(capped).into_owned();
            Ok((status, body))
        })();

        let _ = tx.send(result);
    });

    // Outer timeout guard — slightly longer than the inner client timeout so
    // the thread always wins the race under normal network conditions.
    let outer_timeout = timeout + Duration::from_secs(5);
    match rx.recv_timeout(outer_timeout) {
        Ok(r) => r,
        Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
    }
}

/// Returns `true` when the response looks like a DDG rate-limit / empty page.
fn looks_rate_limited(status: u16, body: &str, result_count: usize) -> bool {
    if status == 202 {
        return true;
    }
    // DDG sometimes returns 200 with a tiny body containing only tracking
    // pixels — treat suspiciously short bodies with zero results as rate-limited.
    if result_count == 0 && body.len() < 2000 {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Backoff delays (ms) for attempt index 0-based.
// attempt 0 -> no sleep (first try)
// attempt 1 -> ~1200ms base + small jitter
// attempt 2 -> ~2500ms base + small jitter
// ---------------------------------------------------------------------------
const BACKOFF_MS: &[u64] = &[0, 1200, 2500];

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
        let per_request_timeout = Duration::from_secs(12);

        const MAX_ATTEMPTS: usize = 3;

        // Seed once per search invocation; each attempt offsets the UA index
        // and refreshes fbid with a new nanos sample.
        let base_seed = nanos_seed();

        for attempt in 0..MAX_ATTEMPTS {
            // Sleep before retry attempts (not on first try).
            if attempt > 0 {
                let base_ms = BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)];
                // Jitter: add 0-199ms derived from a fresh nanos sample.
                let jitter_ms = nanos_seed() % 200;
                std::thread::sleep(Duration::from_millis(base_ms + jitter_ms));
            }

            // Pick a UA that differs across attempts.
            let ua = pick_ua(base_seed, attempt);

            // Fresh fbid per attempt.
            let fbid = make_fbid(nanos_seed() ^ (attempt as u64).wrapping_mul(0xdead_beef_cafe_1337));

            let url = format!(
                "https://html.duckduckgo.com/html/?q={}&kl={}&fbid={}",
                encoded_query, region, fbid
            );

            let (status, body) = match ddg_get(&url, ua, per_request_timeout) {
                Ok(v) => v,
                Err(e) => {
                    // Network-level error; no point retrying further.
                    return Ok(format!("error: {e}"));
                }
            };

            if !(200..300).contains(&status) && status != 202 {
                return Ok(format!("error: HTTP {status} from DuckDuckGo"));
            }

            let results = parse_ddg_results(&body);

            if looks_rate_limited(status, &body, results.len()) {
                // Will retry unless this was the last attempt.
                continue;
            }

            // We have results — format and return immediately.
            if !results.is_empty() {
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
                return Ok(out.trim_end().to_string());
            }

            // 200 OK but zero results — not a rate-limit, just empty.
            return Ok(format!(
                "web_search: no results found for query: {query}\n\
                (DuckDuckGo returned an empty page)"
            ));
        }

        // All attempts were rate-limited.
        Ok(format!(
            "web_search: DuckDuckGo rate-limited after {MAX_ATTEMPTS} attempts, \
             try again shortly or use web_fetch on a known URL."
        ))
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
