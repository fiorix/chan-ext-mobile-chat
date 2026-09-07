//! One retained conversation and its directly spawned terminal agent.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::config::{Agent, Config};
use crate::control::Cs;
use crate::helper::{Connection, Operation, Request};
use crate::model::*;
use crate::store::{Store, atomic_json};

pub(crate) struct Runtime {
    pub active: AtomicBool,
    pub config: Config,
    pub cs: Arc<Cs>,
    pub store: Store,
    pub owner: String,
    pub scope: String,
    pub address: SocketAddr,
    pub executable: PathBuf,
}

impl Runtime {
    pub(crate) fn ensure_active(&self) -> Result<()> {
        if !self.active.load(Ordering::Acquire) {
            bail!("Chan restarted. Reconnect to this workspace.");
        }
        Ok(())
    }
}

pub(crate) struct NewConversation {
    pub id: String,
    pub fingerprint: String,
    pub agent: Agent,
    pub body: String,
    pub window_id: String,
    pub pane_id: Option<String>,
}

pub(crate) struct Chat {
    data: Mutex<Conversation>,
    runtime: Arc<Runtime>,
    updates: broadcast::Sender<()>,
    watcher: Mutex<Option<JoinHandle<()>>>,
    // Serialize terminal writes and stop; this gate carries no conversation data.
    commands: tokio::sync::Mutex<()>,
}

impl Drop for Chat {
    fn drop(&mut self) {
        if let Some(task) = self.watcher.get_mut().expect("watcher lock").take() {
            task.abort();
        }
    }
}

impl Chat {
    pub(crate) fn open(
        mut data: Conversation,
        runtime: Arc<Runtime>,
        updates: broadcast::Sender<()>,
    ) -> Result<Arc<Self>> {
        if !data.run.phase.ended() {
            if data.run.scope != runtime.scope {
                data.stop(
                    Phase::Stopped,
                    "Chan restarted. The conversation is saved; the agent has stopped.",
                );
            } else {
                if data.run.launch == Delivery::Sending {
                    data.run.launch = Delivery::Uncertain;
                }
                if data.run.bootstrap == Delivery::Sending {
                    data.run.bootstrap = Delivery::Uncertain;
                }
                for entry in &mut data.entries {
                    if entry.delivery == Some(Delivery::Sending) {
                        entry.delivery = Some(Delivery::Uncertain);
                        entry.detail =
                            "Delivery was interrupted. Inspect the terminal before sending again."
                                .into();
                    }
                }
            }
            runtime.store.save(&data)?;
        }
        let chat = Arc::new(Self {
            data: Mutex::new(data),
            runtime,
            updates,
            watcher: Mutex::new(None),
            commands: tokio::sync::Mutex::new(()),
        });
        chat.write_descriptor()?;
        Ok(chat)
    }

    pub(crate) fn create(
        spec: NewConversation,
        runtime: Arc<Runtime>,
        updates: broadcast::Sender<()>,
    ) -> Result<Arc<Self>> {
        let NewConversation {
            id,
            fingerprint,
            agent,
            body,
            window_id,
            pane_id,
        } = spec;
        check_body(&body)?;
        if !valid_id(&id) || !valid_id(&window_id) || pane_id.as_ref().is_some_and(|p| !valid_id(p))
        {
            bail!("Invalid conversation or window identity.");
        }
        let run_id = new_id();
        let mut data = Conversation {
            version: 1,
            id: id.clone(),
            title: body
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or(&body)
                .chars()
                .take(72)
                .collect(),
            agent,
            created_at: now(),
            updated_at: now(),
            revision: 1,
            view_revision: 0,
            view: ViewState {
                at_bottom: true,
                ..ViewState::default()
            },
            run: AgentRun {
                id: run_id.clone(),
                token: format!("{}{}", new_id(), new_id()),
                scope: runtime.scope.clone(),
                handle: format!("@@chat-{run_id}"),
                window_id,
                pane_id,
                phase: Phase::Starting,
                detail: String::new(),
                launch: Delivery::Saved,
                bootstrap: Delivery::Saved,
                started_at: now(),
                queue_depth: 0,
                session_id: None,
                accepts_input: false,
            },
            entries: vec![Entry::user(new_id(), body, None)],
            receipts: BTreeMap::new(),
        };
        data.record(id.clone(), fingerprint, json!({"conversation_id": id}));
        runtime.store.save(&data)?;
        Self::open(data, runtime, updates)
    }

    pub(crate) fn data(&self) -> Conversation {
        self.data.lock().expect("conversation lock").clone()
    }

    pub(crate) fn retire(&self) {
        if let Some(task) = self.watcher.lock().expect("watcher lock").take() {
            task.abort();
        }
        // Drain any committed write before a new scope opens the same files.
        drop(self.data.lock().expect("conversation lock"));
    }

    /// Serialize durable changes on a blocking worker, publishing only committed data.
    async fn change<T, F>(self: &Arc<Self>, change: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Conversation) -> Result<T> + Send + 'static,
    {
        let chat = Arc::clone(self);
        let result = tokio::task::spawn_blocking(move || {
            let mut data = chat.data.lock().expect("conversation lock");
            chat.runtime.ensure_active()?;
            let mut next = data.clone();
            let result = change(&mut next)?;
            next.revision += 1;
            next.updated_at = now();
            chat.runtime.store.save(&next)?;
            *data = next;
            Ok::<T, anyhow::Error>(result)
        })
        .await
        .context("saving conversation task")??;
        let _ = self.updates.send(());
        Ok(result)
    }

    fn write_descriptor(&self) -> Result<()> {
        let data = self.data();
        let path = self.runtime.store.descriptor(&data.id)?;
        atomic_json(
            &path,
            &Connection {
                address: self.runtime.address,
                owner: self.runtime.owner.clone(),
                conversation: data.id,
                token: data.run.token,
            },
        )
    }

    pub(crate) fn activate(self: &Arc<Self>) {
        let mut watcher = self.watcher.lock().expect("watcher lock");
        if self.runtime.ensure_active().is_err()
            || watcher.as_ref().is_some_and(|task| !task.is_finished())
            || self.data().run.phase.ended()
        {
            return;
        }
        let weak = Arc::downgrade(self);
        let interval = self.runtime.config.health.poll_interval_secs.clamp(1, 30);
        *watcher = Some(tokio::spawn(async move {
            let mut missing = 0;
            loop {
                let Some(chat) = weak.upgrade() else { return };
                if chat.data().run.phase.ended() {
                    return;
                }
                if let Err(error) = chat.tick(&mut missing).await {
                    let detail = format!("Connection check failed: {error:#}");
                    if chat.data().run.detail != detail {
                        let _ = chat
                            .change(move |data| {
                                data.run.detail = detail;
                                Ok(())
                            })
                            .await;
                    }
                }
                drop(chat);
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }));
    }

    pub(crate) async fn send(
        self: &Arc<Self>,
        id: String,
        fingerprint: String,
        body: String,
        question: Option<String>,
        cancel: bool,
    ) -> Result<Value> {
        check_body(&body)?;
        self.change(move |data| {
            if let Some(result) = data.receipt(&id, &fingerprint)? {
                return Ok(result);
            }
            if data.run.phase.ended() || data.run.phase == Phase::Stopping {
                bail!("This agent has stopped. Start a new conversation to continue working.");
            }
            let message_id = new_id();
            if let Some(question_id) = &question {
                let question = data
                    .entries
                    .iter_mut()
                    .find(|e| &e.id == question_id)
                    .and_then(|entry| entry.question.as_mut())
                    .context("Question does not exist.")?;
                if question.status != QuestionStatus::Pending {
                    bail!("This question is no longer waiting for an answer.");
                }
                question.status = if cancel {
                    QuestionStatus::Cancelled
                } else {
                    QuestionStatus::Answered
                };
                question.answer_id = Some(message_id.clone());
                data.view.answers.remove(question_id);
            }
            data.entries
                .push(Entry::user(message_id.clone(), body, question));
            let result = json!({"message_id": message_id});
            data.record(id, fingerprint, result.clone());
            Ok(result)
        })
        .await
    }

    pub(crate) async fn save_view(
        self: &Arc<Self>,
        revision: u64,
        view: ViewState,
    ) -> Result<Value> {
        view.validate()?;
        self.change(move |data| {
            if data.view_revision != revision {
                if data.view == view { return Ok(json!({"view_revision": data.view_revision})); }
                bail!("This conversation's draft changed in another tab. Your local text has been kept.");
            }
            data.view = view;
            data.view_revision += 1;
            Ok(json!({"view_revision": data.view_revision}))
        }).await
    }

    pub(crate) async fn agent(self: &Arc<Self>, token: String, request: Request) -> Result<Value> {
        let fingerprint = digest(&serde_json::to_vec(&request.action)?);
        self.change(move |data| {
            if data.run.token != token || token.is_empty() || data.run.phase.ended()
                || data.run.phase == Phase::Stopping
            { bail!("This agent run is no longer active."); }
            if let Some(result) = data.receipt(&request.id, &fingerprint)? { return Ok(result); }
            let retain_receipt = !matches!(&request.action, Operation::Read { .. } | Operation::Ready);
            let result = match request.action {
                Operation::Ready => {
                    data.run.bootstrap = Delivery::Read;
                    data.run.accepts_input = true;
                    data.run.phase = if data.pending_questions() > 0 { Phase::Waiting } else { Phase::Ready };
                    data.run.detail.clear();
                    json!({"ready": true, "instruction": "User message references arrive through the terminal queue. Read each with the helper; post all replies and questions through the helper."})
                }
                Operation::Read { message } => {
                    data.run.accepts_input = false;
                    let entry = data.entries.iter_mut().find(|e| e.id == message && e.role == "user")
                        .context("Queued message does not exist.")?;
                    let already_read = entry.delivery == Some(Delivery::Read);
                    entry.delivery = Some(Delivery::Read);
                    entry.detail.clear();
                    let result = json!({"id": entry.id, "body": entry.body, "question_id": entry.reply_to,
                        "already_read": already_read});
                    data.run.phase = Phase::Working;
                    data.run.detail.clear();
                    result
                }
                Operation::Reply { to, body, progress } => {
                    validate_reply(data, &to, &body)?;
                    let entry_id = new_id();
                    data.entries.push(Entry {
                        id: entry_id.clone(), role: "assistant".into(),
                        kind: if progress { "progress" } else { "message" }.into(), body,
                        created_at: now(), reply_to: Some(to), delivery: None,
                        detail: String::new(), question: None,
                    });
                    data.run.phase = if progress { Phase::Working }
                        else if data.pending_questions() > 0 { Phase::Waiting } else { Phase::Ready };
                    data.run.accepts_input = !progress;
                    data.run.detail.clear();
                    json!({"message_id": entry_id})
                }
                Operation::Ask { to, body, options } => {
                    data.run.accepts_input = false;
                    validate_reply(data, &to, &body)?;
                    if options.len() > 8 || options.iter().any(|o| o.trim().is_empty() || o.len() > 128)
                        || options.iter().collect::<std::collections::BTreeSet<_>>().len() != options.len()
                    { bail!("Use up to eight distinct, nonempty choices of at most 128 bytes."); }
                    let entry_id = new_id();
                    data.entries.push(Entry {
                        id: entry_id.clone(), role: "assistant".into(), kind: "question".into(),
                        body, created_at: now(), reply_to: Some(to), delivery: None,
                        detail: String::new(),
                        question: Some(Question { options, status: QuestionStatus::Pending, answer_id: None }),
                    });
                    data.run.phase = Phase::Waiting;
                    data.run.detail.clear();
                    json!({"question_id": entry_id,
                        "instruction": "The question is saved. Finish independent work if available, then run the helper's agent ready command and end this turn. Ready allows the queued answer to arrive. Do not poll, sleep, or open a terminal survey."})
                }
            };
            // Read and Ready are intrinsically idempotent and need no retained receipt.
            if retain_receipt {
                data.record(request.id, fingerprint, result.clone());
            }
            Ok(result)
        }).await
    }

    pub(crate) async fn stop(self: &Arc<Self>, id: String, fingerprint: String) -> Result<Value> {
        let _command = self.commands.lock().await;
        let data = self.data();
        if let Some(result) = data.receipt(&id, &fingerprint)? {
            return Ok(result);
        }
        if data.run.phase.ended() {
            return Ok(json!({"stopped": true}));
        }
        self.change(|data| {
            data.run.phase = Phase::Stopping;
            Ok(())
        })
        .await?;
        let out = self
            .runtime
            .cs
            .run(
                &data.run.window_id,
                ["terminal", "close", "--tab-name", &data.run.handle],
            )
            .await;
        let success = match &out {
            Ok(out) => out.ok() || out.message().contains("no live terminal session matched"),
            Err(_) => false,
        };
        if !success {
            let detail = match out {
                Ok(out) => out.message(),
                Err(error) => format!("{error:#}"),
            };
            self.change(move |data| {
                data.run.detail =
                    format!("Stop could not be confirmed: {detail}. Use Peek or retry Stop agent.");
                Ok(())
            })
            .await?;
            bail!(
                "Stop could not be confirmed. The conversation is saved; inspect the terminal or retry."
            );
        }
        self.change(move |data| {
            data.stop(Phase::Stopped, "Agent stopped. Conversation saved.");
            let result = json!({"stopped": true});
            data.record(id, fingerprint, result.clone());
            Ok(result)
        })
        .await
    }

    pub(crate) async fn peek(&self) -> Result<Value> {
        let data = self.data();
        let listed = self
            .runtime
            .cs
            .run(&data.run.window_id, ["terminal", "list", "--json"])
            .await?;
        if !listed.ok() {
            bail!("{}", listed.message());
        }
        let entry = find_terminal(&serde_json::from_str(&listed.stdout)?, &data.run.handle)
            .context("The agent terminal is no longer available.")?;
        let window = entry["window"].as_str().unwrap_or(&data.run.window_id);
        let pane = entry["pane"]
            .as_str()
            .or(data.run.pane_id.as_deref())
            .context("The agent's pane could not be identified.")?;
        let side = entry["side"].as_str().unwrap_or("b");
        let out = self
            .runtime
            .cs
            .run(window, ["pane", "focus", pane, "--side", side])
            .await?;
        if !out.ok() {
            bail!("{}", out.message());
        }
        Ok(json!({"window_id": window, "pane_id": pane, "tab_id": entry["tab"]}))
    }

    async fn tick(self: &Arc<Self>, missing: &mut u32) -> Result<()> {
        let _command = self.commands.lock().await;
        let data = self.data();
        if data.run.phase.ended() || data.run.phase == Phase::Stopping {
            return Ok(());
        }
        if data.run.launch == Delivery::Saved {
            self.launch().await?;
            return Ok(());
        }
        let out = self
            .runtime
            .cs
            .run(&data.run.window_id, ["terminal", "list", "--json"])
            .await?;
        if !out.ok() {
            bail!("{}", out.message());
        }
        let listed: Value =
            serde_json::from_str(&out.stdout).context("reading terminal inventory")?;
        let Some(entry) = find_terminal(&listed, &data.run.handle) else {
            *missing += 1;
            if (data.run.session_id.is_some() && *missing >= 3)
                || (data.run.session_id.is_none()
                    && now().saturating_sub(data.run.started_at)
                        > self.runtime.config.health.boot_timeout_secs)
            {
                self.change(|data| {
                    data.stop(
                        Phase::Stopped,
                        "Agent terminal is unavailable. Conversation saved.",
                    );
                    Ok(())
                })
                .await?;
            }
            return Ok(());
        };
        *missing = 0;
        let session_id = entry["session_id"].as_str().map(str::to_owned);
        let depth = entry["queue_depth"].as_u64().unwrap_or(0);
        if data.run.session_id.is_some()
            && session_id.is_some()
            && data.run.session_id != session_id
        {
            self.change(|data| {
                data.stop(
                    Phase::Stopped,
                    "Agent terminal was replaced. Start a new conversation.",
                );
                Ok(())
            })
            .await?;
            return Ok(());
        }
        if data.run.session_id != session_id || data.run.queue_depth != depth {
            self.change(move |data| {
                data.run.session_id = session_id;
                data.run.queue_depth = depth;
                Ok(())
            })
            .await?;
        }
        if depth > 0
            && data.run.detail.is_empty()
            && data.entries.iter().any(|entry| {
                entry.delivery == Some(Delivery::Queued)
                    && now().saturating_sub(entry.created_at)
                        > self.runtime.config.health.stall_after_secs
            })
        {
            self.change(|data| {
                data.run.detail =
                    "A message is still queued. Peek can show native prompts or a busy agent."
                        .into();
                Ok(())
            })
            .await?;
        }
        if data.run.bootstrap == Delivery::Read {
            self.deliver().await?;
        } else if now().saturating_sub(data.run.started_at)
            > self.runtime.config.health.boot_timeout_secs
            && data.run.detail.is_empty()
        {
            self.change(|data| {
                data.run.detail = "Waiting for the agent to connect to chat. Use Peek for startup or permission prompts.".into(); Ok(())
            }).await?;
        }
        Ok(())
    }

    async fn launch(self: &Arc<Self>) -> Result<()> {
        let data = self.data();
        if let Err(error) = preflight(&data.agent.command).await {
            let detail = format!("{error:#}");
            self.change(move |data| {
                data.stop(Phase::Failed, detail);
                Ok(())
            })
            .await?;
            return Ok(());
        }
        let pane = match self
            .runtime
            .cs
            .pane(&data.run.window_id, data.run.pane_id.as_deref())
            .await
        {
            Ok(pane) => pane,
            Err(error) => {
                let detail = format!("Could not locate this chat's pane: {error:#}");
                self.change(move |data| {
                    data.stop(Phase::Failed, detail);
                    Ok(())
                })
                .await?;
                return Ok(());
            }
        };
        let chosen = pane.clone();
        self.change(move |data| {
            data.run.launch = Delivery::Sending;
            if data.agent.prompt_argument {
                data.run.bootstrap = Delivery::Sending;
            }
            data.run.pane_id = Some(chosen);
            Ok(())
        })
        .await?;
        let descriptor = self.runtime.store.descriptor(&data.id)?;
        let args = spawn_args(&data, &pane, &self.runtime.executable, &descriptor);
        let result = self.runtime.cs.run(&data.run.window_id, args).await;
        // Terminal creation selects side B. Return focus to the chat after
        // Chan acknowledges creation; the agent stays mounted on the back.
        if result.as_ref().is_ok_and(|out| out.ok()) {
            let focused = self
                .runtime
                .cs
                .run(&data.run.window_id, ["pane", "focus", &pane, "--side", "a"])
                .await;
            if let Err(error) = focused {
                eprintln!("mobile-chat: could not return to chat: {error:#}");
            }
        }
        self.change(move |data| {
            if data.run.phase == Phase::Starting { data.run.phase = Phase::Connecting; }
            match result {
                Ok(out) if out.ok() => {
                    data.run.launch = Delivery::Queued;
                    if data.run.bootstrap == Delivery::Sending { data.run.bootstrap = Delivery::Queued; }
                    if !data.agent.prompt_argument {
                        data.run.detail = "Finish startup through Peek if needed. Once the agent's normal input is ready, choose Connect chat.".into();
                    }
                }
                Ok(out) => {
                    data.run.launch = Delivery::Failed;
                    data.stop(Phase::Failed, format!("Could not start agent: {}. Direct terminal --command and --env support is required.", out.message()));
                }
                Err(error) => {
                    data.run.launch = Delivery::Uncertain;
                    if data.run.bootstrap == Delivery::Sending { data.run.bootstrap = Delivery::Uncertain; }
                    data.run.detail = format!("Launch acknowledgment was lost: {error:#}. Checking for the terminal; not launching it again.");
                }
            }
            Ok(())
        }).await
    }

    pub(crate) async fn connect_agent(self: &Arc<Self>) -> Result<Value> {
        let _command = self.commands.lock().await;
        let data = self.data();
        if data.run.phase.ended() || data.run.phase == Phase::Stopping {
            bail!("This agent has stopped.");
        }
        if data.run.bootstrap != Delivery::Saved {
            return Ok(json!({"connecting": true}));
        }
        if data.run.session_id.is_none() {
            bail!("The terminal is still starting.");
        }
        self.bootstrap().await?;
        Ok(json!({"connecting": true}))
    }

    async fn bootstrap(self: &Arc<Self>) -> Result<()> {
        let data = self.data();
        self.change(|data| {
            data.run.bootstrap = Delivery::Sending;
            Ok(())
        })
        .await?;
        let result = self
            .runtime
            .cs
            .write(
                &data.run.window_id,
                &data.run.handle,
                &data.agent.submit_chord,
                &brief(),
            )
            .await;
        self.change(move |data| {
            // A fast helper acknowledgment can arrive before cs returns.
            if data.run.bootstrap == Delivery::Read { return Ok(()); }
            match result {
                Ok(()) => data.run.bootstrap = Delivery::Queued,
                Err(error) => {
                    data.run.bootstrap = Delivery::Uncertain;
                    data.run.detail = format!("Setup delivery could not be confirmed: {error:#}. Use Peek to inspect the agent.");
                }
            }
            Ok(())
        }).await
    }

    async fn deliver(self: &Arc<Self>) -> Result<()> {
        let data = self.data();
        if !data.run.accepts_input {
            return Ok(());
        }
        let pending: Vec<String> = data
            .entries
            .iter()
            .filter(|entry| entry.delivery == Some(Delivery::Saved))
            .map(|entry| entry.id.clone())
            .take(1)
            .collect();
        for id in pending {
            if self.data().run.phase.ended() {
                break;
            }
            let sending = id.clone();
            self.change(move |data| {
                data.run.accepts_input = false;
                if let Some(entry) = data.entries.iter_mut().find(|e| e.id == sending) {
                    entry.delivery = Some(Delivery::Sending);
                }
                Ok(())
            })
            .await?;
            let result = self
                .runtime
                .cs
                .write(
                    &data.run.window_id,
                    &data.run.handle,
                    &data.agent.submit_chord,
                    &message_prompt(&id),
                )
                .await;
            self.change(move |data| {
                let entry = data.entries.iter_mut().find(|e| e.id == id).context("message disappeared")?;
                if entry.delivery == Some(Delivery::Read) { return Ok(()); }
                match result {
                    Ok(()) => entry.delivery = Some(Delivery::Queued),
                    Err(error) => {
                        entry.delivery = Some(Delivery::Uncertain);
                        entry.detail = format!("Delivery could not be confirmed: {error:#}. Inspect the agent before sending again.");
                    }
                }
                Ok(())
            }).await?;
        }
        Ok(())
    }
}

fn validate_reply(data: &Conversation, to: &str, body: &str) -> Result<()> {
    check_body(body)?;
    if !data
        .entries
        .iter()
        .any(|e| e.id == to && e.role == "user" && e.delivery == Some(Delivery::Read))
    {
        bail!("Read the queued user message with the helper before replying to it.");
    }
    Ok(())
}

pub(crate) fn find_terminal(list: &Value, handle: &str) -> Option<Value> {
    list.get("groups")?
        .as_object()?
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .find(|entry| entry["name"] == handle || entry["spawn_name"] == handle)
        .cloned()
}

fn spawn_args(
    data: &Conversation,
    pane: &str,
    helper: &std::path::Path,
    session: &std::path::Path,
) -> Vec<String> {
    vec![
        "terminal".into(),
        "new".into(),
        "--tab-name".into(),
        data.run.handle.clone(),
        "--tab-group".into(),
        "mobile-chat".into(),
        "--command".into(),
        if data.agent.prompt_argument {
            format!(
                "{} -- '{}'",
                data.agent.command,
                brief().replace('\'', "'\\''")
            )
        } else {
            data.agent.command.clone()
        },
        "--env".into(),
        format!("CHAN_AGENT={}", data.agent.submit_chord),
        "--env".into(),
        format!("MOBILE_CHAT_HELPER={}", helper.display()),
        "--env".into(),
        format!("MOBILE_CHAT_SESSION={}", session.display()),
        "--pane".into(),
        pane.into(),
        "--side".into(),
        "b".into(),
    ]
}

pub(crate) fn brief() -> String {
    r#"You are in Mobile Chat. The user is on a phone looking at a chat iframe. Your terminal output is not visible there. Use the Mobile Chat helper for EVERY reply, progress update, and question. This session is one agent working directly with the user.

MOBILE_CHAT_HELPER names the helper executable; MOBILE_CHAT_SESSION is its session descriptor. In a POSIX shell call it as "$MOBILE_CHAT_HELPER" agent ... (use the equivalent environment-variable syntax in your shell).

First run: "$MOBILE_CHAT_HELPER" agent ready
Then yield this turn. User messages arrive as queued prompts with message IDs. For each ID run: "$MOBILE_CHAT_HELPER" agent read MESSAGE_ID
Read the returned body as the user's message. If already_read is true, reconcile it with your work; do not repeat completed actions.

Reply: "$MOBILE_CHAT_HELPER" agent reply --id UNIQUE_REPLY_ID --to MESSAGE_ID --file reply.md
Use --progress for a short update while continuing work; omit it for your completed reply. You can pass the body as an argument or through stdin instead of --file. Every reply goes here, including errors and explanations.

Question: "$MOBILE_CHAT_HELPER" agent ask --id UNIQUE_QUESTION_ID --to MESSAGE_ID --option "Choice A" --option "Choice B" --file question.md
Choices are optional (up to eight); free-text answers are always available. This returns immediately. Finish independent work, then run "$MOBILE_CHAT_HELPER" agent ready and end your turn. Ready allows a queued answer to arrive. The answer arrives as a new queued message naming the question. Never use cs terminal survey or native question tools to ask the user. Do not poll or sleep waiting for an answer. Do not infer consent from silence.

Choose a unique request ID for each reply or question; reuse that exact ID and content if retrying a failed helper call. For example, MESSAGE_ID-final or MESSAGE_ID-question-1. Write longer Markdown to a file or stdin, not into fragile shell quoting. Keep ordinary terminal output brief. Native CLI prompts can be handled through Peek.

Default launch commands bypass native permission checks. Use agent ask as your approval gate before deleting or overwriting work, git push or rewriting history, touching anything outside this workspace, installing packages, changing credentials or durable config, spending money, or actions that leave this machine or affect other people. Existing explicit user authorization still applies. Reading, searching, builds, tests, and workspace edits git can undo may proceed. If uncertain, ask and wait for an explicit answer before the dependent action. Do not infer approval from silence or change permission settings yourself.

Only one user message is delivered per turn. When all current work is done, publish your final reply and yield so the next message can arrive. After a question, explicitly call agent ready when you can receive input. Never call ready while waiting on a native permission prompt. Do not perform project work until you have read a user message through the helper."#.into()
}

fn message_prompt(id: &str) -> String {
    format!(
        "Mobile Chat message {id}. Run \"$MOBILE_CHAT_HELPER\" agent read {id} to read and acknowledge it (use your shell's environment-variable syntax). Reply through the helper with --to {id}; use agent ask for questions. Read all queued IDs before acting. Native terminal output does not reach the chat. Do not repeat completed work if already_read is true."
    )
}

async fn preflight(command: &str) -> Result<()> {
    if cfg!(windows)
        || command.contains(['|', '&', ';', '<', '>', '(', ')', '`', '$', '\'', '"', '\\'])
    {
        return Ok(());
    }
    let Some(program) = command.split_whitespace().next() else {
        bail!("Agent command is empty.");
    };
    if program.contains('=') {
        return Ok(());
    }
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let mut process = tokio::process::Command::new(&shell);
    process
        .args(["-lc", &format!("command -v -- {program}")])
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), process.output())
        .await
        .context("login shell did not respond")??;
    if !output.status.success() {
        bail!(
            "{program:?} is not on the login shell's PATH. Set an absolute command in mobile-chat.toml."
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
