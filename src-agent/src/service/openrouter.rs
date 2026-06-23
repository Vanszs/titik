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
use crate::dto::chat::ChatMessage;
use crate::dto::openrouter::{ChatRequest, ChatResponse, StreamChunk};
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
        messages: Vec<ChatMessage>,
        tx: UnboundedSender<StreamEvent>,
    ) -> Result<()> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = ChatRequest {
            model: self.model.clone(),
            messages,
            stream: true,
            provider: self.provider_routing(),
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
                StreamEvent::Error(format!("OpenRouter error {status}: {text}")),
            );
            return Ok(());
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
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
                    emit(&tx, StreamEvent::Done);
                    return Ok(());
                }
                if let Ok(c) = serde_json::from_str::<StreamChunk>(data) {
                    if let Some(choice) = c.choices.first() {
                        if let Some(t) = &choice.delta.content {
                            if !t.is_empty() {
                                emit(&tx, StreamEvent::Token(t.clone()));
                            }
                        }
                    }
                }
                // unparseable JSON (partial keepalive) is ignored
            }
        }
        emit(&tx, StreamEvent::Done); // stream ended without explicit [DONE]
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
            return Err(anyhow!("OpenRouter error {status}: {text}"));
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
