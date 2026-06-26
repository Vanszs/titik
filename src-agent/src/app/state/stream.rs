//! Streaming-lifecycle methods on [`super::AppStateRest`].
//!
//! Covers the per-turn streaming buffer (`begin_stream`, `append_token`,
//! `take_stream`) and the parallel reasoning buffer (`append_reasoning`,
//! `take_reasoning`).

use super::rest::AppStateRest;

impl AppStateRest {
    /// Streaming lifecycle methods.
    pub fn begin_stream(&mut self) {
        self.streaming = Some(String::new());
        // Arm the parallel reasoning buffer fresh so the previous round's
        // thinking can never bleed into this one.
        self.stream_reasoning.clear();
    }

    pub fn append_token(&mut self, t: &str) {
        if let Some(buf) = self.streaming.as_mut() {
            buf.push_str(t);
        }
    }

    /// Append a reasoning fragment to the parallel thinking buffer (driven by
    /// `StreamEvent::Reasoning`, mirroring `append_token` for content).
    pub fn append_reasoning(&mut self, t: &str) {
        self.stream_reasoning.push_str(t);
    }

    pub fn take_stream(&mut self) -> Option<String> {
        self.streaming.take()
    }

    /// Take the accumulated reasoning buffer, clearing it. Returns `Some` only
    /// when non-empty so an empty thinking block never attaches to a message.
    /// Always clears (alongside `take_stream`) so reasoning can't leak forward.
    pub fn take_reasoning(&mut self) -> Option<String> {
        if self.stream_reasoning.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.stream_reasoning))
        }
    }
}
