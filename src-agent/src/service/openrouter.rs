use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{APP_TITLE, HTTP_REFERER};
use crate::dto::chat::ChatMessage;
use crate::dto::openrouter::{ChatRequest, ChatResponse, StreamChunk};
use crate::service::{Generation, StreamEvent, TaggedEvent};

pub struct OpenRouterClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

fn emit(tx: &UnboundedSender<TaggedEvent>, generation: Generation, event: StreamEvent) {
    let _ = tx.send(TaggedEvent { generation, event });
}

impl OpenRouterClient {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            base_url,
            model,
        }
    }

    /// Streaming completion. Emits TaggedEvent { generation, Token/Done/Error }.
    /// Never panics; every error path emits Error then returns Ok(()).
    pub async fn stream_complete(
        &self,
        messages: Vec<ChatMessage>,
        generation: Generation,
        tx: UnboundedSender<TaggedEvent>,
    ) -> Result<()> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = ChatRequest {
            model: self.model.clone(),
            messages,
            stream: true,
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
                emit(&tx, generation, StreamEvent::Error(format!("request failed: {e}")));
                return Ok(());
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            emit(
                &tx,
                generation,
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
                    emit(&tx, generation, StreamEvent::Error(format!("stream error: {e}")));
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
                    emit(&tx, generation, StreamEvent::Done);
                    return Ok(());
                }
                if let Ok(c) = serde_json::from_str::<StreamChunk>(data) {
                    if let Some(choice) = c.choices.first() {
                        if let Some(t) = &choice.delta.content {
                            if !t.is_empty() {
                                emit(&tx, generation, StreamEvent::Token(t.clone()));
                            }
                        }
                    }
                }
                // unparseable JSON (partial keepalive) is ignored
            }
        }
        emit(&tx, generation, StreamEvent::Done); // stream ended without explicit [DONE]
        Ok(())
    }

    /// Non-stream completion (used by /compact). Returns assistant content.
    pub async fn complete(&self, messages: Vec<ChatMessage>) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = ChatRequest {
            model: self.model.clone(),
            messages,
            stream: false,
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
