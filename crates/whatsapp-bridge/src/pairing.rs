//! Pairing state machine, QR SVG rendering, and reconnect after the server
//! runs out of pairing refs.
//!
//! The QR payload is the raw comma-separated string from
//! `Event::PairingQrCode`, rendered to inline SVG with `fast_qr`. The server
//! hands out six refs per connection (60s for the first, 20s for each of the
//! other five); when they run out the client must be rebuilt against the same
//! `session.db`, because `Client::disconnect()` is final for an instance.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{mpsc, watch};

use crate::client::{self, ConnEvent, EVENT_CHANNEL_CAPACITY, Inbound};

/// How long to wait before rebuilding the connection after an unexpected
/// transport end, so a refusing server cannot hot-loop the bridge.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// The pairing QR currently outstanding, if any.
#[derive(Debug, Clone, Serialize)]
pub struct QrCode {
    /// Raw `ref,noise_pub,identity_pub,adv_secret,client_type` string.
    pub raw: String,
    /// Inline SVG for the browser.
    pub svg: String,
    /// `https://wa.me/settings/linked_devices#` + raw, openable from a phone.
    pub deep_link: String,
}

impl QrCode {
    /// Render `raw` to an inline SVG and build the wa.me deep link.
    pub fn render(raw: &str) -> Result<QrCode> {
        let qr = fast_qr::QRBuilder::new(raw.as_bytes().to_vec())
            .build()
            .map_err(anyhow::Error::from)?;
        let svg = fast_qr::convert::svg::SvgBuilder::default().to_str(&qr);
        Ok(QrCode {
            raw: raw.to_string(),
            svg,
            deep_link: format!(
                "{}{raw}",
                whatsapp_rust::pair::NATIVE_CAMERA_DEEP_LINK_PREFIX
            ),
        })
    }
}

/// The pairing phase, surfaced to the UI through [`Snapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// No connection attempt yet, or the transport dropped without a pair.
    #[default]
    Idle,
    /// A QR code is outstanding, waiting to be scanned.
    Pairing,
    /// Linked and connected.
    Connected,
    /// The server ended the session; re-pairing is required.
    LoggedOut,
    /// Pairing or connecting failed; `Snapshot.error` says why.
    Error,
}

/// Process-wide pairing state, serialized verbatim for `whatsapp_status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub phase: Phase,
    /// Linked account jid, once pairing succeeds.
    pub jid: Option<String>,
    pub push_name: Option<String>,
    /// The current QR code, while one is outstanding.
    pub qr: Option<QrCode>,
    /// Unix seconds when the outstanding QR code stops being valid.
    pub qr_expires_at: Option<i64>,
    /// True once the server used up all six refs of the current connection;
    /// a fresh client is being built.
    pub refs_exhausted: bool,
    /// Human-readable reason for `Phase::Error`.
    pub error: Option<String>,
}

/// What applying a [`ConnEvent`] asks the connection owner to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// All six QR refs are used up; rebuild the client against the same
    /// `session.db` and connect the new instance.
    Reconnect,
}

/// The pairing state machine. Consumes [`ConnEvent`]s, maintains the
/// [`Snapshot`], and notifies subscribers (the UI) on every change. Pure with
/// respect to the network: tests drive it with [`ConnEvent`]s through
/// [`Pairing::apply_at`].
pub struct Pairing {
    snapshot: Snapshot,
    notify: watch::Sender<Snapshot>,
}

impl Default for Pairing {
    fn default() -> Self {
        Self::new()
    }
}

impl Pairing {
    pub fn new() -> Self {
        let (notify, _) = watch::channel(Snapshot::default());
        Pairing {
            snapshot: Snapshot::default(),
            notify,
        }
    }

    /// Subscribe to state changes. The receiver starts holding the current
    /// snapshot, so a late subscriber never misses the state.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.notify.subscribe()
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Apply an event at the current time; see [`Pairing::apply_at`].
    pub fn apply(&mut self, event: &ConnEvent) -> Option<Effect> {
        self.apply_at(event, Utc::now())
    }

    /// Advance the state machine. Returns an [`Effect`] when the connection
    /// owner must act (rebuild the client after ref exhaustion).
    pub fn apply_at(&mut self, event: &ConnEvent, now: DateTime<Utc>) -> Option<Effect> {
        let effect = self.transition(event, now);
        // A send error only means no subscriber is listening; the state has
        // still advanced.
        let _ = self.notify.send(self.snapshot.clone());
        effect
    }

    fn transition(&mut self, event: &ConnEvent, now: DateTime<Utc>) -> Option<Effect> {
        match event {
            ConnEvent::PairingQrCode { code, timeout } => {
                match QrCode::render(code) {
                    Ok(qr) => {
                        let expires = now
                            + chrono::Duration::from_std(*timeout)
                                .unwrap_or(chrono::Duration::seconds(20));
                        self.snapshot.phase = Phase::Pairing;
                        self.snapshot.qr = Some(qr);
                        self.snapshot.qr_expires_at = Some(expires.timestamp());
                        self.snapshot.refs_exhausted = false;
                        self.snapshot.error = None;
                    }
                    Err(error) => {
                        self.snapshot.phase = Phase::Error;
                        self.snapshot.error = Some(format!("rendering QR code: {error:#}"));
                    }
                }
                None
            }
            ConnEvent::PairingQrCodesExhausted { .. } => {
                self.snapshot.qr = None;
                self.snapshot.qr_expires_at = None;
                self.snapshot.refs_exhausted = true;
                Some(Effect::Reconnect)
            }
            ConnEvent::PairSuccess { id } => {
                self.snapshot.phase = Phase::Connected;
                self.snapshot.jid = Some(id.clone());
                self.snapshot.qr = None;
                self.snapshot.qr_expires_at = None;
                self.snapshot.refs_exhausted = false;
                self.snapshot.error = None;
                None
            }
            ConnEvent::PairError { detail } => {
                self.snapshot.phase = Phase::Error;
                self.snapshot.error = Some(detail.clone());
                self.snapshot.qr = None;
                self.snapshot.qr_expires_at = None;
                None
            }
            ConnEvent::LoggedOut { .. } => {
                self.snapshot.phase = Phase::LoggedOut;
                self.snapshot.jid = None;
                self.snapshot.push_name = None;
                self.snapshot.qr = None;
                self.snapshot.qr_expires_at = None;
                None
            }
            ConnEvent::Connected => {
                // A restored session connects without a fresh PairSuccess; a
                // successful reconnect also clears a stale error. LoggedOut is
                // server-authoritative and is not overridden by transport up.
                if self.snapshot.phase != Phase::LoggedOut {
                    self.snapshot.phase = Phase::Connected;
                    self.snapshot.qr = None;
                    self.snapshot.qr_expires_at = None;
                    self.snapshot.refs_exhausted = false;
                    self.snapshot.error = None;
                }
                None
            }
            ConnEvent::Disconnected { .. } => {
                if self.snapshot.phase == Phase::Connected {
                    self.snapshot.phase = Phase::Idle;
                }
                None
            }
            ConnEvent::ConnectFailure { reason, message } => {
                self.snapshot.phase = Phase::Error;
                self.snapshot.error = Some(match message {
                    Some(message) => format!("{reason}: {message}"),
                    None => reason.clone(),
                });
                None
            }
            ConnEvent::TemporaryBan { detail, expire } => {
                self.snapshot.phase = Phase::Error;
                self.snapshot.error =
                    Some(format!("temporary ban: {detail} (expires in {expire:?})"));
                None
            }
            ConnEvent::StreamReplaced => {
                // Another device took the stream; the client reconnects.
                if self.snapshot.phase == Phase::Connected {
                    self.snapshot.phase = Phase::Idle;
                }
                None
            }
            ConnEvent::ClientOutdated => {
                self.snapshot.phase = Phase::Error;
                self.snapshot.error = Some(
                    "the WhatsApp client version is outdated; update whatsapp-rust".to_string(),
                );
                None
            }
        }
    }
}

/// Owns the live connection and drives the [`Pairing`] state machine with the
/// events it produces. Rebuilds the client against the same `session.db` when
/// the server runs out of QR refs, and after transport ends that the client
/// will not recover from on its own. Bot construction and the network live
/// behind [`Supervisor::start`]; unit tests drive [`Pairing`] directly.
pub struct Supervisor {
    root: PathBuf,
    pairing: Pairing,
    inbound_tx: mpsc::Sender<Inbound>,
}

impl Supervisor {
    pub fn new(root: PathBuf, inbound_tx: mpsc::Sender<Inbound>) -> Self {
        Supervisor {
            root,
            pairing: Pairing::new(),
            inbound_tx,
        }
    }

    pub fn pairing(&self) -> &Pairing {
        &self.pairing
    }

    /// Connect (or reconnect) and pump events into the state machine until a
    /// persistent error stops the loop. Returns `Ok(())` when the state is
    /// `Error`; the snapshot tells the UI why.
    pub async fn start(mut self) -> Result<()> {
        let session_db = self.root.join("session.db");
        loop {
            let (conn_tx, mut conn_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
            let connection = client::connect(&session_db, conn_tx, self.inbound_tx.clone()).await?;

            let mut reconnect = false;
            while let Some(event) = conn_rx.recv().await {
                if self.pairing.apply(&event) == Some(Effect::Reconnect) {
                    reconnect = true;
                    break;
                }
            }
            drop(conn_rx);
            connection.abort();

            if self.pairing.snapshot().phase == Phase::Error {
                // Persistent errors (ban, outdated client, pair failure) need
                // a human; do not respawn against a refusing server.
                return Ok(());
            }
            if !reconnect {
                tokio::time::sleep(RECONNECT_BACKOFF).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: DateTime<Utc> = DateTime::UNIX_EPOCH;

    fn qr_event(code: &str, timeout_secs: u64) -> ConnEvent {
        ConnEvent::PairingQrCode {
            code: code.to_string(),
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    fn apply(pairing: &mut Pairing, event: &ConnEvent) -> Option<Effect> {
        pairing.apply_at(event, T0)
    }

    #[test]
    fn disconnected_then_qr_then_paired() {
        let mut pairing = Pairing::new();
        assert_eq!(pairing.snapshot().phase, Phase::Idle);

        // A transport drop before pairing changes nothing.
        assert_eq!(
            apply(
                &mut pairing,
                &ConnEvent::Disconnected {
                    reason: "socket closed".to_string()
                }
            ),
            None
        );
        assert_eq!(pairing.snapshot().phase, Phase::Idle);

        // First ref: 60s.
        assert_eq!(apply(&mut pairing, &qr_event("ref1", 60)), None);
        let snapshot = pairing.snapshot();
        assert_eq!(snapshot.phase, Phase::Pairing);
        assert!(snapshot.qr.is_some());
        assert_eq!(snapshot.qr_expires_at, Some(60));

        // Pairing succeeds.
        assert_eq!(
            apply(
                &mut pairing,
                &ConnEvent::PairSuccess {
                    id: "5511987654321@s.whatsapp.net".to_string()
                }
            ),
            None
        );
        let snapshot = pairing.snapshot();
        assert_eq!(snapshot.phase, Phase::Connected);
        assert_eq!(
            snapshot.jid.as_deref(),
            Some("5511987654321@s.whatsapp.net")
        );
        assert!(snapshot.qr.is_none());
    }

    #[test]
    fn qr_rotation_tracks_each_ref_timeout() {
        let mut pairing = Pairing::new();
        apply(&mut pairing, &qr_event("ref1", 60));
        assert_eq!(pairing.snapshot().qr_expires_at, Some(60));
        assert_eq!(pairing.snapshot().qr.as_ref().unwrap().raw, "ref1");

        // Later refs last 20s each.
        apply(&mut pairing, &qr_event("ref2", 20));
        assert_eq!(pairing.snapshot().qr_expires_at, Some(20));
        assert_eq!(pairing.snapshot().qr.as_ref().unwrap().raw, "ref2");
    }

    #[test]
    fn exhausted_refs_request_reconnect() {
        let mut pairing = Pairing::new();
        apply(&mut pairing, &qr_event("ref1", 60));

        let effect = apply(
            &mut pairing,
            &ConnEvent::PairingQrCodesExhausted { disconnected: true },
        );
        assert_eq!(effect, Some(Effect::Reconnect));
        let snapshot = pairing.snapshot();
        assert!(snapshot.qr.is_none());
        assert!(snapshot.refs_exhausted);
        assert_eq!(snapshot.qr_expires_at, None);
        // Still pairing: the fresh client will issue new refs.
        assert_eq!(snapshot.phase, Phase::Pairing);
    }

    #[test]
    fn logged_out_unpairs() {
        let mut pairing = Pairing::new();
        apply(
            &mut pairing,
            &ConnEvent::PairSuccess {
                id: "5511987654321@s.whatsapp.net".to_string(),
            },
        );
        assert_eq!(pairing.snapshot().phase, Phase::Connected);

        apply(
            &mut pairing,
            &ConnEvent::LoggedOut {
                on_connect: false,
                reason: "LoggedOut".to_string(),
            },
        );
        let snapshot = pairing.snapshot();
        assert_eq!(snapshot.phase, Phase::LoggedOut);
        assert!(snapshot.jid.is_none());
    }

    #[test]
    fn connected_does_not_override_logged_out() {
        let mut pairing = Pairing::new();
        apply(
            &mut pairing,
            &ConnEvent::LoggedOut {
                on_connect: true,
                reason: "AccountLocked".to_string(),
            },
        );
        apply(&mut pairing, &ConnEvent::Connected);
        assert_eq!(pairing.snapshot().phase, Phase::LoggedOut);
    }

    #[test]
    fn restored_session_connects_without_pair_success() {
        let mut pairing = Pairing::new();
        apply(&mut pairing, &qr_event("ref1", 60));
        apply(&mut pairing, &ConnEvent::Connected);
        assert_eq!(pairing.snapshot().phase, Phase::Connected);
        assert!(pairing.snapshot().qr.is_none());
    }

    #[test]
    fn temporary_ban_enters_error_state() {
        let mut pairing = Pairing::new();
        apply(
            &mut pairing,
            &ConnEvent::PairingQrCodesExhausted { disconnected: true },
        );
        apply(
            &mut pairing,
            &ConnEvent::TemporaryBan {
                detail: "sent to too many people".to_string(),
                expire: Duration::from_secs(3600),
            },
        );
        let snapshot = pairing.snapshot();
        assert_eq!(snapshot.phase, Phase::Error);
        let error = snapshot.error.as_deref().unwrap();
        assert!(error.contains("temporary ban"));
        assert!(error.contains("sent to too many people"));
    }

    #[test]
    fn connect_failure_and_client_outdated_enter_error_state() {
        let mut pairing = Pairing::new();
        apply(
            &mut pairing,
            &ConnEvent::ConnectFailure {
                reason: "ServiceUnavailable".to_string(),
                message: Some("try later".to_string()),
            },
        );
        assert!(
            pairing
                .snapshot()
                .error
                .as_deref()
                .unwrap()
                .contains("try later")
        );

        let mut pairing = Pairing::new();
        apply(&mut pairing, &ConnEvent::ClientOutdated);
        assert_eq!(pairing.snapshot().phase, Phase::Error);
        assert!(
            pairing
                .snapshot()
                .error
                .as_deref()
                .unwrap()
                .contains("outdated")
        );
    }

    #[test]
    fn pair_error_and_stream_replaced() {
        let mut pairing = Pairing::new();
        apply(&mut pairing, &qr_event("ref1", 60));
        apply(
            &mut pairing,
            &ConnEvent::PairError {
                detail: "temporarily_unavailable".to_string(),
            },
        );
        assert_eq!(pairing.snapshot().phase, Phase::Error);
        assert_eq!(
            pairing.snapshot().error.as_deref(),
            Some("temporarily_unavailable")
        );

        let mut pairing = Pairing::new();
        apply(
            &mut pairing,
            &ConnEvent::PairSuccess {
                id: "5511@s.whatsapp.net".to_string(),
            },
        );
        apply(&mut pairing, &ConnEvent::StreamReplaced);
        assert_eq!(pairing.snapshot().phase, Phase::Idle);
    }

    #[test]
    fn subscribers_see_every_snapshot() {
        let mut pairing = Pairing::new();
        let mut rx = pairing.subscribe();
        assert_eq!(rx.borrow().phase, Phase::Idle);

        apply(&mut pairing, &qr_event("ref1", 60));
        assert!(
            rx.has_changed().expect("sender still alive"),
            "a subscription must observe the new snapshot"
        );
        assert_eq!(rx.borrow_and_update().phase, Phase::Pairing);
    }

    #[test]
    fn qr_svg_renders_and_deep_links() {
        let raw = "2@.mockref,ANoisePubKeyBase64,AnIdentityPubKeyBase64,AnAdvSecretBase64,web";
        let qr = QrCode::render(raw).expect("QR renders");

        assert_eq!(qr.raw, raw);
        assert!(qr.svg.contains("<svg"), "svg must open a root element");
        assert!(qr.svg.contains("</svg>"), "svg must close the root element");
        assert!(
            qr.svg.len() > raw.len(),
            "the payload must encode to modules"
        );

        let expected_prefix = whatsapp_rust::pair::NATIVE_CAMERA_DEEP_LINK_PREFIX;
        assert_eq!(
            qr.deep_link,
            format!("{expected_prefix}{raw}"),
            "deep link is the native-camera prefix plus the raw code"
        );
        assert!(qr.deep_link.starts_with("https://wa.me/"));
    }

    #[test]
    fn reconnect_after_error_is_a_no_op_until_applied() {
        // Exhaustion while already in Error (e.g. a ban landed between refs)
        // still asks for a rebuild; the supervisor decides whether to respawn.
        let mut pairing = Pairing::new();
        apply(
            &mut pairing,
            &ConnEvent::TemporaryBan {
                detail: "banned".to_string(),
                expire: Duration::from_secs(60),
            },
        );
        let effect = apply(
            &mut pairing,
            &ConnEvent::PairingQrCodesExhausted {
                disconnected: false,
            },
        );
        assert_eq!(effect, Some(Effect::Reconnect));
    }
}
