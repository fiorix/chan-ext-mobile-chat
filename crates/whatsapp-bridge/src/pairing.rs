//! Pairing state machine, QR SVG rendering, and reconnect after the server
//! runs out of pairing refs.
//!
//! The QR payload is the raw comma-separated string from
//! `Event::PairingQrCode`, rendered to inline SVG with `fast_qr`. When all six
//! refs are exhausted the client must be rebuilt against the same persistence
//! manager, because `Client::disconnect()` is final for an instance.

use serde::Serialize;

/// The QR payload currently outstanding, if any.
#[derive(Debug, Clone, Serialize)]
pub struct QrCode {
    /// Raw `ref,noise_pub,identity_pub,adv_secret,client_type` string.
    pub raw: String,
    /// Inline SVG for the browser.
    pub svg: String,
    /// `https://wa.me/settings/linked_devices#` + raw, openable from a phone.
    pub deep_link: String,
}
