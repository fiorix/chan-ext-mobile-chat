//! The WhatsApp bridge integration: the [`Host`] implementation over
//! [`AppState`], process-wide startup wiring, the status payload pushed to
//! control sockets, and the control-socket actions (`whatsapp_*`).
//!
//! The bridge is constructed once next to [`AppState`], not per tenant, but
//! bindings name an `(owner, conversation)` pair, so a chat bound in one
//! workspace does not resolve from another. WhatsApp state is process-wide and
//! reaches every control socket through a dedicated watch channel, per the
//! design's control protocol section.
//!
//! Bridge API gaps worked around here (to fix in the bridge crate later):
//!
//! - `Supervisor::start` hides the connections it builds (and rebuilds on
//!   every reconnect), so the embedding binary cannot construct the bridge's
//!   `WaClient` sink. [`drive_connection`] mirrors `Supervisor::start` and
//!   publishes each new client into a watch that [`SharedSink`] reads.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use whatsapp_bridge::client::{self, EVENT_CHANNEL_CAPACITY, Inbound, MediaRef, WaSink};
use whatsapp_bridge::command::{AutoReplyGate, REPLY_NO_AGENT};
use whatsapp_bridge::pairing::{Effect, Pairing, Phase as PairPhase, Snapshot};
use whatsapp_bridge::route_in::Router;
use whatsapp_bridge::route_out::ForwarderSet;
use whatsapp_bridge::settings::SettingsStore;
use whatsapp_bridge::store::{ChatMeta, ChatStore};
use whatsapp_bridge::{
    Bridge, BridgeConfig, ConversationView, EntryView, Host, QuestionStatus, QuestionView,
    Resolved, SendRequest, State,
};
use whatsapp_rust::prelude::*;

use crate::config;
use crate::model::{Conversation, Delivery, Entry, Phase, QuestionStatus as ModelQuestionStatus};
use crate::{AppState, Tenant};

/// How long to wait before reconnecting after a transport end, so a refusing
/// server cannot hot-loop the bridge. Mirrors the bridge's reconnect backoff.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// How often the forwarder set reconciles its running tasks against
/// `settings.json`, picking up new bindings within one interval.
const FORWARDER_SYNC_INTERVAL: Duration = Duration::from_secs(5);

impl Host for AppState {
    fn resolve<'a>(
        &'a self,
        owner: &'a str,
        conversation: &'a str,
    ) -> Pin<Box<dyn Future<Output = Resolved> + Send + 'a>> {
        Box::pin(async move {
            let (tenant, chat) = {
                let tenants = self.tenants.lock().expect("tenants lock");
                let Some(tenant) = tenants.get(owner) else {
                    return Resolved::NotLoaded;
                };
                let Ok(chat) = tenant.get(conversation) else {
                    return Resolved::Missing;
                };
                (Arc::clone(tenant), chat)
            };
            let data = chat.data();
            if !tenant.runtime.active.load(Ordering::Acquire)
                || data.run.phase.ended()
                || data.run.phase == Phase::Stopping
            {
                return Resolved::Stopped;
            }
            Resolved::Found(ConversationView {
                title: data.title.clone(),
                phase: phase_label(&data.run.phase),
                queue_depth: data
                    .entries
                    .iter()
                    .filter(|entry| entry.role == "user" && entry.delivery != Some(Delivery::Read))
                    .count(),
                pending_questions: data.pending_questions(),
                entries: data
                    .entries
                    .iter()
                    .map(|entry| entry_view(&data, entry))
                    .collect(),
            })
        })
    }

    fn send<'a>(
        &'a self,
        owner: &'a str,
        conversation: &'a str,
        send: SendRequest,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let chat = {
                let tenants = self.tenants.lock().expect("tenants lock");
                // Never create a tenant from WhatsApp: a binding whose tenant
                // was never loaded is the same honest "no agent running" the
                // bridge auto-replies for a stopped agent.
                let tenant = tenants.get(owner).with_context(|| REPLY_NO_AGENT)?;
                tenant.get(conversation)?
            };
            // Chat::send enforces the (id, fingerprint) idempotency receipts,
            // so WhatsApp retries dedupe exactly like control-socket retries.
            chat.send(
                send.id,
                send.fingerprint,
                send.body,
                send.question,
                send.cancel,
            )
            .await?;
            Ok(())
        })
    }

    fn subscribe<'a>(
        &'a self,
        owner: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<broadcast::Receiver<()>>> + Send + 'a>> {
        Box::pin(async move {
            self.tenants
                .lock()
                .expect("tenants lock")
                .get(owner)
                .map(|tenant| tenant.updates.subscribe())
        })
    }
}

/// Human-readable run phase for status replies, the words the UI shows:
/// `Booting`, `Running`, `Waiting`, ...
fn phase_label(phase: &Phase) -> String {
    match phase {
        Phase::Starting | Phase::Connecting => "Booting",
        Phase::Ready | Phase::Working => "Running",
        Phase::Waiting => "Waiting",
        Phase::Stopping => "Stopping",
        Phase::Stopped => "Stopped",
        Phase::Failed => "Failed",
    }
    .to_string()
}

fn entry_view(data: &Conversation, entry: &Entry) -> EntryView {
    EntryView {
        id: entry.id.clone(),
        kind: entry.kind.clone(),
        body: entry.body.clone(),
        role: entry.role.clone(),
        agent_name: data.agent.name.clone(),
        question: entry.question.as_ref().map(|question| QuestionView {
            options: question.options.clone(),
            status: question_status(&question.status),
        }),
    }
}

fn question_status(status: &ModelQuestionStatus) -> QuestionStatus {
    match status {
        ModelQuestionStatus::Pending => QuestionStatus::Pending,
        ModelQuestionStatus::Answered => QuestionStatus::Answered,
        // A stopped run deactivates its pending questions; the bridge has no
        // Inactive, and Cancelled is the honest "will never be answered".
        ModelQuestionStatus::Cancelled | ModelQuestionStatus::Inactive => QuestionStatus::Cancelled,
    }
}

/// The `whatsapp_status` payload, keyed exactly as the design's response
/// table: `{state, jid, push_name, qr_svg, qr_raw, qr_deep_link,
/// qr_expires_at, error}`. `state` is one of `disabled`, `locked`,
/// `unpaired`, `pairing`, `connected`, `error`.
fn status_payload(state: &State, snapshot: Option<&Snapshot>) -> Value {
    let mut error = None;
    let phase_state = match snapshot {
        Some(snapshot) => {
            error = snapshot.error.clone();
            match snapshot.phase {
                PairPhase::Pairing => "pairing",
                PairPhase::Connected => "connected",
                PairPhase::Error => "error",
                PairPhase::Idle | PairPhase::LoggedOut => "unpaired",
            }
        }
        None => "unpaired",
    };
    let state = match state {
        State::Disabled => "disabled",
        State::Locked => "locked",
        State::Error { detail } => {
            error = Some(detail.clone());
            "error"
        }
        _ => phase_state,
    };
    let qr = snapshot.and_then(|snapshot| snapshot.qr.clone());
    json!({
        "state": state,
        "jid": snapshot.and_then(|snapshot| snapshot.jid.clone()),
        "push_name": snapshot.and_then(|snapshot| snapshot.push_name.clone()),
        "qr_svg": qr.as_ref().map(|qr| qr.svg.clone()),
        "qr_raw": qr.as_ref().map(|qr| qr.raw.clone()),
        "qr_deep_link": qr.as_ref().map(|qr| qr.deep_link.clone()),
        "qr_expires_at": snapshot.and_then(|snapshot| snapshot.qr_expires_at),
        "error": error,
    })
}

/// The live sink handed to the bridge's router and forwarders. The connection
/// driver publishes each new client into the watch as it connects and clears
/// it on teardown, so a send waits out a reconnect instead of racing a
/// half-torn-down client.
#[derive(Clone)]
struct SharedSink {
    client: watch::Receiver<Option<Arc<Client>>>,
}

impl SharedSink {
    async fn client(&self) -> Result<Arc<Client>> {
        let mut watch = self.client.clone();
        loop {
            if let Some(client) = watch.borrow_and_update().clone() {
                return Ok(client);
            }
            watch
                .changed()
                .await
                .context("WhatsApp is not connected.")?;
        }
    }
}

impl WaSink for SharedSink {
    async fn send_text(&self, chat: &client::ChatId, text: &str) -> Result<String> {
        let client = self.client().await?;
        let to: Jid = chat
            .0
            .parse()
            .with_context(|| format!("invalid chat jid: {}", chat.0))?;
        let message = wa::Message {
            conversation: Some(text.to_owned()),
            ..Default::default()
        };
        let sent = client.send_message(to, message).await?;
        Ok(sent.message_id)
    }

    async fn download(&self, media: &MediaRef, to: &Path) -> Result<u64> {
        let client = self.client().await?;
        let downloadable = media
            .downloadable()
            .with_context(|| format!("message has no {:?} media to download", media.kind()))?;
        let file =
            std::fs::File::create(to).with_context(|| format!("creating {}", to.display()))?;
        let file = client.download_to_writer(downloadable, file).await?;
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

/// Connect and pump events into the pairing state machine until a persistent
/// error stops the loop; rebuilds the client against the same `session.db`
/// when the server runs out of QR refs. Mirrors `Supervisor::start`, plus
/// publishing each connection's client for the [`SharedSink`].
async fn drive_connection(
    root: PathBuf,
    mut pairing: Pairing,
    inbound_tx: mpsc::Sender<Inbound>,
    client_tx: watch::Sender<Option<Arc<Client>>>,
) -> Result<()> {
    let session_db = root.join("session.db");
    loop {
        let (conn_tx, mut conn_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let connection = client::connect(&session_db, conn_tx, inbound_tx.clone()).await?;
        let _ = client_tx.send(Some(connection.client()));

        let mut reconnect = false;
        while let Some(event) = conn_rx.recv().await {
            if pairing.apply(&event) == Some(Effect::Reconnect) {
                reconnect = true;
                break;
            }
        }
        drop(conn_rx);
        let _ = client_tx.send(None);
        connection.abort();

        if pairing.snapshot().phase == PairPhase::Error {
            // Persistent errors (ban, outdated client, pair failure) need a
            // human; do not respawn against a refusing server.
            return Ok(());
        }
        if !reconnect {
            tokio::time::sleep(RECONNECT_BACKOFF).await;
        }
    }
}

/// Process-wide WhatsApp state: the bridge (held for its lock), the latest
/// status payload, and the live runtime pieces when the bridge is enabled.
pub(crate) struct Whatsapp {
    status: watch::Sender<Value>,
    bridge: Bridge,
    inner: Option<Inner>,
}

struct Inner {
    root: PathBuf,
    settings: Arc<Mutex<SettingsStore>>,
    inbound_tx: mpsc::Sender<Inbound>,
    client_tx: watch::Sender<Option<Arc<Client>>>,
    slot: Mutex<Slot>,
}

#[derive(Default)]
struct Slot {
    connection: Option<JoinHandle<()>>,
    pump: Option<JoinHandle<()>>,
}

/// Construct the bridge next to `AppState` and spawn its tasks. Returns
/// `Ok(None)` only when the feature is compiled out (the caller cfg-gates this
/// whole module, so this always returns `Some`); a disabled or locked bridge
/// still reports its state but spawns nothing.
pub(crate) async fn start(state: Arc<AppState>) -> Result<Option<Arc<Whatsapp>>> {
    let config = &state.config.whatsapp;
    let root = config
        .root
        .clone()
        .unwrap_or_else(|| config::chan_home().join("mobile-chat/whatsapp"));
    let bridge_config = BridgeConfig {
        log_max_bytes: config.log_max_bytes,
        log_keep: config.log_keep,
        media_max_bytes: config.media_max_bytes,
        media: config.media.clone(),
        reply_progress: config.reply_progress,
    };
    let bridge = Bridge::new(config.enabled, bridge_config.clone(), &root)?;
    let (status_tx, _) = watch::channel(Value::Null);
    let inner = match bridge.state() {
        State::Disabled | State::Locked => None,
        _ => {
            let settings = Arc::new(Mutex::new(SettingsStore::new(root.join("settings.json"))?));
            let chats = Arc::new(Mutex::new(ChatStore::new(
                &root,
                config.log_max_bytes,
                config.log_keep,
            )?));
            let (inbound_tx, mut inbound_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
            let (client_tx, client_rx) = watch::channel(None);
            let host: Arc<dyn Host> = state.clone();
            let sink = Arc::new(SharedSink { client: client_rx });
            let router = Arc::new(Router::new(
                settings.clone(),
                chats.clone(),
                bridge_config.clone(),
                sink.clone(),
                AutoReplyGate::new(),
                host.clone(),
            ));
            tokio::spawn(async move {
                while let Some(inbound) = inbound_rx.recv().await {
                    if let Err(error) = router.handle(&inbound).await {
                        eprintln!("mobile-chat: whatsapp routing: {error:#}");
                    }
                }
            });
            let forwarders = ForwarderSet::new(
                settings.clone(),
                chats.clone(),
                sink,
                host,
                config.reply_progress,
            );
            tokio::spawn(async move {
                forwarders.supervise(FORWARDER_SYNC_INTERVAL).await;
            });
            Some(Inner {
                root,
                settings,
                inbound_tx,
                client_tx,
                slot: Mutex::new(Slot::default()),
            })
        }
    };
    let whatsapp = Arc::new(Whatsapp {
        status: status_tx,
        bridge,
        inner,
    });
    match &whatsapp.inner {
        Some(inner) => whatsapp.ensure_pairing(inner).await?,
        None => {
            let _ = whatsapp
                .status
                .send(status_payload(whatsapp.bridge.state(), None));
        }
    }
    Ok(Some(whatsapp))
}

impl Whatsapp {
    fn inner(&self) -> Result<&Inner> {
        self.inner.as_ref().context("WhatsApp is disabled.")
    }

    /// The current status payload, for `whatsapp_status` and control-socket
    /// pushes.
    pub(crate) fn status(&self) -> Value {
        self.status.borrow().clone()
    }

    /// Subscribe to status changes; the receiver starts holding the current
    /// payload, so a late subscriber never misses the state.
    pub(crate) fn subscribe(&self) -> watch::Receiver<Value> {
        self.status.subscribe()
    }

    /// Start or restart pairing: spawn the connection driver and the status
    /// pump unless a live connection already runs. After `unpair`, or once a
    /// persistent error ended the driver, the slot is finished and this
    /// respawns it.
    async fn ensure_pairing(&self, inner: &Inner) -> Result<()> {
        let mut slot = inner.slot.lock().await;
        if slot
            .connection
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return Ok(());
        }
        if let Some(task) = slot.connection.take() {
            task.abort();
        }
        if let Some(pump) = slot.pump.take() {
            pump.abort();
        }
        let pairing = Pairing::new();
        let mut snapshots = pairing.subscribe();
        let status = self.status.clone();
        slot.pump = Some(tokio::spawn(async move {
            loop {
                let _ = status.send(status_payload(
                    &State::Unpaired,
                    Some(&snapshots.borrow_and_update()),
                ));
                if snapshots.changed().await.is_err() {
                    break;
                }
            }
        }));
        let connection = tokio::spawn({
            let root = inner.root.clone();
            let inbound_tx = inner.inbound_tx.clone();
            let client_tx = inner.client_tx.clone();
            async move {
                if let Err(error) = drive_connection(root, pairing, inbound_tx, client_tx).await {
                    eprintln!("mobile-chat: whatsapp connection: {error:#}");
                }
            }
        });
        slot.connection = Some(connection);
        Ok(())
    }

    /// `whatsapp_pair`: (re)start pairing.
    pub(crate) async fn pair(&self) -> Result<Value> {
        let inner = self.inner()?;
        self.ensure_pairing(inner).await?;
        Ok(self.status())
    }

    /// `whatsapp_unpair`: stop the connection, delete the linked-device
    /// session, and return to `unpaired`. `Client::disconnect` is final for an
    /// instance, so unpair drops the whole connection and rebuilds from a
    /// clean `session.db` on the next pair.
    pub(crate) async fn unpair(&self) -> Result<Value> {
        let inner = self.inner()?;
        {
            let mut slot = inner.slot.lock().await;
            if let Some(task) = slot.connection.take() {
                task.abort();
            }
            if let Some(pump) = slot.pump.take() {
                pump.abort();
            }
        }
        let _ = inner.client_tx.send(None);
        let session_db = inner.root.join("session.db");
        match std::fs::remove_file(&session_db) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("removing {}", session_db.display()));
            }
        }
        let payload = status_payload(&State::Unpaired, None);
        let _ = self.status.send(payload.clone());
        Ok(payload)
    }

    /// `whatsapp_chats`: the chats the bridge has seen, with their recording
    /// and binding state plus the display metadata from each chat directory's
    /// `meta.json`, most recently seen first.
    pub(crate) async fn chats(&self) -> Result<Value> {
        let inner = self.inner()?;
        let mut settings = inner.settings.lock().await;
        let mut chats: Vec<Value> = Vec::new();
        for (jid, chat) in &settings.get().chats {
            let meta = chat.dir.as_ref().and_then(|dir| {
                let path = inner.root.join("chats").join(dir).join("meta.json");
                let bytes = std::fs::read(path).ok()?;
                serde_json::from_slice::<ChatMeta>(&bytes).ok()
            });
            chats.push(json!({
                "jid": jid,
                "dir": chat.dir,
                "record": chat.record,
                "conversation": chat.conversation,
                "owner": chat.owner,
                "name": meta.as_ref().map(|meta| meta.display_name.clone()),
                "kind": meta.as_ref().map(|meta| meta.kind.clone()),
                "last_seen": meta.as_ref().map(|meta| meta.last_seen),
            }));
        }
        chats.sort_by_key(|chat| std::cmp::Reverse(chat["last_seen"].as_u64().unwrap_or(0)));
        Ok(json!({"chats": chats}))
    }

    /// `whatsapp_set_chat`: recording enablement and conversation binding.
    /// Binding requires the conversation to exist in the requesting tenant and
    /// records that tenant's owner key. An absent conversation id leaves the
    /// binding unchanged — the Record toggle sends only `{jid, record}` and
    /// must not disturb a binding; an empty id clears it, which is what the
    /// bind `<select>` sends for "Not bound". A newly created or changed
    /// binding primes `last_forwarded_entry` at the conversation's current
    /// latest assistant entry, so the forwarder's first pass is a no-op and
    /// binding a chat does not replay the transcript into WhatsApp. The
    /// forwarder set's periodic `sync` picks a new binding up within one
    /// interval.
    pub(crate) async fn set_chat(
        &self,
        tenant: &Tenant,
        jid: &str,
        record: Option<bool>,
        conversation_id: Option<String>,
    ) -> Result<Value> {
        let inner = self.inner()?;
        if jid.is_empty() {
            bail!("A chat jid is required.");
        }
        let mut settings = inner.settings.lock().await;
        if let Some(record) = record {
            settings.set_record(jid, record)?;
        }
        match conversation_id.as_deref().map(str::trim) {
            None => {}
            Some("") => settings.clear_binding(jid)?,
            Some(id) => {
                let chat = tenant.get(id)?;
                // Prime the watermark for every fresh binding: a chat with no
                // settings entry yet, or one whose binding differs from what
                // is being set. Re-asserting the same binding leaves it
                // untouched.
                let rebinding = settings.chat(jid).is_none_or(|chat| {
                    chat.conversation.as_deref() != Some(id)
                        || chat.owner.as_deref() != Some(&tenant.runtime.owner)
                });
                settings.set_binding(jid, id, &tenant.runtime.owner)?;
                if rebinding {
                    // Prime the forwarder watermark at the latest assistant
                    // entry already in the conversation: only what the agent
                    // says after the bind may forward, never the backlog.
                    let data = chat.data();
                    if let Some(latest) = data
                        .entries
                        .iter()
                        .rev()
                        .find(|entry| entry.role == "assistant")
                    {
                        settings.set_last_forwarded_entry(jid, &latest.id)?;
                    }
                }
            }
        }
        let chat = settings.chat(jid);
        Ok(json!({
            "jid": jid,
            "record": chat.as_ref().is_some_and(|chat| chat.record),
            "conversation": chat.and_then(|chat| chat.conversation),
        }))
    }

    /// `whatsapp_allow`: add or remove a number from the default-deny
    /// allowlist, normalized to digits without a plus. The first mutation on
    /// a fresh install creates `settings.json`.
    pub(crate) async fn set_allow(&self, number: &str, allow: bool) -> Result<Value> {
        let inner = self.inner()?;
        let phone: String = number.chars().filter(|ch| ch.is_ascii_digit()).collect();
        if phone.is_empty() {
            bail!("A phone number is required.");
        }
        let mut settings = inner.settings.lock().await;
        settings.set_allow(&phone, allow)?;
        let allow = settings.get().allow.clone();
        drop(settings);
        Ok(json!({"allow": allow}))
    }
}

/// Test support: build a loaded tenant with one created conversation.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Cs;
    use crate::helper::{Operation, Request};
    use crate::model::digest;
    use crate::{BrowserContext, owner_key};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use whatsapp_bridge::client::ConnEvent;
    use whatsapp_bridge::pairing::Pairing;

    struct Fixture {
        _dir: tempfile::TempDir,
        state: Arc<AppState>,
        tenant: Arc<Tenant>,
        chat: Arc<crate::session::Chat>,
        owner: String,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState {
            token: "token".into(),
            config: config::Config::default(),
            cs: Arc::new(Cs::for_test(dir.path().join("cs"))),
            store: crate::store::Store::new(dir.path().join("data")).unwrap(),
            address: "127.0.0.1:1234".parse().unwrap(),
            executable: PathBuf::from("/helper"),
            tenants: Mutex::new(HashMap::new()),
            whatsapp: std::sync::OnceLock::new(),
        });
        let tenant = state
            .tenant("scope".into(), Some("workspace".into()))
            .await
            .unwrap();
        let chat = tenant
            .create(
                "conversation".into(),
                "create".into(),
                "codex".into(),
                "Fix the build".into(),
                BrowserContext {
                    window_id: Some("window".into()),
                    tab_id: None,
                    pane_id: None,
                },
            )
            .await
            .unwrap();
        let owner = owner_key("scope", Some("workspace"));
        assert_eq!(owner, tenant.runtime.owner);
        Fixture {
            _dir: dir,
            state,
            tenant,
            chat,
            owner,
        }
    }

    fn send_request(id: &str, body: &str) -> SendRequest {
        SendRequest {
            id: id.to_string(),
            fingerprint: digest(body.as_bytes()),
            body: body.to_string(),
            question: None,
            cancel: false,
        }
    }

    async fn ask_question(chat: &Arc<crate::session::Chat>) {
        let token = chat.data().run.token;
        chat.agent(
            token.clone(),
            Request {
                id: "ready".into(),
                action: Operation::Ready,
            },
        )
        .await
        .unwrap();
        let message = chat.data().entries[0].id.clone();
        chat.agent(
            token.clone(),
            Request {
                id: "read".into(),
                action: Operation::Read {
                    message: message.clone(),
                },
            },
        )
        .await
        .unwrap();
        chat.agent(
            token,
            Request {
                id: "ask".into(),
                action: Operation::Ask {
                    to: message,
                    body: "Where should we run this?".into(),
                    options: vec!["Local".into(), "Remote".into()],
                },
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn resolve_reports_title_phase_queue_and_pending_counts() {
        let fixture = fixture().await;
        let Resolved::Found(view) = fixture.state.resolve(&fixture.owner, "conversation").await
        else {
            panic!("a loaded conversation must resolve");
        };
        assert_eq!(view.title, "Fix the build");
        assert_eq!(view.phase, "Booting", "a fresh run is starting");
        assert_eq!(view.queue_depth, 1, "the opening user entry is unread");
        assert_eq!(view.pending_questions, 0);
        assert_eq!(view.entries.len(), 1);
        assert_eq!(view.entries[0].role, "user");
        assert_eq!(view.entries[0].kind, "message");
        assert_eq!(view.entries[0].agent_name, "codex");

        ask_question(&fixture.chat).await;
        let Resolved::Found(view) = fixture.state.resolve(&fixture.owner, "conversation").await
        else {
            panic!("a waiting conversation must resolve");
        };
        assert_eq!(view.phase, "Waiting");
        assert_eq!(view.queue_depth, 0, "the user entry was read");
        assert_eq!(view.pending_questions, 1);
        let question = view
            .entries
            .iter()
            .find(|entry| entry.kind == "question")
            .expect("a question entry");
        assert_eq!(question.agent_name, "codex");
        let question = question.question.as_ref().expect("the question payload");
        assert_eq!(question.options, ["Local", "Remote"]);
        assert_eq!(question.status, QuestionStatus::Pending);
    }

    #[tokio::test]
    async fn resolve_distinguishes_not_loaded_missing_and_stopped() {
        let fixture = fixture().await;
        assert_eq!(
            fixture.state.resolve("never-loaded", "conversation").await,
            Resolved::NotLoaded
        );
        assert_eq!(
            fixture.state.resolve(&fixture.owner, "unknown").await,
            Resolved::Missing
        );
        fixture
            .chat
            .stop("stop-1".into(), "stop".into())
            .await
            .expect_err("the fake cs cannot confirm the close");
        assert_eq!(
            fixture.state.resolve(&fixture.owner, "conversation").await,
            Resolved::Stopped
        );
    }

    #[tokio::test]
    async fn send_records_entries_and_dedupes_idempotency_pairs() {
        let fixture = fixture().await;
        let entries = || fixture.chat.data().entries.len();
        let before = entries();

        let send = send_request("wa-1", "fix it");
        fixture
            .state
            .send(&fixture.owner, "conversation", send.clone())
            .await
            .unwrap();
        assert_eq!(entries(), before + 1);

        // The same (id, fingerprint) pair is a silent retry: the recorded
        // receipt returns and no new entry appears.
        fixture
            .state
            .send(&fixture.owner, "conversation", send.clone())
            .await
            .unwrap();
        assert_eq!(entries(), before + 1);

        // The same id with different content is rejected.
        let mismatched = SendRequest {
            fingerprint: digest(b"other"),
            ..send.clone()
        };
        fixture
            .state
            .send(&fixture.owner, "conversation", mismatched)
            .await
            .expect_err("a reused id with new content must fail");
        assert_eq!(entries(), before + 1);
    }

    #[tokio::test]
    async fn send_to_a_never_loaded_tenant_is_a_no_agent_error() {
        let fixture = fixture().await;
        let error = fixture
            .state
            .send("never-loaded", "conversation", send_request("wa-1", "hi"))
            .await
            .expect_err("a missing tenant must not be created");
        assert_eq!(format!("{error:#}"), REPLY_NO_AGENT);
    }

    #[tokio::test]
    async fn subscribe_fires_on_tenant_updates() {
        let fixture = fixture().await;
        assert!(fixture.state.subscribe("never-loaded").await.is_none());
        let mut updates = fixture
            .state
            .subscribe(&fixture.owner)
            .await
            .expect("a loaded tenant has a channel");
        fixture.tenant.updates.send(()).unwrap();
        updates
            .recv()
            .await
            .expect("the subscription observes the tenant update");
    }

    #[test]
    fn status_payload_has_exactly_the_plans_keys() {
        let mut pairing = Pairing::new();
        pairing.apply(&ConnEvent::PairingQrCode {
            code: "ref,noise,identity,adv,web".to_string(),
            timeout: Duration::from_secs(60),
        });
        let payload = status_payload(&State::Unpaired, Some(pairing.snapshot()));
        let mut keys: Vec<_> = payload.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "error",
                "jid",
                "push_name",
                "qr_deep_link",
                "qr_expires_at",
                "qr_raw",
                "qr_svg",
                "state",
            ]
        );
        assert_eq!(payload["state"], "pairing");
        assert!(
            payload["qr_svg"]
                .as_str()
                .is_some_and(|svg| svg.contains("<svg"))
        );
        assert_eq!(
            payload["qr_raw"].as_str(),
            Some("ref,noise,identity,adv,web")
        );
        assert!(
            payload["qr_deep_link"]
                .as_str()
                .is_some_and(|link| link.starts_with("https://wa.me/"))
        );
        assert!(payload["qr_expires_at"].as_i64().is_some());
        assert!(payload["jid"].is_null());
        assert!(payload["push_name"].is_null());
        assert!(payload["error"].is_null());

        // Disabled and locked report themselves with everything else null.
        for state in [State::Disabled, State::Locked] {
            let payload = status_payload(&state, None);
            assert_eq!(
                payload["state"],
                if state == State::Disabled {
                    "disabled"
                } else {
                    "locked"
                }
            );
            for key in [
                "jid",
                "push_name",
                "qr_svg",
                "qr_raw",
                "qr_deep_link",
                "error",
            ] {
                assert!(payload[key].is_null(), "{key} must be null for {state:?}");
            }
            assert!(payload["qr_expires_at"].is_null());
        }

        // A pairing error surfaces as the error state with the reason.
        let mut pairing = Pairing::new();
        pairing.apply(&ConnEvent::PairingQrCodesExhausted { disconnected: true });
        pairing.apply(&ConnEvent::TemporaryBan {
            detail: "sent to too many people".to_string(),
            expire: Duration::from_secs(3600),
        });
        let payload = status_payload(&State::Unpaired, Some(pairing.snapshot()));
        assert_eq!(payload["state"], "error");
        assert!(
            payload["error"]
                .as_str()
                .is_some_and(|error| error.contains("temporary ban"))
        );
    }

    #[test]
    fn inactive_questions_map_to_cancelled() {
        assert_eq!(
            question_status(&ModelQuestionStatus::Inactive),
            QuestionStatus::Cancelled
        );
        assert_eq!(
            question_status(&ModelQuestionStatus::Answered),
            QuestionStatus::Answered
        );
    }

    /// A `Whatsapp` with a live settings store but no connection machinery,
    /// for `set_chat`/`set_allow` tests. The store starts without a
    /// `settings.json`, like a fresh install.
    async fn whatsapp_only() -> (Arc<Whatsapp>, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let wa_root = root.path().join("wa");
        std::fs::create_dir_all(&wa_root).unwrap();
        let bridge = whatsapp_bridge::Bridge::new(
            false,
            BridgeConfig {
                log_max_bytes: 1024 * 1024,
                log_keep: 10,
                media_max_bytes: 1000,
                media: Vec::new(),
                reply_progress: true,
            },
            &wa_root,
        )
        .unwrap();
        let whatsapp = Arc::new(Whatsapp {
            status: watch::channel(Value::Null).0,
            bridge,
            inner: Some(Inner {
                root: wa_root.clone(),
                settings: Arc::new(tokio::sync::Mutex::new(
                    SettingsStore::new(wa_root.join("settings.json")).unwrap(),
                )),
                inbound_tx: mpsc::channel(1).0,
                client_tx: watch::channel(None).0,
                slot: tokio::sync::Mutex::new(Slot::default()),
            }),
        });
        (whatsapp, root)
    }

    async fn jid_chat(whatsapp: &Whatsapp, jid: &str) -> whatsapp_bridge::settings::ChatSettings {
        whatsapp
            .inner()
            .unwrap()
            .settings
            .lock()
            .await
            .chat(jid)
            .unwrap()
    }

    #[tokio::test]
    async fn set_chat_record_toggle_leaves_the_binding_and_empty_id_clears_it() {
        let fixture = fixture().await;
        let (whatsapp, _root) = whatsapp_only().await;
        let jid = "5511987654321@s.whatsapp.net";

        // Bind, then toggle Record the way the Record button does — with no
        // conversation id at all. The binding must hold.
        whatsapp
            .set_chat(
                &fixture.tenant,
                jid,
                Some(true),
                Some("conversation".into()),
            )
            .await
            .unwrap();
        whatsapp
            .set_chat(&fixture.tenant, jid, Some(false), None)
            .await
            .unwrap();
        let chat = jid_chat(&whatsapp, jid).await;
        assert!(!chat.record);
        assert_eq!(chat.conversation.as_deref(), Some("conversation"));

        // Toggling back on still leaves the binding alone.
        whatsapp
            .set_chat(&fixture.tenant, jid, Some(true), None)
            .await
            .unwrap();
        assert!(jid_chat(&whatsapp, jid).await.record);

        // "Not bound" sends an empty id; that clears, keeping recording.
        let payload = whatsapp
            .set_chat(&fixture.tenant, jid, None, Some(String::new()))
            .await
            .unwrap();
        assert!(payload["conversation"].is_null());
        let chat = jid_chat(&whatsapp, jid).await;
        assert!(chat.conversation.is_none());
        assert!(chat.record, "clearing the binding keeps recording");
    }

    #[tokio::test]
    async fn set_chat_primes_the_forward_watermark_only_for_a_fresh_binding() {
        let fixture = fixture().await;
        let (whatsapp, _root) = whatsapp_only().await;
        let jid = "5511987654321@s.whatsapp.net";
        ask_question(&fixture.chat).await;
        let latest = fixture
            .chat
            .data()
            .entries
            .iter()
            .rev()
            .find(|entry| entry.role == "assistant")
            .expect("the question is an assistant entry")
            .id
            .clone();

        // A new binding primes the watermark at the conversation's current
        // latest assistant entry, so the forwarder's first pass is a no-op
        // and the backlog does not replay into WhatsApp.
        whatsapp
            .set_chat(
                &fixture.tenant,
                jid,
                Some(true),
                Some("conversation".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            jid_chat(&whatsapp, jid)
                .await
                .last_forwarded_entry
                .as_deref(),
            Some(latest.as_str())
        );

        // Re-binding to the same conversation leaves the watermark untouched.
        whatsapp
            .inner()
            .unwrap()
            .settings
            .lock()
            .await
            .set_last_forwarded_entry(jid, "custom")
            .unwrap();
        whatsapp
            .set_chat(
                &fixture.tenant,
                jid,
                Some(true),
                Some("conversation".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            jid_chat(&whatsapp, jid)
                .await
                .last_forwarded_entry
                .as_deref(),
            Some("custom")
        );

        // Binding a jid that has no settings entry at all (record untouched)
        // primes too.
        let fresh = "447700900124@s.whatsapp.net";
        whatsapp
            .set_chat(&fixture.tenant, fresh, None, Some("conversation".into()))
            .await
            .unwrap();
        assert_eq!(
            jid_chat(&whatsapp, fresh)
                .await
                .last_forwarded_entry
                .as_deref(),
            Some(latest.as_str())
        );
    }

    #[tokio::test]
    async fn set_allow_creates_settings_json_on_a_fresh_install() {
        let (whatsapp, root) = whatsapp_only().await;
        let path = root.path().join("wa/settings.json");
        assert!(!path.exists(), "a fresh install has no settings.json");

        let payload = whatsapp.set_allow("+55 11 98765-4321", true).await.unwrap();
        assert!(path.is_file(), "the first mutation creates the file");
        assert_eq!(payload["allow"], json!(["5511987654321"]));

        let payload = whatsapp.set_allow("5511987654321", false).await.unwrap();
        assert_eq!(payload["allow"], json!([]));
    }
}
