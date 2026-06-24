//! OpenRouter HTTP client: the only thing that talks to the network.
//!
//! Two entry points, both spawned as async tasks by the runtime:
//! - [`OpenRouterClient::stream_complete`] — chat streaming over SSE, emitting
//!   [`StreamEvent`]s down a per-request channel.
//! - [`OpenRouterClient::complete`] — one-shot completion (used by `/compact`).
//!
//! The client knows nothing about the UI; it just pushes `StreamEvent`s. A
//! dropped receiver makes every send a no-op, so an aborted/superseded request
//! cannot corrupt the next one.

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{APP_TITLE, HTTP_REFERER};
use crate::dto::chat::{sanitize_tool_arguments, ChatMessage, ToolCall};
use crate::dto::openrouter::{
    to_wire, ChatRequest, ChatResponse, ModelInfo, ModelsResponse, ReasoningConfig, StreamChunk,
    ToolDef, ToolFunctionDef, UsageRequest,
};
use crate::service::StreamEvent;

pub struct OpenRouterClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    provider: String,
    /// Reasoning/thinking effort for the interactive chat path. Free-form token
    /// (`""`/`"default"` = model default, `"off"`/`"none"` = thinking off, or an
    /// effort level like `"low"`/`"high"`); mapped to the request `reasoning`
    /// object by [`reasoning_config`]. Only [`Self::stream_complete`] applies it.
    effort: String,
    /// Whimsical plan lead-in word, chosen ONCE per client (= per session) in
    /// the constructor. [`Self::stream_complete`] injects this SAME word into the
    /// system message every request instead of rolling a fresh one each time —
    /// keeping the system prefix byte-stable across the session so OpenRouter
    /// prompt caching can hit. (A per-request random word busted the cache.)
    plan_word: String,
}

/// Send one event on the request channel, ignoring a closed receiver (the
/// request was interrupted/superseded, so the event is simply dropped).
fn emit(tx: &UnboundedSender<StreamEvent>, event: StreamEvent) {
    let _ = tx.send(event);
}

/// Repair every accumulated tool call's `function.arguments` in place via
/// [`sanitize_tool_arguments`] before the assembled set leaves the client.
///
/// Streamed argument fragments are concatenated assuming pure deltas; providers
/// that re-send the FULL arguments per chunk (common on budget routes) make that
/// concatenation a malformed `{...}{...}` string. Collapsing it to one clean value
/// here keeps the bad string from entering the runtime/persistence pipeline — the
/// SOURCE-layer guard. A single clean value is left semantically unchanged.
fn sanitize_tool_acc(tool_acc: &mut [ToolCall]) {
    for call in tool_acc.iter_mut() {
        call.function.arguments = sanitize_tool_arguments(&call.function.arguments);
    }
}

/// Build a provider-routing directive from a provider slug.
///
/// Returns `None` for an empty slug (OpenRouter default routing) and
/// `Some(ProviderRouting)` with `allow_fallbacks: false` otherwise, strictly
/// pinning the request to that single provider. Free helper so every request
/// path (streaming, `complete`, `complete_with`) shares one routing rule.
fn provider_routing_for(provider: &str) -> Option<crate::dto::openrouter::ProviderRouting> {
    if provider.is_empty() {
        None
    } else {
        Some(crate::dto::openrouter::ProviderRouting {
            only: vec![provider.to_string()],
            allow_fallbacks: false,
        })
    }
}

/// Map a stored effort token to the request `reasoning` object.
///
/// - `""` / `"default"` → `None`: omit `reasoning` entirely so the model uses
///   its own default thinking behaviour.
/// - `"off"` / `"none"` → `Some(enabled: false)`: turn thinking off.
/// - any effort token (`minimal`/`low`/`medium`/`high`/`xhigh`/`max`/…) →
///   `Some(effort: <token>)`. `effort` and `enabled` are mutually exclusive, so
///   only `effort` is set here.
///
/// Free helper (not a method) so it has no hidden state — what you pass is what
/// you get. Applied only on the interactive chat path.
fn reasoning_config(effort: &str) -> Option<ReasoningConfig> {
    match effort.trim() {
        "" | "default" => None,
        "off" | "none" => Some(ReasoningConfig {
            effort: None,
            enabled: Some(false),
            exclude: None,
        }),
        level => Some(ReasoningConfig {
            effort: Some(level.to_string()),
            enabled: None,
            exclude: None,
        }),
    }
}

/// Derived reasoning capability for a single model, used to build the `/effort`
/// menu conditionally.
///
/// - `supported`: the model exposes any reasoning control at all.
/// - `mandatory`: reasoning can't be turned off (no "off" option offered).
/// - `efforts`: discrete effort tokens the model accepts, in the order the API
///   reported them; empty means on/off-only (or unreported).
pub struct EffortCaps {
    pub supported: bool,
    pub mandatory: bool,
    pub efforts: Vec<String>,
}

/// Derive [`EffortCaps`] for `model_id` from a `GET /models` listing.
///
/// Matches the model by exact `id`. Reasoning is considered supported when the
/// model carries a `reasoning` object OR advertises `reasoning` /
/// `include_reasoning` in its `supported_parameters`. The effort list and the
/// mandatory flag come from the `reasoning` object when present. A model absent
/// from the listing yields `supported = false` so the caller can fall back.
pub fn effort_caps(models: &[ModelInfo], model_id: &str) -> EffortCaps {
    let Some(info) = models.iter().find(|m| m.id == model_id) else {
        return EffortCaps {
            supported: false,
            mandatory: false,
            efforts: Vec::new(),
        };
    };
    let has_param = info
        .supported_parameters
        .iter()
        .any(|p| p == "reasoning" || p == "include_reasoning");
    let supported = info.reasoning.is_some() || has_param;
    let efforts = info
        .reasoning
        .as_ref()
        .map(|r| r.supported_efforts.clone())
        .unwrap_or_default();
    let mandatory = info.reasoning.as_ref().map(|r| r.mandatory).unwrap_or(false);
    EffortCaps {
        supported,
        mandatory,
        efforts,
    }
}

/// Return the per-token pricing for `model_id` from a `GET /models` listing.
///
/// Returns `Some((prompt_price, completion_price))` in USD-per-token when the
/// model is found and both pricing strings parse as finite `f64` values. Returns
/// `None` when the model is absent from the listing, has no `pricing` object, or
/// either price string is missing / unparseable. The caller treats `None` as
/// "pricing unknown" and falls back to showing no live estimate. Never panics.
pub fn pricing_for(models: &[ModelInfo], model_id: &str) -> Option<(f64, f64)> {
    let pricing = models
        .iter()
        .find(|m| m.id == model_id)
        .and_then(|m| m.pricing.as_ref())?;
    let prompt = pricing.prompt.trim().parse::<f64>().ok()?;
    let completion = pricing.completion.trim().parse::<f64>().ok()?;
    if prompt.is_finite() && completion.is_finite() {
        Some((prompt, completion))
    } else {
        None
    }
}

/// Return the context-window size (tokens) for `model_id` from a `GET /models`
/// listing. Returns `None` when the model is absent from the listing or its
/// `context_length` field was not reported. The caller falls back to a hardcoded
/// default when `None` is returned — never panics.
///
/// Prefers `top_provider.context_length` (the limit the serving provider
/// actually enforces) over the nominal top-level `context_length` (the
/// model's theoretical maximum). Falls back to the nominal value when the
/// `top_provider` object is absent or its `context_length` is not reported.
pub fn context_length_for(models: &[ModelInfo], model_id: &str) -> Option<u64> {
    models
        .iter()
        .find(|m| m.id == model_id)
        .and_then(|model| {
            model
                .top_provider
                .as_ref()
                .and_then(|tp| tp.context_length)
                .or(model.context_length)
        })
}

/// Turn an OpenRouter error response body into a short human-readable message.
/// OpenRouter returns `{"error":{"message":..,"code":..,"metadata":{"raw":..}}}`;
/// the upstream provider's own text lives in `metadata.raw`, so prefer that, then
/// `message`, then a trimmed slice of the raw body. `status` renders as e.g.
/// "429 Too Many Requests".
fn clean_error(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        let err = &v["error"];
        let raw = err["metadata"]["raw"].as_str().unwrap_or("");
        let msg = err["message"].as_str().unwrap_or("");
        let detail = if !raw.is_empty() { raw } else { msg };
        if !detail.is_empty() {
            let detail: String = detail.chars().take(200).collect();
            return format!("{status}: {detail}");
        }
    }
    let trimmed: String = body.chars().take(160).collect();
    if trimmed.trim().is_empty() {
        format!("{status}")
    } else {
        format!("{status}: {trimmed}")
    }
}

impl OpenRouterClient {
    pub fn new(
        api_key: String,
        base_url: String,
        model: String,
        provider: String,
        effort: String,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            base_url,
            model,
            provider,
            effort,
            // Pick the plan lead-in ONCE here so every request in this session
            // injects the same word → the cached system prefix stays byte-stable.
            plan_word: crate::resources::wanderer_word(),
        }
    }

    /// Build a provider-routing directive from this client's stored provider
    /// slug. Thin wrapper over [`provider_routing_for`] for the session model.
    fn provider_routing(&self) -> Option<crate::dto::openrouter::ProviderRouting> {
        provider_routing_for(&self.provider)
    }

    /// The whimsical plan lead-in word chosen once per client (= per session) in
    /// the constructor. Exposed so the runtime can inject the SAME steer into the
    /// system message every request (inside the cached head), keeping the cached
    /// prefix byte-stable so prompt caching can hit.
    pub fn plan_word(&self) -> &str {
        &self.plan_word
    }

    /// Streaming chat completion over Server-Sent Events.
    ///
    /// POSTs with `stream: true`, then reads the byte stream line-by-line:
    /// bytes are buffered until a `\n`, each complete line is stripped of its
    /// `data:` prefix, and the JSON payload is parsed into a `StreamChunk`.
    /// Each non-empty delta is emitted as [`StreamEvent::Token`]; a `[DONE]`
    /// sentinel (or stream EOF) emits [`StreamEvent::Done`]. Non-`data:` lines
    /// (SSE comments / keepalives) and unparseable partial JSON are skipped.
    ///
    /// Never panics: every failure emits [`StreamEvent::Error`] and returns
    /// `Ok(())`. The caller (a spawned task) discards the return value.
    pub async fn stream_complete(
        &self,
        messages: Vec<ChatMessage>,
        tx: UnboundedSender<StreamEvent>,
    ) -> Result<()> {
        // The plan-word steer is now injected into the System message upstream in
        // `start_stream_task`, BEFORE the volatile project-files/awareness tail and
        // ahead of the `CACHE_SPLIT_MARK` boundary, so it stays inside the cached
        // (byte-stable) head. `to_wire` splits the System content on that mark and
        // puts the cache breakpoint on the head only.
        let url = format!("{}/chat/completions", self.base_url);
        // Expose the built-in tool set to the model. Each tool maps to an
        // OpenAI/OpenRouter `function` definition (name + description + raw
        // JSON-Schema parameters).
        let tools: Vec<ToolDef> = crate::tool::all_tools()
            .iter()
            .map(|t| ToolDef {
                kind: "function".into(),
                function: ToolFunctionDef {
                    name: t.name().into(),
                    description: t.description().into(),
                    parameters: t.parameters(),
                },
            })
            .collect();
        let body = ChatRequest {
            model: self.model.clone(),
            // Wrap into wire messages: the system message gets the single prompt-
            // caching breakpoint; everything else serialises as a plain string.
            messages: to_wire(messages),
            stream: true,
            provider: self.provider_routing(),
            usage: UsageRequest { include: true },
            tools: Some(tools),
            // Interactive chat is the only path that thinks; map the session's
            // effort token to a `reasoning` directive (None = model default).
            reasoning: reasoning_config(&self.effort),
            // Free-form text reply; structured output is classifier-only.
            response_format: None,
            // Generous runaway cap for the interactive path.
            max_tokens: Some(32_000),
        };

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", HTTP_REFERER)
            .header("X-Title", APP_TITLE)
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                emit(&tx, StreamEvent::Error(format!("request failed: {e}")));
                return Ok(());
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            emit(
                &tx,
                StreamEvent::Error(clean_error(status, &text)),
            );
            return Ok(());
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        // Tool calls stream across many frames, one (or more) per `index`. Each
        // frame contributes the id / name once and appends argument fragments;
        // we merge them here and emit the assembled set at finalisation.
        let mut tool_acc: Vec<ToolCall> = Vec::new();
        // Last `finish_reason` seen on the active choice. OpenAI/OpenRouter set
        // it to `"tool_calls"` on the frame that closes a tool-calling turn; we
        // record it so finalisation can confirm the model wants tools run.
        let mut finished_tool_calls = false;
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    emit(&tx, StreamEvent::Error(format!("stream error: {e}")));
                    return Ok(());
                }
            };
            buf.extend_from_slice(&bytes);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim_end(); // strip trailing \r\n
                if line.is_empty() {
                    continue; // SSE event separator
                }
                let data = match line
                    .strip_prefix("data: ")
                    .or_else(|| line.strip_prefix("data:"))
                {
                    Some(d) => d.trim(),
                    None => continue, // comments/keepalive
                };
                if data == "[DONE]" {
                    // Finalise: any accumulated tool calls go out just before
                    // Done so the runtime can run them. The `finished_tool_calls`
                    // flag (finish_reason == "tool_calls") is the protocol-level
                    // confirmation; non-empty `tool_acc` is the data we actually
                    // need, so either being set means "run the tools".
                    if !tool_acc.is_empty() || finished_tool_calls {
                        // Repair argument strings before they leave the client:
                        // some providers re-send the FULL arguments per chunk, so
                        // blind delta concatenation yields `{...}{...}`. Collapse
                        // to one clean value so the runtime + persistence never see
                        // a malformed (and later prefill-rejected) string.
                        sanitize_tool_acc(&mut tool_acc);
                        emit(&tx, StreamEvent::ToolCalls(tool_acc.clone()));
                    }
                    emit(&tx, StreamEvent::Done);
                    return Ok(());
                }
                if let Ok(c) = serde_json::from_str::<StreamChunk>(data) {
                    // A chunk carries content / tool-call deltas OR usage (the
                    // terminal chunk has an empty `choices` array + a `usage`
                    // object). Handle each independently so a usage-bearing
                    // chunk isn't skipped.
                    if let Some(choice) = c.choices.first() {
                        if choice.finish_reason.as_deref() == Some("tool_calls") {
                            finished_tool_calls = true;
                        }
                        if let Some(t) = &choice.delta.content {
                            if !t.is_empty() {
                                emit(&tx, StreamEvent::Token(t.clone()));
                            }
                        }
                        // Reasoning rides a separate delta channel (present only
                        // when reasoning is enabled); accumulate it as a display-
                        // only block, mirroring the content handling above.
                        if let Some(r) = &choice.delta.reasoning {
                            if !r.is_empty() {
                                emit(&tx, StreamEvent::Reasoning(r.clone()));
                            }
                        }
                        if let Some(tcs) = &choice.delta.tool_calls {
                            for d in tcs {
                                // Grow the accumulator so `index` is in range,
                                // then merge this fragment into its slot.
                                while tool_acc.len() <= d.index {
                                    tool_acc.push(ToolCall {
                                        kind: "function".into(),
                                        ..Default::default()
                                    });
                                }
                                let acc = &mut tool_acc[d.index];
                                if let Some(id) = &d.id {
                                    acc.id = id.clone();
                                }
                                if let Some(f) = &d.function {
                                    if let Some(n) = &f.name {
                                        acc.function.name = n.clone();
                                    }
                                    if let Some(a) = &f.arguments {
                                        acc.function.arguments.push_str(a);
                                    }
                                }
                            }
                        }
                    }
                    if let Some(u) = c.usage {
                        // Cache hit count lives in the optional details object;
                        // absent/null → 0 (cold prefix or no cache reporting).
                        let cached_tokens = u
                            .prompt_tokens_details
                            .as_ref()
                            .map(|d| d.cached_tokens)
                            .unwrap_or(0);
                        emit(
                            &tx,
                            StreamEvent::Usage {
                                prompt_tokens: u.prompt_tokens,
                                completion_tokens: u.completion_tokens,
                                cached_tokens,
                                cost: u.cost,
                            },
                        );
                    }
                }
                // unparseable JSON (partial keepalive) is ignored
            }
        }
        // Stream ended without an explicit [DONE]: same finalisation order —
        // tool calls (if any), then Done. Same argument repair as the [DONE]
        // path so a non-delta provider that never sends [DONE] is also covered.
        if !tool_acc.is_empty() || finished_tool_calls {
            sanitize_tool_acc(&mut tool_acc);
            emit(&tx, StreamEvent::ToolCalls(tool_acc.clone()));
        }
        emit(&tx, StreamEvent::Done);
        Ok(())
    }

    /// Non-stream completion (used by /compact). Returns assistant content.
    pub async fn complete(&self, messages: Vec<ChatMessage>) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = ChatRequest {
            model: self.model.clone(),
            messages: to_wire(messages),
            stream: false,
            provider: self.provider_routing(),
            usage: UsageRequest { include: true },
            // /compact summarisation uses no tools.
            tools: None,
            // Compaction is a mechanical summary; no thinking needed.
            reasoning: None,
            // Free-form summary text; structured output is classifier-only.
            response_format: None,
            // No cap on compaction: the summary length is bounded by the prompt.
            max_tokens: None,
        };

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", HTTP_REFERER)
            .header("X-Title", APP_TITLE)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("{}", clean_error(status, &text)));
        }

        let chat_response: ChatResponse = response.json().await?;
        chat_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content.unwrap_or_default())
            .ok_or_else(|| anyhow!("no choices returned"))
    }

    /// One-off non-streaming completion against a DIFFERENT model/provider,
    /// reusing this client's http + api_key + base_url. provider "" = default
    /// routing.
    ///
    /// Generic helper for secondary-model calls (project-awareness summaries
    /// today; a future request classifier reuses the same path). Builds the
    /// same body `complete` does — no tools, `stream: false`, usage on — but
    /// with the caller's `model` and provider pin. Returns the assistant
    /// content; clean errors, no panics.
    pub async fn complete_with(
        &self,
        model: &str,
        provider: &str,
        messages: Vec<ChatMessage>,
    ) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = ChatRequest {
            model: model.to_string(),
            messages: to_wire(messages),
            stream: false,
            provider: provider_routing_for(provider),
            usage: UsageRequest { include: true },
            // Secondary-model calls use no tools.
            tools: None,
            // Secondary-model calls (awareness / classifier) don't think.
            reasoning: None,
            // Free-form reply; structured output is classifier-only.
            response_format: None,
            // No cap: awareness summaries can be long.
            max_tokens: None,
        };

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", HTTP_REFERER)
            .header("X-Title", APP_TITLE)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("{}", clean_error(status, &text)));
        }

        let chat_response: ChatResponse = response.json().await?;
        chat_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content.unwrap_or_default())
            .ok_or_else(|| anyhow!("no choices returned"))
    }

    /// Classifier completion against a DIFFERENT model/provider — the dedicated
    /// path for the safety harness, kept separate from [`Self::complete_with`] so
    /// the awareness summary path is unaffected.
    ///
    /// Same body as `complete_with` (no tools, `stream: false`, usage on, provider
    /// pin from `provider`) but tuned for a deterministic, fast, machine-parseable
    /// verdict:
    /// - `reasoning: {enabled: false}` turns thinking OFF. The safeguard model
    ///   (`gpt-oss-safeguard-20b`) can reason, but a free-form thinking pass made
    ///   the reply slow and unstructured; off is deterministic, fast, and fills
    ///   `content` directly. `effort` and `enabled` are mutually exclusive — only
    ///   `enabled` is set.
    /// - `response_format` pins a STRICT `json_schema` (`{allow, reason}`,
    ///   `additionalProperties:false`) so the model must return exactly the
    ///   verdict object as JSON. The safeguard model advertises both
    ///   `response_format` and `structured_outputs`, so this is honoured.
    ///
    /// Returns the raw reply for the caller to parse: `message.content` (the JSON
    /// string) when non-empty, else `message.reasoning` (defensive — should be
    /// empty with thinking off), else an error. The HTTP-error path returns
    /// `Err(clean_error(..))` carrying the upstream text — that reason now matters
    /// because the caller surfaces it. Clean errors, no panics.
    pub async fn classify_with(
        &self,
        model: &str,
        provider: &str,
        messages: Vec<ChatMessage>,
    ) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        // Strict JSON-schema for the verdict object. `strict: true` +
        // `additionalProperties: false` force the model to emit exactly
        // `{"allow": <bool>, "reason": <string>}` and nothing else.
        let response_format = serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "verdict",
                "strict": true,
                "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["allow", "reason"],
                    "properties": {
                        "allow": { "type": "boolean" },
                        "reason": { "type": "string" }
                    }
                }
            }
        });
        let body = ChatRequest {
            model: model.to_string(),
            messages: to_wire(messages),
            stream: false,
            provider: provider_routing_for(provider),
            usage: UsageRequest { include: true },
            // Classifier calls use no tools.
            tools: None,
            // Thinking excluded: `exclude: true` keeps reasoning mandatory for
            // endpoints that require it, but strips the `reasoning` field from the
            // response — deterministic, fast, bleed-proof, verdict lands in `content`.
            reasoning: Some(ReasoningConfig {
                effort: None,
                enabled: None,
                exclude: Some(true),
            }),
            // Force the verdict object as strict JSON.
            response_format: Some(response_format),
            // Classifier returns a tiny JSON object; cap prevents runaway.
            max_tokens: Some(2_000),
        };

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", HTTP_REFERER)
            .header("X-Title", APP_TITLE)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("{}", clean_error(status, &text)));
        }

        let chat_response: ChatResponse = response.json().await?;
        let message = chat_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| anyhow!("no choices returned"))?;
        // `exclude: true` means no `reasoning` field is returned; content-only.
        // `content` may be null on some models — treat null/absent as empty.
        let content = message.content.as_deref().unwrap_or("").trim();
        if !content.is_empty() {
            return Ok(content.to_string());
        }
        Err(anyhow!("empty classifier reply"))
    }

    /// Rolling-summary "fold" completion against a DIFFERENT model/provider — the
    /// dedicated path for the short-send incremental summary (P2), kept separate
    /// from [`Self::complete_with`] so the awareness path is unaffected.
    ///
    /// Takes the fold system prompt + the pre-built user payload directly (a plain
    /// two-message request) rather than a message vec, since the caller always
    /// sends exactly system + user. Same body shape as `complete_with` (no tools,
    /// `stream: false`, usage on, provider pin from `provider`) with two critical
    /// differences:
    /// - `reasoning: {exclude: true}` keeps reasoning mandatory for endpoints that
    ///   require it, but strips the `reasoning` field from the response. The summary
    ///   is PERSISTED and replayed forever — a CoT bleed would poison the
    ///   conversation permanently. Bleed-proof, verdict lands in `content`.
    /// - `response_format` pins a STRICT `json_schema` (`{summary: string}`,
    ///   `additionalProperties: false`) so even weak/4B models must emit exactly
    ///   the summary object as JSON — never a verdict, refusal, or meta-commentary.
    ///
    /// Returns the clean summary string extracted from `{"summary": "..."}`. On
    /// parse failure or an empty `summary` field the function returns
    /// `Err(anyhow!("unparseable summary"))` — no fallback to raw content or the
    /// reasoning field (a model that ignores the schema fails-open; the caller
    /// `update_summary` already swallows the error via `let _ =`, so no summary
    /// is written that turn — acceptable). Clean errors, no panics.
    pub async fn summarize_fold(
        &self,
        model: &str,
        provider: Option<&str>,
        system_prompt: &str,
        user_payload: &str,
    ) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        // Strict JSON-schema for the summary object. `strict: true` +
        // `additionalProperties: false` force the model to emit exactly
        // `{"summary": "<text>"}` and nothing else.
        let response_format = serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "rolling_summary",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string" }
                    },
                    "required": ["summary"],
                    "additionalProperties": false
                }
            }
        });
        let messages = vec![
            ChatMessage::new(crate::dto::chat::Role::System, system_prompt),
            ChatMessage::new(crate::dto::chat::Role::User, user_payload),
        ];
        let body = ChatRequest {
            model: model.to_string(),
            messages: to_wire(messages),
            stream: false,
            // `provider_routing_for` treats "" as default routing; a `None`
            // provider behaves the same (no pin).
            provider: provider_routing_for(provider.unwrap_or("")),
            usage: UsageRequest { include: true },
            // Fold calls use no tools.
            tools: None,
            // Thinking excluded: `exclude: true` keeps reasoning mandatory for
            // endpoints that require it, but strips the `reasoning` field from the
            // response. The summary is PERSISTED and replayed forever — a CoT bleed
            // would poison the conversation permanently, so the `reasoning` fallback
            // is intentionally absent here. Content-only, bleed-proof.
            reasoning: Some(ReasoningConfig {
                effort: None,
                enabled: None,
                exclude: Some(true),
            }),
            // Force the summary object as strict JSON.
            response_format: Some(response_format),
            // No cap: fold summaries can be proportionally sized.
            max_tokens: None,
        };

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", HTTP_REFERER)
            .header("X-Title", APP_TITLE)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("{}", clean_error(status, &text)));
        }

        let chat_response: ChatResponse = response.json().await?;
        let message = chat_response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| anyhow!("no choices returned"))?;
        // Strict-JSON extraction: parse `message.content` as `{"summary": "..."}`.
        // No fallback to raw content or the reasoning field — a model that ignores
        // the schema fails-open (the caller swallows the error), which is the correct
        // behaviour: better to skip one turn's summary than to persist garbage.
        let content = message.content.as_deref().unwrap_or("").trim();
        let parsed: serde_json::Value =
            serde_json::from_str(content).map_err(|_| anyhow!("unparseable summary"))?;
        let summary = parsed
            .get("summary")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("unparseable summary"))?;
        Ok(summary.to_string())
    }

    /// Blob-rehydrate router completion against a DIFFERENT model/provider — the
    /// dedicated path for the short-send retrieval router (P3), kept separate from
    /// [`Self::complete_with`] so the awareness/summary paths are unaffected.
    ///
    /// Takes the router system prompt + a pre-built user payload (the latest user
    /// message plus the candidate blob list) and returns the ids of the blobs whose
    /// full content the router judged necessary. Same body shape as `classify_with`
    /// (no tools, `stream: false`, usage on, provider pin from `provider`):
    /// - `reasoning: {enabled: false}` turns thinking OFF — deterministic, fast,
    ///   and the verdict lands in `content`. `effort` and `enabled` are mutually
    ///   exclusive — only `enabled` is set.
    /// - `response_format` pins a STRICT `json_schema` (`{blob_ids: integer[]}`)
    ///   so the model must return exactly the id list as JSON.
    ///
    /// BLEED GUARD: thinking is off and the reply is parsed as JSON only; no
    /// chain-of-thought is ever read or persisted. The returned ids merely select
    /// already-clean message content from sqlite to rehydrate.
    ///
    /// Best-effort: on ANY error (HTTP failure, empty reply, unparseable JSON) this
    /// returns `Ok(vec![])` so the caller simply rehydrates nothing rather than
    /// breaking the send. The selection is content-or-reasoning extracted, like the
    /// other secondary-model paths.
    pub async fn pick_blobs(
        &self,
        model: &str,
        provider: &str,
        system_prompt: &str,
        user_payload: &str,
    ) -> Result<Vec<i64>> {
        let url = format!("{}/chat/completions", self.base_url);
        // Strict JSON-schema for the id list. `strict: true` +
        // `additionalProperties: false` force the model to emit exactly
        // `{"blob_ids": [<integer>, ...]}` and nothing else.
        let response_format = serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "blob_selection",
                "strict": true,
                "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["blob_ids"],
                    "properties": {
                        "blob_ids": {
                            "type": "array",
                            "items": { "type": "integer" }
                        }
                    }
                }
            }
        });
        let messages = vec![
            ChatMessage::new(crate::dto::chat::Role::System, system_prompt),
            ChatMessage::new(crate::dto::chat::Role::User, user_payload),
        ];
        let body = ChatRequest {
            model: model.to_string(),
            messages: to_wire(messages),
            stream: false,
            provider: provider_routing_for(provider),
            usage: UsageRequest { include: true },
            // Router calls use no tools.
            tools: None,
            // Thinking excluded: `exclude: true` keeps reasoning mandatory for
            // endpoints that require it, but strips the `reasoning` field from the
            // response — deterministic, fast, bleed-proof, id list lands in `content`.
            reasoning: Some(ReasoningConfig {
                effort: None,
                enabled: None,
                exclude: Some(true),
            }),
            // Force the id list as strict JSON.
            response_format: Some(response_format),
            // Picker returns a tiny JSON object; cap prevents runaway.
            max_tokens: Some(2_000),
        };

        // Best-effort: any failure returns an empty selection rather than erroring.
        let response = match self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", HTTP_REFERER)
            .header("X-Title", APP_TITLE)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return Ok(Vec::new()),
        };

        if !response.status().is_success() {
            return Ok(Vec::new());
        }

        let chat_response: ChatResponse = match response.json().await {
            Ok(r) => r,
            Err(_) => return Ok(Vec::new()),
        };
        let Some(message) = chat_response.choices.into_iter().next().map(|c| c.message) else {
            return Ok(Vec::new());
        };
        // Prefer `content`; fall back to `reasoning` (some models leave `content`
        // empty/null and put the answer there even with thinking off). Either way
        // it must be the strict JSON object — we never read a CoT.
        let raw = {
            let content = message.content.as_deref().unwrap_or("").trim();
            if !content.is_empty() {
                content.to_string()
            } else {
                message
                    .reasoning
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            }
        };
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        // Parse `{"blob_ids": [..]}` and return the ids. Unparseable → empty.
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new()),
        };
        let ids = parsed
            .get("blob_ids")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|x| x.as_i64()).collect())
            .unwrap_or_default();
        Ok(ids)
    }

    /// Fetch the OpenRouter model catalogue (`GET /models`).
    ///
    /// Drives the `/effort` capability menu: the returned [`ModelInfo`] list is
    /// passed to [`effort_caps`] to decide which options the current model
    /// supports. The endpoint needs no auth, but we send the bearer header
    /// anyway for consistency with the other calls. Returns the `data` array;
    /// clean errors, no panics. Callers treat any `Err` as "capabilities
    /// unknown" and fall back to a generic menu.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/models", self.base_url);
        let response = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", HTTP_REFERER)
            .header("X-Title", APP_TITLE)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("{}", clean_error(status, &text)));
        }

        let models: ModelsResponse = response.json().await?;
        Ok(models.data)
    }
}
