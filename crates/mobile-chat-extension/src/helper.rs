//! Agent-facing CLI using the same binary as the extension server.

use std::io::{self, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

use crate::model::{MAX_BODY, check_body, new_id};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Connection {
    pub address: SocketAddr,
    pub owner: String,
    pub conversation: String,
    pub token: String,
}

#[derive(Debug, Args)]
pub(crate) struct AgentCli {
    /// Session descriptor. Normally inherited from MOBILE_CHAT_SESSION.
    #[arg(long)]
    session: Option<PathBuf>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Acknowledge the chat-mode bootstrap before receiving user messages.
    Ready,
    /// Read and acknowledge a queued message or survey answer.
    Read { message: String },
    /// Post a reply. Use the same --id when retrying the same reply.
    Reply {
        #[arg(long)]
        id: String,
        #[arg(long)]
        to: String,
        /// Report progress without marking this message complete.
        #[arg(long)]
        progress: bool,
        #[command(flatten)]
        body: Body,
    },
    /// Ask an inline question and return immediately; answers arrive as prompts.
    Ask {
        #[arg(long)]
        id: String,
        #[arg(long)]
        to: String,
        /// Choice label. Repeat for up to eight choices; free text is always available.
        #[arg(long = "option")]
        options: Vec<String>,
        #[command(flatten)]
        body: Body,
    },
}

#[derive(Debug, Args)]
struct Body {
    /// Markdown body. When omitted, read stdin.
    #[arg(conflicts_with = "file")]
    text: Option<String>,
    /// Read the Markdown body from this file instead of stdin.
    #[arg(long)]
    file: Option<PathBuf>,
}

impl Body {
    fn read(self) -> Result<String> {
        let text = if let Some(text) = self.text {
            text
        } else {
            let reader: Box<dyn Read> = match self.file {
                Some(path) => Box::new(
                    std::fs::File::open(&path)
                        .with_context(|| format!("reading {}", path.display()))?,
                ),
                None => Box::new(io::stdin()),
            };
            let mut text = String::new();
            reader.take(MAX_BODY as u64 + 1).read_to_string(&mut text)?;
            text
        };
        check_body(&text)?;
        Ok(text)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Request {
    pub id: String,
    #[serde(flatten)]
    pub action: Operation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Operation {
    Ready,
    Read {
        message: String,
    },
    Reply {
        to: String,
        body: String,
        progress: bool,
    },
    Ask {
        to: String,
        body: String,
        options: Vec<String>,
    },
}

pub(crate) async fn run(cli: AgentCli) -> Result<()> {
    let path = cli
        .session
        .or_else(|| std::env::var_os("MOBILE_CHAT_SESSION").map(PathBuf::from))
        .context(
            "MOBILE_CHAT_SESSION is missing; run this helper inside a Mobile Chat agent terminal",
        )?;
    let connection: Connection = crate::store::read_json(&path)?;
    let request = match cli.action {
        Action::Ready => Request {
            id: new_id(),
            action: Operation::Ready,
        },
        Action::Read { message } => Request {
            id: new_id(),
            action: Operation::Read { message },
        },
        Action::Reply {
            id,
            to,
            progress,
            body,
        } => Request {
            id,
            action: Operation::Reply {
                to,
                body: body.read()?,
                progress,
            },
        },
        Action::Ask {
            id,
            to,
            options,
            body,
        } => Request {
            id,
            action: Operation::Ask {
                to,
                body: body.read()?,
                options,
            },
        },
    };
    let result = tokio::time::timeout(Duration::from_secs(30), exchange(&connection, &request))
        .await
        .context("Mobile Chat did not respond; retry with the same request ID")??;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

pub(crate) async fn exchange(connection: &Connection, request: &Request) -> Result<Value> {
    if !connection.address.is_ipv4() || !connection.address.ip().is_loopback() {
        bail!("Agent endpoint must use IPv4 loopback.");
    }
    let mut upgrade = format!("ws://{}/agent", connection.address).into_client_request()?;
    let headers = upgrade.headers_mut();
    headers.insert(
        "authorization",
        format!("Bearer {}", connection.token).parse()?,
    );
    headers.insert("x-mobile-chat-owner", connection.owner.parse()?);
    headers.insert(
        "x-mobile-chat-conversation",
        connection.conversation.parse()?,
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(upgrade)
        .await
        .context("connecting to Mobile Chat; the extension may be stopped")?;
    socket
        .send(Message::Text(serde_json::to_string(request)?.into()))
        .await?;
    while let Some(message) = socket.next().await {
        if let Message::Text(text) = message? {
            let reply: Value = serde_json::from_str(&text)?;
            if reply["ok"] != true {
                bail!(
                    "{}",
                    reply["error"]
                        .as_str()
                        .unwrap_or("Mobile Chat rejected the request")
                );
            }
            return Ok(reply["result"].clone());
        }
    }
    bail!("Mobile Chat disconnected before acknowledging the request; retry with the same ID")
}
