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
    ChatRequest, ChatResponse, StreamChunk, ToolDef, ToolFunctionDef, UsageRequest,
};
use crate::service::StreamEvent;

pub struct OpenRouterClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    provider: String,
}

/// Send one event on the request channel, ignoring a closed receiver (the
/// request was interrupted/superseded, so the event is simply dropped).
fn emit(tx: &UnboundedSender<StreamEvent>, event: StreamEvent) {
    let _ = tx.send(event);
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
    pub fn new(api_key: String, base_url: String, model: String, provider: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            base_url,
            model,
            provider,
        }
    }

    /// Build a provider-routing directive from the stored provider slug.
    ///
    /// Returns `None` when the slug is empty (OpenRouter default routing).
    /// Returns `Some(ProviderRouting)` with `allow_fallbacks: false` when a
    /// slug is set, strictly pinning the request to that provider.
    fn provider_routing(&self) -> Option<crate::dto::openrouter::ProviderRouting> {
        if self.provider.is_empty() {
            None
        } else {
            Some(crate::dto::openrouter::ProviderRouting {
                only: vec![self.provider.clone()],
                allow_fallbacks: false,
            })
        }
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
}
