//! Mobile Chat: a Chan extension that drives an agent CLI from a chat tab.
//!
//! Chan spawns this process, reads one handshake line from stdout, and reverse
//! proxies the loopback server behind an opaque-origin iframe. The iframe is
//! sandboxed without `allow-same-origin`, so every URL it uses must be relative
//! and every `postMessage` needs a `"*"` target origin with a source check.
//!
//! All mutations ride a single WebSocket rather than POST routes: Chan's proxy
//! answers 403 to POST/PUT/DELETE from non-owner tunnel participants, and a
//! phone reaching this over the Chan tunnel is exactly that case.

mod config;
mod control;
mod session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use config::Config;
use control::Cs;
use session::Chat;

/// Chan's private per-request header naming the serving tenant. It never
/// reaches the browser, so it is the only trustworthy session key.
const SCOPE_HEADER: &str = "x-chan-extension-scope";

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");

#[derive(Debug, Parser)]
#[command(
    name = "mobile-chat-extension",
    about = "Mobile Chat extension for Chan"
)]
struct Cli {
    /// IPv4 loopback address used by Chan's private extension proxy.
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,

    /// Config file. Defaults to `<chan-home>/mobile-chat.toml`.
    #[arg(long)]
    config: Option<PathBuf>,
}

struct AppState {
    token: String,
    config: Config,
    cs: Arc<Cs>,
    chats: Mutex<HashMap<String, Arc<Chat>>>,
}

impl AppState {
    /// One chat per Chan tenant. The scope comes from Chan's private header,
    /// never from the browser, so one tenant cannot address another's session.
    async fn chat(&self, scope: &str) -> Arc<Chat> {
        let mut chats = self.chats.lock().await;
        Arc::clone(
            chats
                .entry(scope.to_string())
                .or_insert_with(|| Chat::new(Arc::clone(&self.cs), self.config.clone())),
        )
    }
}

#[derive(Debug, Deserialize)]
struct AuthQuery {
    #[serde(default)]
    t: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if !cli.listen.is_ipv4() || !cli.listen.ip().is_loopback() {
        bail!("--listen must use IPv4 loopback");
    }

    let config_path = cli.config.unwrap_or_else(config::default_config_path);
    let config =
        Config::load(&config_path).with_context(|| format!("loading {}", config_path.display()))?;
    let cs = Cs::locate()?;
    eprintln!(
        "mobile-chat: config {} ({} agents), cs at {}",
        config_path.display(),
        config.roster().len(),
        cs.binary().display()
    );

    let listener = TcpListener::bind(cli.listen)
        .await
        .with_context(|| format!("binding {}", cli.listen))?;
    let address = listener.local_addr().context("reading bound address")?;
    let token = random_token();

    let state = Arc::new(AppState {
        token: token.clone(),
        config,
        cs,
        chats: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/app.css", get(app_css))
        .route("/app.js", get(app_js))
        .route("/control", get(control_socket))
        .with_state(Arc::clone(&state));

    println!("{}", handshake_line(&handshake(address, &token)));
    // Chan reads exactly one line within 5 seconds. Line buffering to a pipe
    // would usually carry it, but an explicit flush removes the "usually".
    use std::io::Write;
    std::io::stdout()
        .flush()
        .context("flushing the handshake")?;

    axum::serve(listener, app)
        .await
        .context("serving the extension")
}

/// The handshake payload Chan validates against `ExtensionHandshake`.
///
/// Chan already registers an "Apps" launcher entry titled after the
/// declaration's `name`, and that entry opens the tab. Declaring a command with
/// the same title would put a duplicate row in the launcher, so only what the
/// name cannot do on its own is declared here.
fn handshake(address: SocketAddr, token: &str) -> serde_json::Value {
    serde_json::json!({
        "url": format!("http://{address}/"),
        "token": token,
        "singleton": true,
        "commands": [
            {"id": "peek", "title": "Peek at the Agent", "keywords": ["flip", "terminal", "agent"]}
        ]
    })
}

/// The single stdout line Chan parses. The marker is the contract version.
fn handshake_line(handshake: &serde_json::Value) -> String {
    format!("CHAN_EXTENSION_V1={handshake}")
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Chan appends the private `?t=` on the upstream leg and strips any the
/// browser supplied, so a request without the exact token did not come from
/// Chan's proxy.
fn check_auth(state: &AppState, auth: &AuthQuery) -> Result<(), StatusCode> {
    if auth.t == state.token {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn scope(headers: &HeaderMap) -> Result<String, StatusCode> {
    let value = headers
        .get(SCOPE_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    if value.is_empty() || value.len() > 256 {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(value.to_string())
}

fn static_response(content_type: &'static str, body: &'static str) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(body.into())
        .expect("static response builds")
}

macro_rules! embedded {
    ($name:ident, $content_type:literal, $body:expr) => {
        async fn $name(
            State(state): State<Arc<AppState>>,
            Query(auth): Query<AuthQuery>,
        ) -> Result<Response, StatusCode> {
            check_auth(&state, &auth)?;
            Ok(static_response($content_type, $body))
        }
    };
}

embedded!(index, "text/html; charset=utf-8", INDEX_HTML);
embedded!(app_css, "text/css; charset=utf-8", APP_CSS);
embedded!(app_js, "text/javascript; charset=utf-8", APP_JS);

async fn control_socket(
    State(state): State<Arc<AppState>>,
    Query(auth): Query<AuthQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    check_auth(&state, &auth)?;
    let scope = scope(&headers)?;
    Ok(upgrade.on_upgrade(move |socket| serve_control(state, scope, socket)))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ClientMessage {
    Hello { window_id: String },
    Start { agent: String, window_id: String },
    Send { text: String },
    Peek,
    Nudge,
    Escape,
    Restart,
    Close,
}

async fn serve_control(state: Arc<AppState>, scope: String, socket: WebSocket) {
    let chat = state.chat(&scope).await;
    let (mut sink, mut stream) = socket.split();
    let mut updates = chat.subscribe();

    if let Ok(status) = serde_json::to_string(&chat.status().await)
        && sink.send(Message::Text(status.into())).await.is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            pushed = updates.recv() => match pushed {
                Ok(text) => {
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
                // A lagging slow reader just misses intermediate frames; the
                // next status carries the current truth.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { return };
                let Message::Text(text) = message else { continue };
                let reply = handle_client_message(&chat, &text).await;
                // Only the notice goes out here. Every state change already
                // broadcasts its own status, and sending a second one after
                // the command returns races the broadcast: the client would
                // see the newer phase first and then be walked backwards by
                // the queued older one.
                if let Ok(reply) = serde_json::to_string(&reply)
                    && sink.send(Message::Text(reply.into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct Notice {
    r#type: &'static str,
    ok: bool,
    message: String,
}

fn notice(result: Result<String>) -> Notice {
    match result {
        Ok(message) => Notice {
            r#type: "notice",
            ok: true,
            message,
        },
        Err(error) => Notice {
            r#type: "notice",
            ok: false,
            message: format!("{error:#}"),
        },
    }
}

async fn handle_client_message(chat: &Arc<Chat>, text: &str) -> Notice {
    let message: ClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            return Notice {
                r#type: "notice",
                ok: false,
                message: format!("unreadable request: {error}"),
            };
        }
    };
    match message {
        ClientMessage::Hello { window_id } => notice(chat.hello(&window_id).await),
        ClientMessage::Start { agent, window_id } => notice(
            chat.start(&agent, &window_id)
                .await
                .map(|()| format!("starting {agent}")),
        ),
        ClientMessage::Send { text } => notice(chat.send(&text).await),
        ClientMessage::Peek => notice(chat.peek().await),
        ClientMessage::Nudge => notice(chat.nudge().await),
        ClientMessage::Escape => notice(chat.escape().await),
        ClientMessage::Restart => notice(chat.restart().await),
        ClientMessage::Close => notice(chat.close().await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_satisfies_chans_validation_rules() {
        // The real shipped payload, not a copy of it.
        let handshake = handshake("127.0.0.1:49152".parse().unwrap(), &random_token());
        let url = handshake["url"].as_str().unwrap();
        assert!(url.starts_with("http://127.0.0.1:"), "loopback http only");
        assert!(!url.contains("?t="), "chan rejects a pre-existing t param");
        assert!(!url.contains(":0/"), "port 0 is rejected");

        let token = handshake["token"].as_str().unwrap();
        assert!(!token.is_empty() && token.len() <= 4096);

        let commands = handshake["commands"].as_array().unwrap();
        assert!(commands.len() <= 32);
        let mut ids = std::collections::HashSet::new();
        for command in commands {
            let id = command["id"].as_str().unwrap();
            assert!(ids.insert(id), "command ids must be unique");
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && id.len() <= 64
                    && id
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "id {id:?} must match [a-z0-9][a-z0-9-]{{0,63}}"
            );
            let title = command["title"].as_str().unwrap();
            assert!(!title.is_empty() && title.len() <= 128);
            let keywords = command["keywords"].as_array().unwrap();
            assert!(keywords.len() <= 8);
            assert!(
                keywords
                    .iter()
                    .all(|k| k.as_str().is_some_and(|k| k.len() <= 64))
            );
        }
    }

    #[test]
    fn the_handshake_line_is_one_line_behind_chans_marker() {
        let line = handshake_line(&serde_json::json!({"url": "http://127.0.0.1:1/"}));
        assert!(
            line.starts_with("CHAN_EXTENSION_V1="),
            "chan scans stdout for this exact prefix"
        );
        assert!(!line.contains('\n'), "the handshake must be a single line");
        let json = line.strip_prefix("CHAN_EXTENSION_V1=").unwrap();
        serde_json::from_str::<serde_json::Value>(json).expect("the remainder is the JSON payload");
    }

    #[test]
    fn tokens_are_long_and_unpredictable() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 64, "32 bytes as hex");
        assert_ne!(a, b);
    }

    #[test]
    fn auth_rejects_a_missing_or_wrong_token() {
        let state = AppState {
            token: "correct".into(),
            config: Config::default(),
            // check_auth never touches the driver, so a path that does not
            // exist is fine here.
            cs: Arc::new(Cs::for_test(PathBuf::from("/nonexistent/cs"))),
            chats: Mutex::new(HashMap::new()),
        };
        assert!(check_auth(&state, &AuthQuery { t: String::new() }).is_err());
        assert!(check_auth(&state, &AuthQuery { t: "wrong".into() }).is_err());
        assert!(
            check_auth(
                &state,
                &AuthQuery {
                    t: "correct".into()
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn the_scope_header_is_required_and_bounded() {
        let mut headers = HeaderMap::new();
        assert!(scope(&headers).is_err(), "absent scope is forbidden");
        headers.insert(SCOPE_HEADER, "".parse().unwrap());
        assert!(scope(&headers).is_err(), "empty scope is forbidden");
        headers.insert(SCOPE_HEADER, "tenant-1".parse().unwrap());
        assert_eq!(scope(&headers).unwrap(), "tenant-1");
    }

    #[test]
    fn client_messages_parse_from_the_wire_shape_the_ui_sends() {
        let cases = [
            r#"{"op":"hello","window_id":"w-1"}"#,
            r#"{"op":"start","agent":"claude","window_id":"w-1"}"#,
            r#"{"op":"send","text":"hi"}"#,
            r#"{"op":"peek"}"#,
            r#"{"op":"nudge"}"#,
            r#"{"op":"escape"}"#,
            r#"{"op":"restart"}"#,
            r#"{"op":"close"}"#,
        ];
        for case in cases {
            serde_json::from_str::<ClientMessage>(case).unwrap_or_else(|e| panic!("{case}: {e}"));
        }
        assert!(serde_json::from_str::<ClientMessage>(r#"{"op":"rm -rf"}"#).is_err());
    }

    #[test]
    fn the_assets_never_use_an_origin_rooted_url() {
        // The iframe has an opaque origin at a capability path, so an absolute
        // `/foo` would hit Chan's tenant root instead of this extension.
        for (name, body) in [
            ("index.html", INDEX_HTML),
            ("app.css", APP_CSS),
            ("app.js", APP_JS),
        ] {
            for needle in ["src=\"/", "href=\"/", "url(/", "\"/api/", "'/api/"] {
                assert!(
                    !body.contains(needle),
                    "{name} contains an origin-rooted URL: {needle}"
                );
            }
        }
    }

    #[test]
    fn no_declared_command_duplicates_the_launcher_entry_chan_derives_from_name() {
        // `name` in the declaration is "Mobile Chat"; a command with that title
        // renders as a second, identical launcher row.
        let handshake = handshake("127.0.0.1:49152".parse().unwrap(), "t");
        for command in handshake["commands"].as_array().unwrap() {
            assert_ne!(
                command["title"].as_str(),
                Some("Mobile Chat"),
                "a command titled like the extension duplicates the Apps entry"
            );
        }
    }

    #[test]
    fn the_hidden_attribute_outranks_the_flex_layout_rules() {
        // A class-level `display` silently defeats `[hidden]`, which is how the
        // composer and the recovery row leaked into the idle state.
        assert!(
            APP_CSS.contains("[hidden]") && APP_CSS.contains("display: none !important"),
            "app.css must restore [hidden] over the flex display rules"
        );
        for section in ["#recovery", "#composer", "#picker", "#peek", "#detail"] {
            assert!(
                APP_JS.contains(&format!("{}.hidden", section.trim_start_matches('#')))
                    || INDEX_HTML.contains(&format!("id=\"{}\"", section.trim_start_matches('#'))),
                "{section} should exist and be toggled by the render pass"
            );
        }
    }

    #[test]
    fn the_frontend_validates_the_postmessage_source() {
        // Opaque origins force `postMessage(..., "*")`, so the source check is
        // the only thing standing between us and any frame on the page.
        assert!(APP_JS.contains("event.source !== window.parent"));
        assert!(APP_JS.contains("chan:extension-ready:v1"));
        assert!(APP_JS.contains("chan:extension-session-context:v1"));
    }
}
