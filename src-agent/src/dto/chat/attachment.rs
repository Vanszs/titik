//! [`Attachment`] — an image attached to a chat message.
//!
//! An attachment is a small record that LINKS a message to an on-disk image; the
//! image bytes themselves are NEVER stored in the message or `messages.json`.
//! They live under `<session>/images/NN-name.ext` (see
//! [`crate::model::store::session_images_dir`] + the ingest core in
//! [`crate::model::attachment`]). The record carries only the relative path, the
//! sniffed mime type, and the marker number `N` that ties it to the literal
//! `[Image #N]` token in the message text.
//!
//! The base64 data-URL the model receives is DERIVED from the on-disk file at
//! send time (see `to_wire_with_images`), so resume re-reads from disk and the
//! link survives across runs.

use serde::{Deserialize, Serialize};

/// One image attached to a [`super::ChatMessage`].
///
/// Persisted in `messages.json` (and the sqlite msglog row's content stays the
/// plain text, attachments ride the JSON message). The bytes are on disk under
/// the session's `images/` dir; this record is the durable link.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    /// The marker number `N` that matches the literal `[Image #N]` token in the
    /// owning message's `content`. Monotonic per session (file count + 1).
    pub marker_n: usize,
    /// Path RELATIVE to the session directory: `images/NN-name.ext`. Resolved
    /// against `<session>/` at send time to read the bytes back off disk.
    pub rel_path: String,
    /// Sniffed MIME type (e.g. `image/png`), used to build the `data:<mime>;…`
    /// URL prefix the model's `image_url` part needs.
    pub mime: String,
}

impl Attachment {
    /// The original on-disk basename (`NN-name.ext`) — the trailing path segment
    /// of `rel_path`. Shown in warn cards + the model-visible strip warning.
    pub fn file_name(&self) -> &str {
        self.rel_path
            .rsplit('/')
            .next()
            .unwrap_or(self.rel_path.as_str())
    }
}
