//! Mobile Chat's embedded UI, conversation service, and agent helper.

mod config;
mod control;
mod helper;
mod markdown;
mod model;
mod session;
mod store;
#[cfg(feature = "whatsapp")]
mod whatsapp;

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use config::Config;
use control::Cs;
use model::{MAX_FRAME, ViewState, digest, new_id, valid_id};
use session::{Chat, NewConversation, Runtime};
use store::Store;

const SCOPE_HEADER: &str = "x-chan-extension-scope";
const WORKSPACE_HEADER: &str = "x-chan-extension-workspace";
const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");

#[derive(Debug, Parser)]
#[command(
    name = "mobile-chat-extension",
    about = "Persistent chat for Chan terminal agents"
)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    /// Defaults to <chan-home>/mobile-chat.toml.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Private conversation storage. Defaults to <chan-home>/mobile-chat.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    mode: Option<Mode>,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Communicate with the user's Mobile Chat conversation.
    Agent(helper::AgentCli),
}

struct AppState {
    token: String,
    config: Config,
    cs: Arc<Cs>,
    store: Store,
    address: SocketAddr,
    executable: PathBuf,
    tenants: Mutex<HashMap<String, Arc<Tenant>>>,
    /// The process-wide WhatsApp bridge, initialized once in `main` after
    /// the rest of the state exists (the bridge's `Host` seam points back
    /// here).
    #[cfg(feature = "whatsapp")]
    whatsapp: std::sync::OnceLock<Arc<whatsapp::Whatsapp>>,
}

struct Tenant {
    runtime: Arc<Runtime>,
    chats: Mutex<HashMap<String, Arc<Chat>>>,
    bindings: Mutex<BTreeMap<String, String>>,
    errors: Vec<String>,
    updates: broadcast::Sender<()>,
}

impl AppState {
    async fn tenant(
        self: &Arc<Self>,
        scope: String,
        workspace: Option<String>,
    ) -> Result<Arc<Tenant>> {
        let owner = owner_key(&scope, workspace.as_deref());
        if let Some(tenant) = self.tenants.lock().expect("tenants lock").get(&owner)
            && tenant.runtime.scope == scope
        {
            return Ok(Arc::clone(tenant));
        }
        let app = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut tenants = app.tenants.lock().expect("tenants lock");
            if let Some(tenant) = tenants.get(&owner)
                && tenant.runtime.scope == scope
            {
                return Ok(Arc::clone(tenant));
            }
            if let Some(previous) = tenants.get(&owner) {
                previous.runtime.active.store(false, Ordering::Release);
                for chat in previous.chats.lock().expect("chats lock").values() {
                    chat.retire();
                }
                // Wait for any binding write accepted by the previous scope.
                drop(previous.bindings.lock().expect("bindings lock"));
                let _ = previous.updates.send(());
            }
            let runtime = Arc::new(Runtime {
                active: AtomicBool::new(true),
                config: app.config.clone(),
                cs: Arc::clone(&app.cs),
                store: app.store.owner(&owner)?,
                owner: owner.clone(),
                scope,
                address: app.address,
                executable: app.executable.clone(),
            });
            let (data, errors) = runtime.store.load_all()?;
            let bindings = runtime.store.bindings()?;
            let (updates, _) = broadcast::channel(32);
            let mut chats = HashMap::new();
            for data in data {
                let id = data.id.clone();
                chats.insert(id, Chat::open(data, Arc::clone(&runtime), updates.clone())?);
            }
            let tenant = Arc::new(Tenant {
                runtime,
                chats: Mutex::new(chats),
                bindings: Mutex::new(bindings),
                errors,
                updates,
            });
            tenants.insert(owner, Arc::clone(&tenant));
            Ok::<_, anyhow::Error>(tenant)
        })
        .await
        .context("opening conversations")?
    }
}

fn owner_key(scope: &str, workspace: Option<&str>) -> String {
    digest(
        format!(
            "{}:{}",
            if workspace.is_some() {
                "workspace"
            } else {
                "runtime"
            },
            workspace.unwrap_or(scope)
        )
        .as_bytes(),
    )
}

impl Tenant {
    fn get(&self, id: &str) -> Result<Arc<Chat>> {
        self.chats
            .lock()
            .expect("chats lock")
            .get(id)
            .cloned()
            .context("Conversation does not exist in this workspace.")
    }

    fn activate(&self) {
        for chat in self.chats.lock().expect("chats lock").values() {
            chat.activate();
        }
    }

    async fn create(
        self: &Arc<Self>,
        id: String,
        fingerprint: String,
        agent: String,
        text: String,
        context: BrowserContext,
    ) -> Result<Arc<Chat>> {
        let tenant = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut chats = tenant.chats.lock().expect("chats lock");
            tenant.runtime.ensure_active()?;
            if let Some(chat) = chats.get(&id) {
                chat.data()
                    .receipt(&id, &fingerprint)?
                    .context("Conversation ID is already in use.")?;
                return Ok(Arc::clone(chat));
            }
            let agent = tenant
                .runtime
                .config
                .roster()
                .into_iter()
                .find(|a| a.name == agent)
                .context("Agent is not in the configured roster.")?;
            let chat = Chat::create(
                NewConversation {
                    id: id.clone(),
                    fingerprint,
                    agent,
                    body: text,
                    window_id: context
                        .window_id
                        .context("Chan has not sent this window's context yet.")?,
                    pane_id: context.pane_id,
                },
                Arc::clone(&tenant.runtime),
                tenant.updates.clone(),
            )?;
            chats.insert(id, Arc::clone(&chat));
            Ok::<_, anyhow::Error>(chat)
        })
        .await
        .context("creating conversation")?
    }

    async fn bind(self: &Arc<Self>, tab: Option<String>, conversation: String) -> Result<()> {
        let Some(tab) = tab else { return Ok(()) };
        if !valid_id(&tab) {
            bail!("Invalid tab identity.");
        }
        self.get(&conversation)?;
        let tenant = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut bindings = tenant.bindings.lock().expect("bindings lock");
            tenant.runtime.ensure_active()?;
            let mut next = bindings.clone();
            next.insert(tab, conversation);
            tenant.runtime.store.save_bindings(&next)?;
            *bindings = next;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("saving tab binding")?
    }

    fn snapshot(&self, active: Option<&str>, persistent: bool) -> Value {
        let chats = self.chats.lock().expect("chats lock");
        let mut summaries: Vec<_> = chats.values().map(|chat| chat.data().summary()).collect();
        summaries
            .sort_by_key(|summary| std::cmp::Reverse(summary["updated_at"].as_u64().unwrap_or(0)));
        json!({
            "type": "snapshot", "agents": self.runtime.config.roster().iter().map(|a| &a.name).collect::<Vec<_>>(),
            "conversations": summaries, "errors": self.errors,
            "conversation": active.and_then(|id| chats.get(id)).map(|chat| chat.data().snapshot()),
            "persistent_host": persistent,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Mode::Agent(agent)) = cli.mode {
        return helper::run(agent).await;
    }
    if !cli.listen.is_ipv4() || !cli.listen.ip().is_loopback() {
        bail!("--listen must use IPv4 loopback");
    }
    let config = Config::load(&cli.config.unwrap_or_else(config::default_config_path))?;
    let cs = Cs::locate()?;
    eprintln!("mobile-chat: cs at {}", cs.binary().display());
    let listener = TcpListener::bind(cli.listen)
        .await
        .context("binding extension server")?;
    let address = listener.local_addr()?;
    let token = format!("{}{}", new_id(), new_id());
    let state = Arc::new(AppState {
        token: token.clone(),
        config,
        cs,
        store: Store::new(
            cli.data_dir
                .unwrap_or_else(|| config::chan_home().join("mobile-chat")),
        )?,
        address,
        executable: std::env::current_exe()?,
        tenants: Mutex::new(HashMap::new()),
        #[cfg(feature = "whatsapp")]
        whatsapp: std::sync::OnceLock::new(),
    });
    #[cfg(feature = "whatsapp")]
    if let Some(whats) = whatsapp::start(Arc::clone(&state)).await?
        && state.whatsapp.set(whats).is_err()
    {
        bail!("The WhatsApp bridge initialized twice.");
    }
    let app = router(state);
    println!("CHAN_EXTENSION_V1={}", handshake(address, &token));
    use std::io::Write;
    std::io::stdout().flush()?;
    axum::serve(listener, app)
        .await
        .context("serving Mobile Chat")
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.css", get(app_css))
        .route("/app.js", get(app_js))
        .route("/control", get(control_socket))
        .route("/agent", get(agent_socket))
        .with_state(state)
}

fn handshake(address: SocketAddr, token: &str) -> Value {
    json!({"url": format!("http://{address}/"), "token": token, "singleton": false})
}

#[derive(Deserialize)]
struct AuthQuery {
    #[serde(default)]
    t: String,
}

fn check_auth(state: &AppState, auth: &AuthQuery) -> Result<(), StatusCode> {
    if auth.t == state.token {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Result<String, StatusCode> {
    let value = headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    if value.is_empty() || value.len() > 256 {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(value.to_string())
}

macro_rules! asset {
    ($name:ident, $content_type:literal, $body:expr) => {
        async fn $name(
            State(state): State<Arc<AppState>>,
            Query(auth): Query<AuthQuery>,
        ) -> Result<Response, StatusCode> {
            check_auth(&state, &auth)?;
            Ok(Response::builder()
                .header(header::CONTENT_TYPE, $content_type)
                .header(header::CACHE_CONTROL, "no-store")
                .body($body.into())
                .expect("embedded asset response"))
        }
    };
}

asset!(index, "text/html; charset=utf-8", INDEX_HTML);
asset!(app_css, "text/css; charset=utf-8", APP_CSS);
asset!(app_js, "text/javascript; charset=utf-8", APP_JS);

async fn control_socket(
    State(state): State<Arc<AppState>>,
    Query(auth): Query<AuthQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    check_auth(&state, &auth)?;
    let scope = header_value(&headers, SCOPE_HEADER)?;
    let workspace = if headers.contains_key(WORKSPACE_HEADER) {
        Some(header_value(&headers, WORKSPACE_HEADER)?)
    } else {
        None
    };
    let persistent = workspace.is_some();
    let tenant = state.tenant(scope, workspace).await.map_err(|error| {
        eprintln!("mobile-chat: {error:#}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    tenant.activate();
    #[cfg(feature = "whatsapp")]
    let whatsapp = state.whatsapp.get().cloned();
    Ok(upgrade
        .max_message_size(MAX_FRAME)
        .on_upgrade(move |socket| {
            serve_control(
                tenant,
                persistent,
                #[cfg(feature = "whatsapp")]
                whatsapp,
                socket,
            )
        }))
}

#[derive(Clone, Default)]
struct BrowserContext {
    window_id: Option<String>,
    tab_id: Option<String>,
    pane_id: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct ClientRequest {
    id: String,
    #[serde(flatten)]
    action: ClientAction,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ClientAction {
    Hello {
        window_id: String,
        tab_id: Option<String>,
        pane_id: Option<String>,
    },
    Create {
        agent: String,
        text: String,
    },
    Attach {
        conversation_id: String,
    },
    Send {
        conversation_id: String,
        text: String,
    },
    Answer {
        conversation_id: String,
        question_id: String,
        text: String,
        #[serde(default)]
        cancel: bool,
    },
    SaveView {
        conversation_id: String,
        revision: u64,
        view: ViewState,
    },
    History {
        conversation_id: String,
        before: String,
    },
    Peek {
        conversation_id: String,
    },
    Stop {
        conversation_id: String,
    },
    ConnectAgent {
        conversation_id: String,
    },
    #[cfg(feature = "whatsapp")]
    WhatsappStatus,
    #[cfg(feature = "whatsapp")]
    WhatsappPair,
    #[cfg(feature = "whatsapp")]
    WhatsappUnpair,
    #[cfg(feature = "whatsapp")]
    WhatsappChats,
    #[cfg(feature = "whatsapp")]
    WhatsappSetChat {
        jid: String,
        #[serde(default)]
        record: Option<bool>,
        #[serde(default)]
        conversation_id: Option<String>,
    },
    #[cfg(feature = "whatsapp")]
    WhatsappAllow {
        number: String,
        allow: bool,
    },
}

async fn handle_client(
    tenant: &Arc<Tenant>,
    context: &mut BrowserContext,
    active: &mut Option<String>,
    #[cfg(feature = "whatsapp")] whatsapp: Option<&whatsapp::Whatsapp>,
    request: ClientRequest,
) -> Result<Value> {
    tenant.runtime.ensure_active()?;
    if !valid_id(&request.id) {
        bail!("Invalid request identity.");
    }
    let fingerprint = digest(&serde_json::to_vec(&request.action)?);
    match request.action {
        ClientAction::Hello {
            window_id,
            tab_id,
            pane_id,
        } => {
            if !valid_id(&window_id)
                || tab_id.as_ref().is_some_and(|s| !valid_id(s))
                || pane_id.as_ref().is_some_and(|s| !valid_id(s))
            {
                bail!("Invalid host context.");
            }
            if context.tab_id != tab_id {
                *active = tab_id.as_ref().and_then(|id| {
                    tenant
                        .bindings
                        .lock()
                        .expect("bindings lock")
                        .get(id)
                        .cloned()
                });
            }
            *context = BrowserContext {
                window_id: Some(window_id),
                tab_id,
                pane_id,
            };
            Ok(json!({"conversation_id": active}))
        }
        ClientAction::Create { agent, text } => {
            let chat = tenant
                .create(request.id, fingerprint, agent, text, context.clone())
                .await?;
            let id = chat.data().id;
            tenant.bind(context.tab_id.clone(), id.clone()).await?;
            *active = Some(id.clone());
            chat.activate();
            Ok(json!({"conversation_id": id}))
        }
        ClientAction::Attach { conversation_id } => {
            tenant.get(&conversation_id)?;
            tenant
                .bind(context.tab_id.clone(), conversation_id.clone())
                .await?;
            *active = Some(conversation_id.clone());
            Ok(json!({"conversation_id": conversation_id}))
        }
        ClientAction::Send {
            conversation_id,
            text,
        } => {
            tenant
                .get(&conversation_id)?
                .send(request.id, fingerprint, text, None, false)
                .await
        }
        ClientAction::Answer {
            conversation_id,
            question_id,
            text,
            cancel,
        } => {
            tenant
                .get(&conversation_id)?
                .send(request.id, fingerprint, text, Some(question_id), cancel)
                .await
        }
        ClientAction::SaveView {
            conversation_id,
            revision,
            view,
        } => {
            tenant
                .get(&conversation_id)?
                .save_view(revision, view)
                .await
        }
        ClientAction::History {
            conversation_id,
            before,
        } => tenant.get(&conversation_id)?.data().page(Some(&before)),
        ClientAction::Peek { conversation_id } => tenant.get(&conversation_id)?.peek().await,
        ClientAction::Stop { conversation_id } => {
            tenant
                .get(&conversation_id)?
                .stop(request.id, fingerprint)
                .await
        }
        ClientAction::ConnectAgent { conversation_id } => {
            tenant.get(&conversation_id)?.connect_agent().await
        }
        #[cfg(feature = "whatsapp")]
        ClientAction::WhatsappStatus => Ok(whatsapp.context("WhatsApp is disabled.")?.status()),
        #[cfg(feature = "whatsapp")]
        ClientAction::WhatsappPair => whatsapp.context("WhatsApp is disabled.")?.pair().await,
        #[cfg(feature = "whatsapp")]
        ClientAction::WhatsappUnpair => whatsapp.context("WhatsApp is disabled.")?.unpair().await,
        #[cfg(feature = "whatsapp")]
        ClientAction::WhatsappChats => whatsapp.context("WhatsApp is disabled.")?.chats().await,
        #[cfg(feature = "whatsapp")]
        ClientAction::WhatsappSetChat {
            jid,
            record,
            conversation_id,
        } => {
            let whatsapp = whatsapp.context("WhatsApp is disabled.")?;
            whatsapp
                .set_chat(tenant, &jid, record, conversation_id)
                .await
        }
        #[cfg(feature = "whatsapp")]
        ClientAction::WhatsappAllow { number, allow } => {
            whatsapp
                .context("WhatsApp is disabled.")?
                .set_allow(&number, allow)
                .await
        }
    }
}

async fn serve_control(
    tenant: Arc<Tenant>,
    persistent: bool,
    #[cfg(feature = "whatsapp")] whatsapp: Option<Arc<whatsapp::Whatsapp>>,
    socket: WebSocket,
) {
    let (mut sink, mut stream) = socket.split();
    let mut updates = tenant.updates.subscribe();
    // The process-wide WhatsApp status, pushed to every control socket. When
    // the bridge never initialized, a forgotten sender keeps the arm silent.
    #[cfg(feature = "whatsapp")]
    let mut wa_updates = match &whatsapp {
        Some(whats) => whats.subscribe(),
        None => {
            let (tx, rx) = tokio::sync::watch::channel(Value::Null);
            std::mem::forget(tx);
            rx
        }
    };
    let mut context = BrowserContext::default();
    let mut active = None;
    if send_json(&mut sink, tenant.snapshot(None, persistent))
        .await
        .is_err()
    {
        return;
    }
    #[cfg(feature = "whatsapp")]
    if let Some(whats) = &whatsapp
        && send_json(&mut sink, whats.status()).await.is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            update = updates.recv() => {
                if tenant.runtime.ensure_active().is_err() { return; }
                if matches!(update, Err(broadcast::error::RecvError::Closed)) { return; }
                if send_json(&mut sink, tenant.snapshot(active.as_deref(), persistent)).await.is_err() { return; }
            }
            changed = async {
                #[cfg(feature = "whatsapp")]
                {
                    wa_updates.changed().await
                }
                #[cfg(not(feature = "whatsapp"))]
                {
                    std::future::pending::<Result<(), std::convert::Infallible>>().await
                }
            } => {
                #[cfg(feature = "whatsapp")]
                match changed {
                    Ok(()) => {
                        let payload = wa_updates.borrow_and_update().clone();
                        if send_json(&mut sink, payload).await.is_err() { return; }
                    }
                    Err(_) => {
                        // The process-wide status is gone; stop selecting on it.
                        let (tx, rx) = tokio::sync::watch::channel(Value::Null);
                        std::mem::forget(tx);
                        wa_updates = rx;
                    }
                }
                #[cfg(not(feature = "whatsapp"))]
                let _ = changed;
            }
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { return };
                let Message::Text(text) = message else { continue };
                let reply = match serde_json::from_str::<ClientRequest>(&text) {
                    Ok(request) => {
                        let id = request.id.clone();
                        match handle_client(
                            &tenant,
                            &mut context,
                            &mut active,
                            #[cfg(feature = "whatsapp")]
                            whatsapp.as_deref(),
                            request,
                        ).await {
                            Ok(result) => json!({"type": "result", "id": id, "ok": true, "result": result}),
                            Err(error) => json!({"type": "result", "id": id, "ok": false, "error": format!("{error:#}")}),
                        }
                    }
                    Err(error) => json!({"type": "result", "ok": false, "error": format!("Invalid request: {error}")}),
                };
                if send_json(&mut sink, reply).await.is_err() { return; }
                if send_json(&mut sink, tenant.snapshot(active.as_deref(), persistent)).await.is_err() { return; }
            }
        }
    }
}

async fn send_json(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    value: Value,
) -> Result<()> {
    sink.send(Message::Text(serde_json::to_string(&value)?.into()))
        .await?;
    Ok(())
}

async fn agent_socket(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let owner = header_value(&headers, "x-mobile-chat-owner")?;
    let conversation = header_value(&headers, "x-mobile-chat-conversation")?;
    let authorization = header_value(&headers, "authorization")?;
    let token = authorization
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::FORBIDDEN)?
        .to_string();
    let tenant = state
        .tenants
        .lock()
        .expect("tenants lock")
        .get(&owner)
        .cloned()
        .ok_or(StatusCode::FORBIDDEN)?;
    let chat = tenant
        .get(&conversation)
        .map_err(|_| StatusCode::FORBIDDEN)?;
    if token.is_empty() || chat.data().run.token != token {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(upgrade
        .max_message_size(MAX_FRAME)
        .on_upgrade(move |mut socket| async move {
            let request =
                tokio::time::timeout(std::time::Duration::from_secs(30), socket.recv()).await;
            let Ok(Some(Ok(Message::Text(text)))) = request else {
                return;
            };
            let result = match serde_json::from_str::<helper::Request>(&text) {
                Ok(request) => chat.agent(token, request).await,
                Err(error) => Err(error.into()),
            };
            let reply = match result {
                Ok(result) => json!({"ok": true, "result": result}),
                Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
            };
            let _ = socket.send(Message::Text(reply.to_string().into())).await;
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn helper_transport_is_conversation_bound_and_retires_the_previous_scope() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(AppState {
            token: "browser-token".into(),
            config: Config::default(),
            cs: Arc::new(Cs::for_test(PathBuf::from("/unused/cs"))),
            store: Store::new(dir.path().to_path_buf()).unwrap(),
            address,
            executable: PathBuf::from("/helper"),
            tenants: Mutex::new(HashMap::new()),
            #[cfg(feature = "whatsapp")]
            whatsapp: std::sync::OnceLock::new(),
        });
        let router = router(Arc::clone(&state));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let tenant = state
            .tenant("scope-a".into(), Some("workspace".into()))
            .await
            .unwrap();
        let chat = tenant
            .create(
                "conversation".into(),
                "create".into(),
                "codex".into(),
                "Hello".into(),
                BrowserContext {
                    window_id: Some("window-a".into()),
                    tab_id: Some("instance".into()),
                    pane_id: Some("pane-a".into()),
                },
            )
            .await
            .unwrap();
        tenant
            .bind(Some("instance".into()), "conversation".into())
            .await
            .unwrap();
        let connection = helper::Connection {
            address,
            owner: tenant.runtime.owner.clone(),
            conversation: "conversation".into(),
            token: chat.data().run.token,
        };
        let ready = helper::Request {
            id: "ready".into(),
            action: helper::Operation::Ready,
        };
        helper::exchange(&connection, &ready).await.unwrap();
        let read = helper::Request {
            id: "read".into(),
            action: helper::Operation::Read {
                message: chat.data().entries[0].id.clone(),
            },
        };
        assert_eq!(
            helper::exchange(&connection, &read).await.unwrap()["body"],
            "Hello"
        );
        let other = tenant
            .create(
                "other".into(),
                "create-other".into(),
                "claude".into(),
                "Another".into(),
                BrowserContext {
                    window_id: Some("window-a".into()),
                    ..BrowserContext::default()
                },
            )
            .await
            .unwrap();
        let mut wrong = connection.clone();
        wrong.conversation = other.data().id;
        assert!(helper::exchange(&wrong, &ready).await.is_err());
        wrong = connection.clone();
        wrong.owner = "foreign-workspace".into();
        assert!(helper::exchange(&wrong, &ready).await.is_err());
        wrong = connection.clone();
        wrong.token = "browser-token".into();
        assert!(helper::exchange(&wrong, &ready).await.is_err());

        let replacement = state
            .tenant("scope-b".into(), Some("workspace".into()))
            .await
            .unwrap();
        assert_eq!(
            replacement.get("conversation").unwrap().data().run.phase,
            model::Phase::Stopped
        );
        assert_eq!(
            replacement
                .bindings
                .lock()
                .unwrap()
                .get("instance")
                .unwrap(),
            "conversation"
        );
        assert!(helper::exchange(&connection, &ready).await.is_err());
        assert!(chat.save_view(0, ViewState::default()).await.is_err());
        assert!(
            tenant
                .bind(Some("instance".into()), "other".into())
                .await
                .is_err()
        );
        assert!(
            tenant
                .create(
                    "stale-create".into(),
                    "stale".into(),
                    "codex".into(),
                    "Stale".into(),
                    BrowserContext::default()
                )
                .await
                .is_err()
        );
        assert_eq!(replacement.runtime.store.load_all().unwrap().0.len(), 2);
        server.abort();
    }

    #[test]
    fn handshake_has_private_auth_and_allows_independent_tabs() {
        let value = handshake("127.0.0.1:12345".parse().unwrap(), "secret");
        assert_eq!(value["singleton"], false);
        assert_eq!(value["url"], "http://127.0.0.1:12345/");
        assert!(!value.to_string().contains('\n'));
        assert!(value.get("commands").is_none());
    }

    #[test]
    fn trusted_workspace_identity_survives_scope_rotation() {
        assert_eq!(
            owner_key("boot-a", Some("workspace-a")),
            owner_key("boot-b", Some("workspace-a"))
        );
        assert_ne!(
            owner_key("boot-a", Some("workspace-a")),
            owner_key("boot-a", Some("workspace-b"))
        );
        assert_ne!(owner_key("boot-a", None), owner_key("boot-b", None));
    }

    #[test]
    fn private_headers_are_required_and_bounded() {
        let mut headers = HeaderMap::new();
        assert!(header_value(&headers, SCOPE_HEADER).is_err());
        headers.insert(SCOPE_HEADER, "".parse().unwrap());
        assert!(header_value(&headers, SCOPE_HEADER).is_err());
        headers.insert(SCOPE_HEADER, "scope".parse().unwrap());
        assert_eq!(header_value(&headers, SCOPE_HEADER).unwrap(), "scope");
    }

    #[test]
    fn assets_use_relative_urls_and_validate_parent_messages() {
        for body in [INDEX_HTML, APP_CSS, APP_JS] {
            for needle in ["src=\"/", "href=\"/", "url(/", "\"/api/"] {
                assert!(!body.contains(needle));
            }
        }
        assert!(APP_JS.contains("event.source !== window.parent"));
        assert!(APP_CSS.contains("display: none !important"));
    }
}
