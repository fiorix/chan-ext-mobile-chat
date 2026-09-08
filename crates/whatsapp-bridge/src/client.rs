//! whatsapp-rust wiring: Bot construction, the [`WaSink`] implementation over
//! `whatsapp_rust::Client`, and conversion of whatsapp-rust events into the
//! bridge's internal inbound types.
//!
//! [`WaSink`] is the seam that keeps routing, allowlist, rotation and chunking
//! logic under unit test: the real implementation wraps the WhatsApp client,
//! tests use a stub that records what would have been sent.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use whatsapp_rust::download::Downloadable;
use whatsapp_rust::prelude::*;
use whatsapp_rust::types::events::{
    ConnectFailure, Disconnected, LoggedOut, PairError, PairSuccess, PairingQrCode,
    PairingQrCodesExhausted, TemporaryBan,
};

/// A WhatsApp chat identifier, bare (`...@s.whatsapp.net`) or group
/// (`...@g.us`). Newtype so jid strings do not leak into the routing layers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChatId(pub String);

/// The kind of content an inbound message carries. Serialized as snake_case
/// because it lands in the log record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundKind {
    Text,
    Image,
    Video,
    Audio,
    Voice,
    Document,
    Sticker,
    Reaction,
    /// Reserved for bodies the bridge logs without extracting content.
    Other,
}

/// The attachment category of a media descriptor. `Voice` is an audio
/// message with the protobuf `ptt` flag set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Voice,
    Document,
    Sticker,
}

/// Handle to an attachment of an inbound message, carried inside
/// [`MediaDesc`] and consumed by [`WaSink::download`]. Retains the protobuf
/// message so the sink can stream the [`Downloadable`] payload on demand
/// without the routing and logging layers touching whatsapp-rust types.
#[derive(Debug, Clone)]
pub struct MediaRef {
    kind: MediaKind,
    message: Arc<wa::Message>,
}

impl MediaRef {
    /// The underlying protobuf payload, when `kind` still matches a media
    /// accessor on the (wrapper-unwrapped) message.
    pub fn downloadable(&self) -> Option<&dyn Downloadable> {
        let base = self.message.get_base_message();
        match self.kind {
            MediaKind::Image => base
                .image_message
                .as_option()
                .map(|m| m as &dyn Downloadable),
            MediaKind::Video => base
                .video_message
                .as_option()
                .map(|m| m as &dyn Downloadable),
            MediaKind::Audio | MediaKind::Voice => base
                .audio_message
                .as_option()
                .map(|m| m as &dyn Downloadable),
            MediaKind::Document => base
                .document_message
                .as_option()
                .map(|m| m as &dyn Downloadable),
            MediaKind::Sticker => base
                .sticker_message
                .as_option()
                .map(|m| m as &dyn Downloadable),
        }
    }

    pub fn kind(&self) -> MediaKind {
        self.kind
    }
}

/// Owned description of an attachment: everything the log writer and the
/// media gate need, plus the sink handle for the actual download.
#[derive(Debug, Clone)]
pub struct MediaDesc {
    pub kind: MediaKind,
    pub mime: Option<String>,
    pub file_name: Option<String>,
    pub caption: Option<String>,
    pub file_length: Option<u64>,
    pub download: MediaRef,
}

/// An inbound WhatsApp message converted to the bridge's own shape at the
/// edge, so the routing and logging layers never touch whatsapp-rust types.
#[derive(Debug, Clone)]
pub struct Inbound {
    /// WhatsApp message id, the dedupe key.
    pub id: String,
    pub chat: ChatId,
    pub sender: String,
    /// LID/PN counterpart of `sender`, when the stanza exposes one.
    pub sender_alt: Option<String>,
    pub push_name: String,
    /// RFC 3339 UTC, from `MessageInfo::timestamp`.
    pub timestamp: String,
    /// The same instant in whole seconds, for cheap log tailing.
    pub timestamp_unix: i64,
    pub from_me: bool,
    pub is_group: bool,
    pub kind: InboundKind,
    /// Body text. Populated by the `conversation` variant, by
    /// `extended_text_message.text`, and by `reaction_message.text` (the
    /// emoji, for `Reaction` kind). Media captions live in
    /// [`MediaDesc::caption`], not here.
    pub text: String,
    pub media: Option<MediaDesc>,
    /// Id of the quoted message, from `context_info.stanza_id`.
    pub quoted_id: Option<String>,
}

/// Connection and pairing events from whatsapp-rust, reduced to what the
/// pairing state machine consumes. Detail strings come from the event's own
/// fields where it carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnEvent {
    /// A fresh QR payload; `timeout` is how long it stays valid.
    PairingQrCode { code: String, timeout: Duration },
    /// All six pairing refs are used up; the client must be rebuilt.
    PairingQrCodesExhausted { disconnected: bool },
    /// Linking succeeded; `id` is the account jid.
    PairSuccess { id: String },
    /// Linking failed; `detail` is the event's error text.
    PairError { detail: String },
    /// The server forced a logout.
    LoggedOut { on_connect: bool, reason: String },
    /// The stream is up (fires on every connect, paired or not).
    Connected,
    /// The transport ended; the client reconnects on its own.
    Disconnected { reason: String },
    /// The server refused the connection.
    ConnectFailure {
        reason: String,
        message: Option<String>,
    },
    /// Temporary ban; `detail` is the server's reason, `expire` the duration.
    TemporaryBan { detail: String, expire: Duration },
    /// Another device took over the stream.
    StreamReplaced,
    /// The client version is no longer accepted.
    ClientOutdated,
}

/// The result of converting one whatsapp-rust `Event` at the edge.
#[derive(Debug)]
pub enum Conversion {
    /// Inbound messages worth keeping; empty when the batch held none.
    Messages(Vec<Inbound>),
    /// A connection or pairing event for the state machine.
    Conn(ConnEvent),
    /// Receipts, presence, history sync, and everything else the bridge
    /// does not act on.
    Ignored,
}

/// Convert one whatsapp-rust event into the internal types. `Event` and its
/// payloads are `#[non_exhaustive]`, so payload matches keep a `..` rest.
pub fn convert_event(event: &Event) -> Conversion {
    match event {
        Event::Messages(batch) => {
            Conversion::Messages(batch.iter().filter_map(inbound_from).collect())
        }
        _ => match conn_event(event) {
            Some(conn) => Conversion::Conn(conn),
            None => Conversion::Ignored,
        },
    }
}

/// Reduce a non-message event to a [`ConnEvent`]; `None` for the events the
/// bridge ignores.
pub fn conn_event(event: &Event) -> Option<ConnEvent> {
    match event {
        Event::PairingQrCode(PairingQrCode { code, timeout, .. }) => {
            Some(ConnEvent::PairingQrCode {
                code: code.clone(),
                timeout: *timeout,
            })
        }
        Event::PairingQrCodesExhausted(PairingQrCodesExhausted { disconnected, .. }) => {
            Some(ConnEvent::PairingQrCodesExhausted {
                disconnected: *disconnected,
            })
        }
        Event::PairSuccess(PairSuccess { id, .. }) => {
            Some(ConnEvent::PairSuccess { id: id.to_string() })
        }
        Event::PairError(PairError { error, .. }) => Some(ConnEvent::PairError {
            detail: error.clone(),
        }),
        Event::LoggedOut(LoggedOut {
            on_connect, reason, ..
        }) => Some(ConnEvent::LoggedOut {
            on_connect: *on_connect,
            reason: format!("{reason:?}"),
        }),
        Event::Connected(_) => Some(ConnEvent::Connected),
        Event::Disconnected(Disconnected { reason, .. }) => Some(ConnEvent::Disconnected {
            reason: reason.to_string(),
        }),
        Event::ConnectFailure(ConnectFailure {
            reason, message, ..
        }) => Some(ConnEvent::ConnectFailure {
            reason: format!("{reason:?}"),
            message: message.clone(),
        }),
        Event::TemporaryBan(TemporaryBan {
            code,
            expire,
            message,
            ..
        }) => Some(ConnEvent::TemporaryBan {
            detail: message.clone().unwrap_or_else(|| code.to_string()),
            expire: expire.to_std().unwrap_or(Duration::ZERO),
        }),
        Event::StreamReplaced(_) => Some(ConnEvent::StreamReplaced),
        Event::ClientOutdated(_) => Some(ConnEvent::ClientOutdated),
        _ => None,
    }
}

/// Convert one decrypted inbound message into the internal type; `None` for
/// empty or unsupported bodies (wrapper-only stanzas with no content).
pub fn inbound_from(inbound: &InboundMessage) -> Option<Inbound> {
    let info = &inbound.info;
    let message = inbound.message.get_base_message();

    let mut kind = InboundKind::Text;
    let mut text = message.conversation.clone().unwrap_or_default();
    if text.is_empty()
        && let Some(ext) = message.extended_text_message.as_option()
    {
        text = ext.text.clone().unwrap_or_default();
    }
    let mut quoted_id = message
        .extended_text_message
        .as_option()
        .and_then(|ext| ext.context_info.as_option())
        .and_then(|ctx| ctx.stanza_id.clone());

    let mut media = None;
    if let Some(image) = message.image_message.as_option() {
        kind = InboundKind::Image;
        if quoted_id.is_none() {
            quoted_id = quoted_from(&image.context_info);
        }
        media = Some(media_desc(
            MediaKind::Image,
            image.mimetype.clone(),
            None,
            image.caption.clone(),
            image.file_length,
            &inbound.message,
        ));
    } else if let Some(video) = message.video_message.as_option() {
        kind = InboundKind::Video;
        if quoted_id.is_none() {
            quoted_id = quoted_from(&video.context_info);
        }
        media = Some(media_desc(
            MediaKind::Video,
            video.mimetype.clone(),
            None,
            video.caption.clone(),
            video.file_length,
            &inbound.message,
        ));
    } else if let Some(audio) = message.audio_message.as_option() {
        let voice = audio.ptt.unwrap_or_default();
        kind = if voice {
            InboundKind::Voice
        } else {
            InboundKind::Audio
        };
        if quoted_id.is_none() {
            quoted_id = quoted_from(&audio.context_info);
        }
        media = Some(media_desc(
            if voice {
                MediaKind::Voice
            } else {
                MediaKind::Audio
            },
            audio.mimetype.clone(),
            None,
            None,
            audio.file_length,
            &inbound.message,
        ));
    } else if let Some(document) = message.document_message.as_option() {
        kind = InboundKind::Document;
        if quoted_id.is_none() {
            quoted_id = quoted_from(&document.context_info);
        }
        media = Some(media_desc(
            MediaKind::Document,
            document.mimetype.clone(),
            document.file_name.clone(),
            document.caption.clone(),
            document.file_length,
            &inbound.message,
        ));
    } else if let Some(sticker) = message.sticker_message.as_option() {
        kind = InboundKind::Sticker;
        media = Some(media_desc(
            MediaKind::Sticker,
            sticker.mimetype.clone(),
            None,
            None,
            sticker.file_length,
            &inbound.message,
        ));
    } else if text.is_empty()
        && let Some(reaction) = message.reaction_message.as_option()
    {
        kind = InboundKind::Reaction;
        text = reaction.text.clone().unwrap_or_default();
    }

    if text.is_empty() && media.is_none() {
        return None;
    }

    Some(Inbound {
        id: info.id.clone(),
        chat: ChatId(info.source.chat.to_string()),
        sender: info.source.sender.to_string(),
        sender_alt: info.source.sender_alt.as_ref().map(ToString::to_string),
        push_name: info.push_name.clone(),
        timestamp: info.timestamp.to_rfc3339(),
        timestamp_unix: info.timestamp.timestamp(),
        from_me: info.source.is_from_me,
        is_group: info.source.is_group,
        kind,
        text,
        media,
        quoted_id,
    })
}

fn quoted_from(ctx: &whatsapp_rust::buffa::MessageField<wa::ContextInfo>) -> Option<String> {
    ctx.as_option().and_then(|ctx| ctx.stanza_id.clone())
}

fn media_desc(
    kind: MediaKind,
    mime: Option<String>,
    file_name: Option<String>,
    caption: Option<String>,
    file_length: Option<u64>,
    message: &Arc<wa::Message>,
) -> MediaDesc {
    MediaDesc {
        kind,
        mime,
        file_name,
        caption,
        file_length,
        download: MediaRef {
            kind,
            message: message.clone(),
        },
    }
}

/// A spawned bot and its client handle. The axum server owns the process
/// lifetime, so the [`BotHandle`] is kept here (dropping it aborts the bot
/// task) rather than awaited.
pub struct Connection {
    handle: BotHandle,
    client: Arc<Client>,
}

impl Connection {
    /// Full client API: sending, downloading, profile queries.
    pub fn client(&self) -> Arc<Client> {
        self.client.clone()
    }

    /// Stop this connection's bot so a fresh client can be built against the
    /// same `session.db` (the only resume path after QR ref exhaustion, since
    /// `Client::disconnect()` is final for an instance).
    pub fn abort(&self) {
        self.handle.abort();
    }
}

/// Capacity of the event channels between the bot's event handler and the
/// bridge's consumers.
pub const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Build the bot over `session_db` and spawn it. `skip_history_sync` keeps
/// startup cheap; QR/pairing events flow to `conn_tx`, inbound messages to
/// `inbound_tx`. The returned [`Connection`] must stay alive for the bot to
/// keep running.
pub async fn connect(
    session_db: &Path,
    conn_tx: mpsc::Sender<ConnEvent>,
    inbound_tx: mpsc::Sender<Inbound>,
) -> Result<Connection> {
    let url = session_db
        .to_str()
        .with_context(|| format!("session.db path is not UTF-8: {}", session_db.display()))?;
    let store = SqliteStore::new(url).await?;
    let bot = Bot::builder()
        .with_backend(store)
        .skip_history_sync()
        .on_event(move |event, _client| {
            let conn_tx = conn_tx.clone();
            let inbound_tx = inbound_tx.clone();
            async move {
                match convert_event(&event) {
                    Conversion::Messages(messages) => {
                        for message in messages {
                            if inbound_tx.send(message).await.is_err() {
                                break;
                            }
                        }
                    }
                    Conversion::Conn(conn) => {
                        let _ = conn_tx.send(conn).await;
                    }
                    Conversion::Ignored => {}
                }
            }
        })
        .build()
        .await?;
    let handle = bot.spawn();
    let client = handle.client();
    Ok(Connection { handle, client })
}

/// The live [`WaSink`] over `whatsapp_rust::Client`.
pub struct WaClient {
    client: Arc<Client>,
}

impl WaClient {
    pub fn new(client: Arc<Client>) -> Self {
        Self { client }
    }
}

impl WaSink for WaClient {
    async fn send_text(&self, chat: &ChatId, text: &str) -> Result<String> {
        let to: Jid = chat
            .0
            .parse()
            .with_context(|| format!("invalid chat jid: {}", chat.0))?;
        let message = wa::Message {
            conversation: Some(text.to_owned()),
            ..Default::default()
        };
        let sent = self.client.send_message(to, message).await?;
        Ok(sent.message_id)
    }

    async fn download(&self, media: &MediaRef, to: &Path) -> Result<u64> {
        let downloadable = media
            .downloadable()
            .with_context(|| format!("message has no {:?} media to download", media.kind()))?;
        let file =
            std::fs::File::create(to).with_context(|| format!("creating {}", to.display()))?;
        let file = self.client.download_to_writer(downloadable, file).await?;
        let bytes = file
            .metadata()
            .with_context(|| format!("statting {}", to.display()))?
            .len();
        if bytes == 0 {
            bail!("download of {:?} media wrote no bytes", media.kind());
        }
        Ok(bytes)
    }
}

/// Sending and downloading through a small async trait, so tests substitute a
/// recording stub for the live WhatsApp client.
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use whatsapp_rust::buffa;
    use whatsapp_rust::types::events::{
        ClientOutdated, ConnectFailureReason, Connected, OfflineSyncCompleted, StreamReplaced,
        TempBanReason,
    };
    use whatsapp_rust::types::message::MessageSource;

    fn jid(s: &str) -> Jid {
        s.parse().unwrap()
    }

    fn message_info() -> MessageInfo {
        MessageInfo {
            source: MessageSource {
                chat: jid("12036300001@g.us"),
                sender: jid("5511987654321@s.whatsapp.net"),
                is_from_me: false,
                is_group: true,
                sender_alt: Some(jid("12345678901234@lid")),
                ..Default::default()
            },
            id: "3EB0FAKE".to_string(),
            push_name: "Alex".to_string(),
            timestamp: Utc.with_ymd_and_hms(2026, 9, 7, 12, 34, 56).unwrap(),
            ..Default::default()
        }
    }

    fn as_inbound(message: wa::Message) -> InboundMessage {
        InboundMessage::builder()
            .message(Arc::new(message))
            .info(Arc::new(message_info()))
            .build()
    }

    #[test]
    fn conn_event_converts_pairing_payloads() {
        let qr = Event::PairingQrCode(
            PairingQrCode::builder()
                .code("ref,noise,identity,adv,web".to_string())
                .timeout(Duration::from_secs(60))
                .build(),
        );
        let Conversion::Conn(ConnEvent::PairingQrCode { code, timeout }) = convert_event(&qr)
        else {
            panic!("expected a pairing QR event, got {:?}", convert_event(&qr));
        };
        assert_eq!(code, "ref,noise,identity,adv,web");
        assert_eq!(timeout, Duration::from_secs(60));

        let exhausted = Event::PairingQrCodesExhausted(
            PairingQrCodesExhausted::builder()
                .disconnected(true)
                .build(),
        );
        let Conversion::Conn(ConnEvent::PairingQrCodesExhausted { disconnected }) =
            convert_event(&exhausted)
        else {
            panic!("expected exhausted event");
        };
        assert!(disconnected);

        let connected = Event::Connected(Connected::builder().build());
        assert!(matches!(
            convert_event(&connected),
            Conversion::Conn(ConnEvent::Connected)
        ));

        let replaced = Event::StreamReplaced(StreamReplaced::builder().build());
        assert!(matches!(
            convert_event(&replaced),
            Conversion::Conn(ConnEvent::StreamReplaced)
        ));
    }

    #[test]
    fn conn_event_converts_failures_with_details() {
        let ban = Event::TemporaryBan(
            TemporaryBan::builder()
                .code(TempBanReason::SentToTooManyPeople)
                .expire(chrono::Duration::seconds(3600))
                .message("slow down".to_string())
                .build(),
        );
        let Conversion::Conn(ConnEvent::TemporaryBan { detail, expire }) = convert_event(&ban)
        else {
            panic!("expected a temporary ban event");
        };
        assert_eq!(detail, "slow down");
        assert_eq!(expire, Duration::from_secs(3600));

        let failure = Event::ConnectFailure(
            ConnectFailure::builder()
                .reason(ConnectFailureReason::ServiceUnavailable)
                .message("try later".to_string())
                .build(),
        );
        let Conversion::Conn(ConnEvent::ConnectFailure { reason, message }) =
            convert_event(&failure)
        else {
            panic!("expected a connect failure event");
        };
        assert_eq!(
            reason,
            format!("{:?}", ConnectFailureReason::ServiceUnavailable)
        );
        assert_eq!(message.as_deref(), Some("try later"));

        let logged_out = Event::LoggedOut(
            LoggedOut::builder()
                .on_connect(true)
                .reason(ConnectFailureReason::LoggedOut)
                .build(),
        );
        let Conversion::Conn(ConnEvent::LoggedOut { on_connect, .. }) = convert_event(&logged_out)
        else {
            panic!("expected a logged out event");
        };
        assert!(on_connect);

        let outdated = Event::ClientOutdated(ClientOutdated::builder().build());
        assert!(matches!(
            convert_event(&outdated),
            Conversion::Conn(ConnEvent::ClientOutdated)
        ));
    }

    #[test]
    fn ignores_events_the_bridge_does_not_act_on() {
        let receipt = Event::OfflineSyncCompleted(OfflineSyncCompleted::builder().count(0).build());
        assert!(matches!(convert_event(&receipt), Conversion::Ignored));
    }

    #[test]
    fn inbound_text_extracts_envelope_and_body() {
        let message = wa::Message {
            conversation: Some("hello there".to_string()),
            ..Default::default()
        };
        let inbound = inbound_from(&as_inbound(message)).expect("a text message converts");

        assert_eq!(inbound.id, "3EB0FAKE");
        assert_eq!(inbound.chat, ChatId("12036300001@g.us".to_string()));
        assert_eq!(inbound.sender, "5511987654321@s.whatsapp.net");
        assert_eq!(inbound.sender_alt.as_deref(), Some("12345678901234@lid"));
        assert_eq!(inbound.push_name, "Alex");
        assert_eq!(inbound.timestamp, "2026-09-07T12:34:56+00:00");
        assert_eq!(inbound.timestamp_unix, 1_788_784_496);
        assert!(!inbound.from_me);
        assert!(inbound.is_group);
        assert_eq!(inbound.kind, InboundKind::Text);
        assert_eq!(inbound.text, "hello there");
        assert!(inbound.media.is_none());
        assert!(inbound.quoted_id.is_none());
    }

    #[test]
    fn inbound_extended_text_and_quote_extract() {
        let message = wa::Message {
            extended_text_message: buffa::MessageField::some(wa::message::ExtendedTextMessage {
                text: Some("see this".to_string()),
                context_info: buffa::MessageField::some(wa::ContextInfo {
                    stanza_id: Some("QUOTED1".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let inbound = inbound_from(&as_inbound(message)).expect("extended text converts");
        assert_eq!(inbound.kind, InboundKind::Text);
        assert_eq!(inbound.text, "see this");
        assert_eq!(inbound.quoted_id.as_deref(), Some("QUOTED1"));
    }

    #[test]
    fn inbound_image_extracts_media_descriptor() {
        let message = wa::Message {
            image_message: buffa::MessageField::some(wa::message::ImageMessage {
                mimetype: Some("image/jpeg".to_string()),
                caption: Some("a cat".to_string()),
                file_length: Some(123_456),
                ..Default::default()
            }),
            ..Default::default()
        };
        let inbound = inbound_from(&as_inbound(message)).expect("image converts");

        assert_eq!(inbound.kind, InboundKind::Image);
        assert!(
            inbound.text.is_empty(),
            "captions live in the media descriptor"
        );
        let media = inbound.media.expect("media descriptor present");
        assert_eq!(media.kind, MediaKind::Image);
        assert_eq!(media.mime.as_deref(), Some("image/jpeg"));
        assert_eq!(media.caption.as_deref(), Some("a cat"));
        assert_eq!(media.file_length, Some(123_456));
        assert!(media.download.downloadable().is_some());
    }

    #[test]
    fn inbound_audio_ptt_distinguishes_voice() {
        let voice = wa::Message {
            audio_message: buffa::MessageField::some(wa::message::AudioMessage {
                ptt: Some(true),
                mimetype: Some("audio/ogg; codecs=opus".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let inbound = inbound_from(&as_inbound(voice)).expect("voice note converts");
        assert_eq!(inbound.kind, InboundKind::Voice);
        assert_eq!(inbound.media.as_ref().unwrap().kind, MediaKind::Voice);

        let audio = wa::Message {
            audio_message: buffa::MessageField::some(wa::message::AudioMessage {
                mimetype: Some("audio/mpeg".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let inbound = inbound_from(&as_inbound(audio)).expect("audio converts");
        assert_eq!(inbound.kind, InboundKind::Audio);
    }

    #[test]
    fn inbound_document_extracts_file_name() {
        let message = wa::Message {
            document_message: buffa::MessageField::some(wa::message::DocumentMessage {
                mimetype: Some("application/pdf".to_string()),
                file_name: Some("report.pdf".to_string()),
                file_length: Some(9),
                ..Default::default()
            }),
            ..Default::default()
        };
        let inbound = inbound_from(&as_inbound(message)).expect("document converts");
        assert_eq!(inbound.kind, InboundKind::Document);
        let media = inbound.media.expect("media descriptor present");
        assert_eq!(media.file_name.as_deref(), Some("report.pdf"));
        assert!(media.download.downloadable().is_some());
    }

    #[test]
    fn inbound_reaction_extracts_emoji() {
        let message = wa::Message {
            reaction_message: buffa::MessageField::some(wa::message::ReactionMessage {
                text: Some("\u{1F44D}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let inbound = inbound_from(&as_inbound(message)).expect("reaction converts");
        assert_eq!(inbound.kind, InboundKind::Reaction);
        assert_eq!(inbound.text, "\u{1F44D}");
    }

    #[test]
    fn inbound_skips_empty_and_wrapper_only_messages() {
        let empty = inbound_from(&as_inbound(wa::Message::default()));
        assert!(empty.is_none(), "an empty body must not convert");

        let wrapper = wa::Message {
            ephemeral_message: buffa::MessageField::some(wa::message::FutureProofMessage {
                message: buffa::MessageField::some(wa::Message {
                    conversation: Some("inside".to_string()),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        };
        let inbound = inbound_from(&as_inbound(wrapper)).expect("wrapper must unwrap");
        assert_eq!(inbound.text, "inside");
    }
}
