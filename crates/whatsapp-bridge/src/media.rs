//! Type and size gating, destination paths, streaming download.
//!
//! A media attachment is stored only when its kind is in the configured
//! `media` list and its size is under `media_max_bytes`; downloads stream
//! through `WaSink::download` so memory stays flat regardless of file size.
//! Stored paths are `media/<kind>/<unix>-<msgid>.<ext>`, relative to the chat
//! directory so logs stay portable.

use serde::Serialize;

/// The media kinds the bridge recognizes, matching the `media` config list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Document,
    Sticker,
}

impl MediaKind {
    /// Config-table spelling (`"image"`, `"video"`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Sticker => "sticker",
        }
    }
}
