//! Data-transfer objects (DTOs) used at the application boundary.
//!
//! - `chat` — core `Role` / `ChatMessage` types shared by both the model layer and the wire format.
//! - `openrouter` — serialisation shapes for the OpenRouter REST + SSE API.

pub mod chat;
pub mod openrouter;
