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
use crate::dto::chat::{ChatMessage, ToolCall};
use crate::dto::openrouter::{
    ChatRequest, ChatResponse, ModelInfo, ModelsResponse, ReasoningConfig, StreamChunk, ToolDef,
    ToolFunctionDef, UsageRequest,
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
}

/// Send one event on the request channel, ignoring a closed receiver (the
/// request was interrupted/superseded, so the event is simply dropped).
fn emit(tx: &UnboundedSender<StreamEvent>, event: StreamEvent) {
    let _ = tx.send(event);
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
        }),
        level => Some(ReasoningConfig {
            effort: Some(level.to_string()),
            enabled: None,
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
        }
    }

    /// Build a provider-routing directive from this client's stored provider
    /// slug. Thin wrapper over [`provider_routing_for`] for the session model.
    fn provider_routing(&self) -> Option<crate::dto::openrouter::ProviderRouting> {
        provider_routing_for(&self.provider)
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
        mut messages: Vec<ChatMessage>,
        tx: UnboundedSender<StreamEvent>,
    ) -> Result<()> {
        // Steer the model to lead its FIRST plan with a random whimsical corpus
        // word (instead of "Plan:"). Injected per request into the System
        // message so the model plans up front and the runtime needs no separate
        // "plan first" nudge round in the common case.
        if let Some(first) = messages.first_mut() {
            if first.role == crate::dto::chat::Role::System {
                let word = crate::resources::wanderer_word();
                first.content.push_str(&format!(
                    "\n\nWhen you write your plan for this task, lead with the single word \"{word}:\" (a whimsical lead-in) instead of \"Plan:\"."
                ));
            }
        }
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
            messages,
            stream: true,
            provider: self.provider_routing(),
            usage: UsageRequest { include: true },
            tools: Some(tools),
            // Interactive chat is the only path that thinks; map the session's
            // effort token to a `reasoning` directive (None = model default).
            reasoning: reasoning_config(&self.effort),
            // Free-form text reply; structured output is classifier-only.
            response_format: None,
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
                        emit(
                            &tx,
                            StreamEvent::Usage {
                                prompt_tokens: u.prompt_tokens,
                                completion_tokens: u.completion_tokens,
                                cost: u.cost,
                            },
                        );
                    }
                }
                // unparseable JSON (partial keepalive) is ignored
            }
        }
        // Stream ended without an explicit [DONE]: same finalisation order —
        // tool calls (if any), then Done.
        if !tool_acc.is_empty() || finished_tool_calls {
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
            messages,
            stream: false,
            provider: self.provider_routing(),
            usage: UsageRequest { include: true },
            // /compact summarisation uses no tools.
            tools: None,
            // Compaction is a mechanical summary; no thinking needed.
            reasoning: None,
            // Free-form summary text; structured output is classifier-only.
            response_format: None,
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
            .map(|c| c.message.content)
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
            messages,
            stream: false,
            provider: provider_routing_for(provider),
            usage: UsageRequest { include: true },
            // Secondary-model calls use no tools.
            tools: None,
            // Secondary-model calls (awareness / classifier) don't think.
            reasoning: None,
            // Free-form reply; structured output is classifier-only.
            response_format: None,
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
            .map(|c| c.message.content)
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
            messages,
            stream: false,
            provider: provider_routing_for(provider),
            usage: UsageRequest { include: true },
            // Classifier calls use no tools.
            tools: None,
            // Thinking OFF: deterministic + fast, and the verdict lands in
            // `content`. `effort` and `enabled` are mutually exclusive — only
            // `enabled` is set.
            reasoning: Some(ReasoningConfig {
                effort: None,
                enabled: Some(false),
            }),
            // Force the verdict object as strict JSON.
            response_format: Some(response_format),
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
        // Prefer `content`; fall back to the model's `reasoning` (the verdict may
        // live in the thinking for a reasoning model). Error only if BOTH empty.
        let content = message.content.trim();
        if !content.is_empty() {
            return Ok(content.to_string());
        }
        if let Some(reasoning) = message.reasoning {
            let reasoning = reasoning.trim();
            if !reasoning.is_empty() {
                return Ok(reasoning.to_string());
            }
        }
        Err(anyhow!("empty classifier reply"))
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
