//! whatsapp-rust wiring: Bot construction, the [`WaSink`] implementation over
//! `whatsapp_rust::Client`, and conversion of whatsapp-rust events into the
//! bridge's internal inbound types.
//!
//! [`WaSink`] is the seam that keeps routing, allowlist, rotation and chunking
//! logic under unit test: the real implementation wraps the WhatsApp client,
//! tests use a stub that records what would have been sent.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A WhatsApp chat identifier, bare (`...@s.whatsapp.net`) or group
/// (`...@g.us`). Newtype so jid strings do not leak into the routing layers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChatId(pub String);

/// An inbound WhatsApp message converted to the bridge's own shape at the
/// edge, so the routing and logging layers never touch whatsapp-rust types.
#[derive(Debug, Clone)]
pub struct Inbound {
    pub chat: ChatId,
    pub sender: String,
    pub push_name: String,
    /// RFC 3339 UTC, from `MessageInfo::timestamp`.
    pub timestamp: String,
    pub text: String,
    pub from_me: bool,
}

/// Reference to an attachment of an inbound message, enough for the sink to
/// download it without holding whatsapp-rust message types.
#[derive(Debug, Clone)]
pub struct MediaRef {
    pub kind: String,
}

/// Sending and downloading through a small async trait, so tests substitute a
/// recording stub for the live WhatsApp client. Written in
/// return-position-impl-trait form so the trait stays object-safe-free and
/// warning-free on stable.
pub trait WaSink: Send + Sync {
    /// Send `text` to `chat`; returns the WhatsApp message id.
    fn send_text(
        &self,
        chat: &ChatId,
        text: &str,
    ) -> impl std::future::Future<Output = Result<String>> + Send;
    /// Stream `media` into `to`; returns bytes written.
    fn download(
        &self,
        media: &MediaRef,
        to: &Path,
    ) -> impl std::future::Future<Output = Result<u64>> + Send;
}
