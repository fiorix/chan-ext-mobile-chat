//! Outbound routing: the per-conversation forwarder, entry diffing, and
//! question rendering.
//!
//! One forwarder task per bound conversation subscribes to the tenant's
//! `updates` broadcast, re-reads the conversation, and forwards assistant
//! entries after `last_forwarded_entry`, which persists in `settings.json` so
//! a restart does not replay the transcript. A `Lagged` broadcast is safe: the
//! forwarder always diffs by entry id, so a lag only costs a redundant
//! re-read. When the tenant is not loaded there is no channel to subscribe
//! to; the forwarder polls until one appears.
//!
//! Question rendering (`whatsapp_text` for the body):
//!
//! ```text
//! *Question from claude*
//! Where should we run this?
//!
//! 1. Local
//! 2. Remote
//!
//! Reply /agent 1, or /agent <your answer>, or /agent cancel
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use crate::client::{ChatId, WaSink};
use crate::settings::SettingsStore;
use crate::store::{ChatStore, LogRecord};
use crate::whatsapp_text::{self, CHUNK_MAX_CHARS};
use crate::{EntryView, Host, Resolved};

/// How long an unsubscribed forwarder waits before re-trying
/// [`Host::subscribe`] and diffing again.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// One conversation's outbound pipeline: diff assistant entries after
/// `last_forwarded_entry`, render and chunk them, send through the sink, log
/// each sent chunk as bridge traffic, and persist the new watermark. Stateless
/// between calls, so a full re-read after a `Lagged` broadcast loses nothing.
/// Generic over the sink because [`WaSink`]'s `impl Future` methods are not
/// object-safe; the host is `Arc<dyn Host>`.
pub struct Forwarder<S: WaSink> {
    settings: Arc<Mutex<SettingsStore>>,
    chats: Arc<Mutex<ChatStore>>,
    sink: Arc<S>,
    host: Arc<dyn Host>,
    reply_progress: bool,
}

impl<S: WaSink> Forwarder<S> {
    pub fn new(
        settings: Arc<Mutex<SettingsStore>>,
        chats: Arc<Mutex<ChatStore>>,
        sink: Arc<S>,
        host: Arc<dyn Host>,
        reply_progress: bool,
    ) -> Self {
        Forwarder {
            settings,
            chats,
            sink,
            host,
            reply_progress,
        }
    }

    /// Diff and forward one batch for a bound chat; returns how many entries
    /// were sent. `NotLoaded`/`Stopped`/`Missing` conversations forward
    /// nothing but are not an error: the watermark only advances over entries
    /// the host actually showed us.
    pub async fn forward_once(&self, jid: &str, owner: &str, conversation: &str) -> Result<usize> {
        let last = self
            .settings
            .lock()
            .await
            .chat(jid)
            .and_then(|chat| chat.last_forwarded_entry);
        // Logging sends requires the chat directory; it is created by the
        // inbound side on first contact, so reopen it when it already exists.
        {
            let mut settings = self.settings.lock().await;
            let mut chats = self.chats.lock().await;
            chats.reopen(&mut settings, jid)?;
        }
        let Resolved::Found(view) = self.host.resolve(owner, conversation).await else {
            return Ok(0);
        };

        let watermark = last.as_deref();
        let mut to_forward: Vec<&EntryView> = Vec::new();
        let mut new_watermark: Option<String> = None;
        let mut past = watermark.is_none();
        for entry in &view.entries {
            if entry.role != "assistant" {
                continue;
            }
            if !past {
                if Some(entry.id.as_str()) == watermark {
                    // The watermark entry itself is not re-forwarded.
                    past = true;
                }
                continue;
            }
            new_watermark = Some(entry.id.clone());
            if forwardable(entry, self.reply_progress) {
                to_forward.push(entry);
            }
        }

        let mut forwarded = 0;
        for entry in to_forward {
            self.forward_entry(jid, conversation, entry).await?;
            forwarded += 1;
        }

        if new_watermark.is_some() && new_watermark != last {
            self.settings
                .lock()
                .await
                .set_last_forwarded_entry(jid, new_watermark.as_deref().unwrap_or_default())?;
        }
        Ok(forwarded)
    }

    /// Render one entry, chunk it, send each chunk, and log every sent chunk
    /// as bridge traffic with the conversation and entry ids.
    async fn forward_entry(&self, jid: &str, conversation: &str, entry: &EntryView) -> Result<()> {
        let rendered = match entry.kind.as_str() {
            "question" => render_question(entry),
            _ => whatsapp_text::render(&entry.body),
        };
        for chunk in whatsapp_text::chunk(&rendered, CHUNK_MAX_CHARS) {
            let id = self
                .sink
                .send_text(&ChatId(jid.to_string()), &chunk)
                .await?;
            self.log_sent(jid, conversation, &entry.id, &id, &chunk)
                .await;
        }
        Ok(())
    }

    async fn log_sent(
        &self,
        jid: &str,
        conversation: &str,
        entry_id: &str,
        message_id: &str,
        text: &str,
    ) {
        let now = chrono::Utc::now();
        let record = LogRecord {
            ts: now.to_rfc3339(),
            ts_unix: now.timestamp().max(0) as u64,
            id: message_id.to_string(),
            chat: jid.to_string(),
            sender: jid.to_string(),
            sender_lid: None,
            push_name: String::new(),
            from_me: true,
            kind: "text".to_string(),
            text: Some(text.to_string()),
            media: None,
            quoted_id: None,
            origin: Some("agent".to_string()),
            conversation: Some(conversation.to_string()),
            entry: Some(entry_id.to_string()),
        };
        let _ = self.chats.lock().await.append(jid, &record);
    }

    /// The per-conversation task: catch up once, then diff on every
    /// notification until the binding disappears and the manager aborts the
    /// task. Runs until the broadcast closes and no re-subscription succeeds.
    pub async fn run(
        self: Arc<Self>,
        jid: String,
        owner: String,
        conversation: String,
        mut updates: Option<broadcast::Receiver<()>>,
    ) {
        // Catch up before subscribing's first wait: entries that landed while
        // no forwarder was running are forwarded on acquisition.
        let _ = self.forward_once(&jid, &owner, &conversation).await;
        loop {
            match &mut updates {
                Some(receiver) => match receiver.recv().await {
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => updates = None,
                },
                None => {
                    tokio::time::sleep(RETRY_INTERVAL).await;
                    updates = self.host.subscribe(&owner).await;
                }
            }
            let _ = self.forward_once(&jid, &owner, &conversation).await;
        }
    }
}

/// Whether an assistant entry goes out: `message` always, `progress` only
/// when configured, `question` always; anything else is skipped (but still
/// advances the watermark).
fn forwardable(entry: &EntryView, reply_progress: bool) -> bool {
    match entry.kind.as_str() {
        "message" | "question" => true,
        "progress" => reply_progress,
        _ => false,
    }
}

/// The question text, following the plan's shape exactly:
///
/// ```text
/// *Question from claude*
/// Where should we run this?
///
/// 1. Local
/// 2. Remote
///
/// Reply /agent 1, or /agent <your answer>, or /agent cancel
/// ```
pub fn render_question(entry: &EntryView) -> String {
    let mut out = format!("*Question from {}*\n", entry.agent_name);
    out.push_str(&whatsapp_text::render(&entry.body));
    if let Some(question) = &entry.question
        && !question.options.is_empty()
    {
        out.push_str("\n\n");
        for (index, option) in question.options.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", index + 1, option));
        }
        out.pop();
    }
    out.push_str("\n\nReply /agent 1, or /agent <your answer>, or /agent cancel");
    out
}

/// Owns the one-task-per-binding forwarders: reconciles the running set
/// against `settings.json` on every [`ForwarderSet::sync`], so a newly bound
/// chat acquires a forwarder and an unbound or re-bound chat loses or
/// replaces its task. [`ForwarderSet::supervise`] runs that reconciliation on
/// an interval for the process lifetime.
pub struct ForwarderSet<S: WaSink> {
    settings: Arc<Mutex<SettingsStore>>,
    chats: Arc<Mutex<ChatStore>>,
    sink: Arc<S>,
    host: Arc<dyn Host>,
    reply_progress: bool,
    running: HashMap<String, RunningForwarder>,
}

struct RunningForwarder {
    owner: String,
    conversation: String,
    task: JoinHandle<()>,
}

impl<S: WaSink + 'static> ForwarderSet<S> {
    pub fn new(
        settings: Arc<Mutex<SettingsStore>>,
        chats: Arc<Mutex<ChatStore>>,
        sink: Arc<S>,
        host: Arc<dyn Host>,
        reply_progress: bool,
    ) -> Self {
        ForwarderSet {
            settings,
            chats,
            sink,
            host,
            reply_progress,
            running: HashMap::new(),
        }
    }

    /// How many forwarder tasks are running; mostly for tests.
    pub fn running_len(&self) -> usize {
        self.running.len()
    }

    /// Reconcile running forwarders with the current bindings.
    pub async fn sync(&mut self) -> Result<()> {
        let bindings: HashMap<String, (String, String)> = self
            .settings
            .lock()
            .await
            .get()
            .chats
            .iter()
            .filter_map(|(jid, chat)| {
                Some((
                    jid.clone(),
                    (chat.owner.clone()?, chat.conversation.clone()?),
                ))
            })
            .collect();

        self.running.retain(|jid, running| {
            let keep = bindings.get(jid).is_some_and(|(owner, conversation)| {
                running.owner == *owner && running.conversation == *conversation
            }) && !running.task.is_finished();
            if !keep {
                running.task.abort();
            }
            keep
        });

        for (jid, (owner, conversation)) in bindings {
            let alive = self.running.get(&jid).is_some_and(|running| {
                running.owner == owner
                    && running.conversation == conversation
                    && !running.task.is_finished()
            });
            if alive {
                continue;
            }
            if let Some(old) = self.running.remove(&jid) {
                old.task.abort();
            }
            let forwarder = Arc::new(Forwarder::new(
                self.settings.clone(),
                self.chats.clone(),
                self.sink.clone(),
                self.host.clone(),
                self.reply_progress,
            ));
            let updates = self.host.subscribe(&owner).await;
            let task_jid = jid.clone();
            let task_owner = owner.clone();
            let task_conversation = conversation.clone();
            let task = tokio::spawn(async move {
                forwarder
                    .run(task_jid, task_owner, task_conversation, updates)
                    .await;
            });
            self.running.insert(
                jid,
                RunningForwarder {
                    owner,
                    conversation,
                    task,
                },
            );
        }
        Ok(())
    }

    /// Reconcile forever, every `interval`; spawned by the bridge at startup.
    pub async fn supervise(mut self, interval: Duration) {
        loop {
            let _ = self.sync().await;
            tokio::time::sleep(interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MediaRef;
    use crate::settings::Settings;
    use crate::{ConversationView, EntryView, QuestionStatus};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;

    const JID: &str = "5511987654321@s.whatsapp.net";
    const OWNER: &str = "owner:ws";
    const CONV: &str = "conv1";

    #[derive(Default)]
    struct RecordingSink {
        sent: StdMutex<Vec<(String, String)>>,
    }

    impl RecordingSink {
        fn sent(&self) -> Vec<(String, String)> {
            self.sent.lock().unwrap().clone()
        }

        fn bodies(&self) -> Vec<String> {
            self.sent().into_iter().map(|(_, text)| text).collect()
        }
    }

    impl crate::client::WaSink for RecordingSink {
        async fn send_text(&self, chat: &crate::client::ChatId, text: &str) -> Result<String> {
            let mut sent = self.sent.lock().unwrap();
            let id = format!("wa-msg-{}", sent.len());
            sent.push((chat.0.clone(), text.to_string()));
            Ok(id)
        }

        async fn download(&self, _media: &MediaRef, _to: &Path) -> Result<u64> {
            Ok(0)
        }
    }

    struct FakeConversation {
        view: ConversationView,
        updates: broadcast::Sender<()>,
    }

    /// The host side of a bound conversation: a mutable in-memory view plus
    /// its notification channel.
    #[derive(Default)]
    struct FakeHost {
        conversations: StdMutex<HashMap<(String, String), FakeConversation>>,
    }

    impl FakeHost {
        fn bind(&self, view: ConversationView) {
            self.conversations.lock().unwrap().insert(
                (OWNER.to_string(), CONV.to_string()),
                FakeConversation {
                    view,
                    updates: broadcast::channel(32).0,
                },
            );
        }

        fn push_entry(&self, entry: EntryView) {
            let mut conversations = self.conversations.lock().unwrap();
            let conversation = conversations
                .get_mut(&(OWNER.to_string(), CONV.to_string()))
                .expect("bound");
            conversation.view.entries.push(entry);
        }

        fn notify(&self) {
            let conversations = self.conversations.lock().unwrap();
            let conversation = conversations
                .get(&(OWNER.to_string(), CONV.to_string()))
                .expect("bound");
            let _ = conversation.updates.send(());
        }

        fn subscribe(&self) -> broadcast::Receiver<()> {
            let conversations = self.conversations.lock().unwrap();
            let conversation = conversations
                .get(&(OWNER.to_string(), CONV.to_string()))
                .expect("bound");
            conversation.updates.subscribe()
        }
    }

    impl Host for FakeHost {
        fn resolve<'a>(
            &'a self,
            owner: &'a str,
            conversation: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Resolved> + Send + 'a>> {
            Box::pin(async move {
                match self
                    .conversations
                    .lock()
                    .unwrap()
                    .get(&(owner.into(), conversation.into()))
                {
                    Some(found) => Resolved::Found(found.view.clone()),
                    None => Resolved::NotLoaded,
                }
            })
        }

        fn send<'a>(
            &'a self,
            _owner: &'a str,
            _conversation: &'a str,
            _send: crate::SendRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn subscribe<'a>(
            &'a self,
            owner: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<broadcast::Receiver<()>>> + Send + 'a>,
        > {
            Box::pin(async move {
                let conversations = self.conversations.lock().unwrap();
                conversations
                    .iter()
                    .find(|(key, _)| key.0 == owner)
                    .map(|(_, found)| found.updates.subscribe())
            })
        }
    }

    struct Fixture {
        settings: Arc<Mutex<SettingsStore>>,
        settings_path: std::path::PathBuf,
        chats: Arc<Mutex<ChatStore>>,
        sink: Arc<RecordingSink>,
        host: Arc<FakeHost>,
        root: tempfile::TempDir,
        reply_progress: bool,
    }

    fn fixture(reply_progress: bool) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let store_root = root.path().join("whatsapp");
        let settings_path = store_root.join("settings.json");
        let mut settings = Settings {
            version: 1,
            ..Settings::default()
        };
        let chat = settings.chats.entry(JID.to_string()).or_default();
        chat.record = true;
        chat.owner = Some(OWNER.to_string());
        chat.conversation = Some(CONV.to_string());
        std::fs::create_dir_all(&store_root).unwrap();
        std::fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
        let mut chats = ChatStore::new(&store_root, 1024 * 1024, 10).unwrap();
        let mut settings_store = SettingsStore::new(settings_path.clone()).unwrap();
        // The chat directory exists, as a prior inbound created it.
        chats
            .ensure_chat(&mut settings_store, JID, "dm", "Alex", 1_700_000_000)
            .unwrap();
        Fixture {
            settings: Arc::new(Mutex::new(settings_store)),
            settings_path,
            chats: Arc::new(Mutex::new(chats)),
            sink: Arc::new(RecordingSink::default()),
            host: Arc::new(FakeHost::default()),
            root,
            reply_progress,
        }
    }

    impl Fixture {
        fn forwarder(&self) -> Forwarder<RecordingSink> {
            Forwarder::new(
                self.settings.clone(),
                self.chats.clone(),
                self.sink.clone(),
                self.host.clone(),
                self.reply_progress,
            )
        }

        fn last_forwarded(&self) -> Option<String> {
            let settings: Settings =
                serde_json::from_slice(&std::fs::read(&self.settings_path).unwrap()).unwrap();
            settings.chats[JID].last_forwarded_entry.clone()
        }

        /// The bridge-traffic log records (from_me with origin "agent").
        fn sent_records(&self) -> Vec<crate::store::LogRecord> {
            let name = {
                let settings: Settings =
                    serde_json::from_slice(&std::fs::read(&self.settings_path).unwrap()).unwrap();
                settings.chats[JID]
                    .dir
                    .clone()
                    .expect("chat directory indexed")
            };
            // ensure_chat ran at first forward (log append requires it).
            let log = self
                .root
                .path()
                .join("whatsapp/chats")
                .join(name)
                .join("log.jsonl");
            let Ok(contents) = std::fs::read_to_string(log) else {
                return Vec::new();
            };
            contents
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .filter(|record: &crate::store::LogRecord| record.from_me)
                .collect()
        }
    }

    fn entry(id: &str, kind: &str, body: &str) -> EntryView {
        EntryView {
            id: id.to_string(),
            kind: kind.to_string(),
            body: body.to_string(),
            role: "assistant".to_string(),
            agent_name: "claude".to_string(),
            question: None,
        }
    }

    fn question(id: &str, body: &str, options: &[&str]) -> EntryView {
        EntryView {
            id: id.to_string(),
            kind: "question".to_string(),
            body: body.to_string(),
            role: "assistant".to_string(),
            agent_name: "claude".to_string(),
            question: Some(crate::QuestionView {
                options: options.iter().map(ToString::to_string).collect(),
                status: QuestionStatus::Pending,
            }),
        }
    }

    fn transcript() -> ConversationView {
        ConversationView {
            title: "Deploy".to_string(),
            phase: "Running".to_string(),
            queue_depth: 0,
            pending_questions: 1,
            entries: vec![
                EntryView {
                    role: "user".to_string(),
                    ..entry("u1", "message", "ignored: a user entry")
                },
                entry("m1", "message", "The build is **green**."),
                entry("p1", "progress", "Still testing..."),
                question("q1", "Where should we run this?", &["Local", "Remote"]),
            ],
        }
    }

    #[test]
    fn question_rendering_matches_the_plan_shape() {
        let rendered = render_question(&question(
            "q1",
            "Where should we run this?",
            &["Local", "Remote"],
        ));
        assert_eq!(
            rendered,
            "*Question from claude*\n\
            Where should we run this?\n\n\
            1. Local\n\
            2. Remote\n\n\
            Reply /agent 1, or /agent <your answer>, or /agent cancel"
        );
    }

    #[tokio::test]
    async fn forwards_message_progress_and_question_and_logs_sends() {
        let fixture = fixture(true);
        fixture.host.bind(transcript());
        let forwarded = fixture
            .forwarder()
            .forward_once(JID, OWNER, CONV)
            .await
            .unwrap();
        assert_eq!(forwarded, 3, "message, progress and question all forward");

        let bodies = fixture.sink.bodies();
        assert_eq!(bodies[0], "The build is *green*.");
        assert_eq!(bodies[1], "Still testing...");
        assert_eq!(
            bodies[2],
            "*Question from claude*\nWhere should we run this?\n\n1. Local\n2. Remote\n\nReply /agent 1, or /agent <your answer>, or /agent cancel"
        );
        // The user entry never forwards.
        assert!(!bodies.iter().any(|body| body.contains("ignored")));

        // The watermark advanced past the last assistant entry.
        assert_eq!(fixture.last_forwarded().as_deref(), Some("q1"));

        // Every send is logged as bridge traffic with conversation and entry.
        let records = fixture.sent_records();
        assert_eq!(records.len(), 3);
        assert!(
            records
                .iter()
                .all(|record| record.origin.as_deref() == Some("agent"))
        );
        assert!(
            records
                .iter()
                .all(|record| record.conversation.as_deref() == Some(CONV))
        );
        let entry_ids: Vec<_> = records
            .iter()
            .map(|record| record.entry.as_deref().unwrap())
            .collect();
        assert_eq!(entry_ids, vec!["m1", "p1", "q1"]);
    }

    #[tokio::test]
    async fn progress_is_skipped_when_reply_progress_is_off_but_the_watermark_advances() {
        let fixture = fixture(false);
        fixture.host.bind(transcript());
        let forwarded = fixture
            .forwarder()
            .forward_once(JID, OWNER, CONV)
            .await
            .unwrap();
        assert_eq!(forwarded, 2);
        assert!(
            !fixture
                .sink
                .bodies()
                .iter()
                .any(|body| body.contains("Still testing"))
        );
        // The skipped progress entry still advances the watermark, so a
        // later run with reply_progress on does not replay it.
        assert_eq!(fixture.last_forwarded().as_deref(), Some("q1"));
    }

    #[tokio::test]
    async fn last_forwarded_entry_prevents_replay_after_restart() {
        let fixture = fixture(true);
        fixture.host.bind(transcript());
        let forwarder = fixture.forwarder();
        assert_eq!(forwarder.forward_once(JID, OWNER, CONV).await.unwrap(), 3);

        // A restart: a fresh forwarder over a fresh store handle on the same
        // settings.json forwards nothing.
        let settings = Arc::new(Mutex::new(
            SettingsStore::new(fixture.settings_path.clone()).unwrap(),
        ));
        let restarted = Forwarder::new(
            settings,
            fixture.chats.clone(),
            fixture.sink.clone(),
            fixture.host.clone(),
            true,
        );
        assert_eq!(restarted.forward_once(JID, OWNER, CONV).await.unwrap(), 0);
        assert_eq!(fixture.sink.sent().len(), 3, "nothing replayed");
    }

    #[tokio::test]
    async fn a_lagged_broadcast_recovers_by_full_diff() {
        let fixture = fixture(true);
        fixture.host.bind(transcript());
        let mut receiver = fixture.host.subscribe();

        // A notification storm while nobody polls overflows the 32-capacity
        // channel.
        for _ in 0..100 {
            fixture.host.notify();
        }
        assert!(
            matches!(
                receiver.recv().await,
                Err(broadcast::error::RecvError::Lagged(_))
            ),
            "the receiver must have lagged"
        );

        // The Lagged arm: a full diff. Nothing lost, nothing duplicated.
        let forwarder = fixture.forwarder();
        assert_eq!(forwarder.forward_once(JID, OWNER, CONV).await.unwrap(), 3);
        assert_eq!(fixture.sink.sent().len(), 3);

        // Further notifications after the catch-up forward nothing new.
        fixture.host.notify();
        // Post-lag catch-up can itself report another Lagged when a send
        // races in; the forwarder's loop treats every arm alike, and so does
        // this test.
        loop {
            match receiver.recv().await {
                Ok(()) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => panic!("channel closed"),
            }
        }
        assert_eq!(forwarder.forward_once(JID, OWNER, CONV).await.unwrap(), 0);
        assert_eq!(fixture.sink.sent().len(), 3, "no duplicates");
    }

    #[tokio::test]
    async fn the_run_loop_forwards_on_notification_without_duplicates() {
        let fixture = fixture(true);
        let mut view = transcript();
        view.entries.clear();
        fixture.host.bind(view);

        let forwarder = Arc::new(fixture.forwarder());
        let task = tokio::spawn({
            let forwarder = forwarder.clone();
            let receiver = fixture.host.subscribe();
            async move {
                forwarder
                    .run(
                        JID.to_string(),
                        OWNER.to_string(),
                        CONV.to_string(),
                        Some(receiver),
                    )
                    .await
            }
        });

        fixture
            .host
            .push_entry(entry("m1", "message", "hello from the agent"));
        fixture.host.notify();
        wait_for(|| fixture.sink.sent().len() == 1).await;

        fixture
            .host
            .push_entry(entry("m2", "message", "second message"));
        fixture.host.notify();
        wait_for(|| fixture.sink.sent().len() == 2).await;

        // A redundant notification re-reads and diffs to nothing.
        fixture.host.notify();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fixture.sink.sent().len(), 2, "no duplicates");
        assert_eq!(fixture.last_forwarded().as_deref(), Some("m2"));

        task.abort();
    }

    #[tokio::test]
    async fn long_bodies_chunk_with_markers() {
        let fixture = fixture(true);
        let long = "word ".repeat(2000);
        fixture.host.bind(ConversationView {
            entries: vec![entry("m1", "message", &long)],
            ..ConversationView::default()
        });
        assert_eq!(
            fixture
                .forwarder()
                .forward_once(JID, OWNER, CONV)
                .await
                .unwrap(),
            1
        );
        let bodies = fixture.sink.bodies();
        assert!(
            bodies.len() > 1,
            "the body must chunk: {} chunks",
            bodies.len()
        );
        assert!(bodies[0].ends_with("(1/2)") || bodies[0].contains("(1/"));
        let total: usize = bodies.iter().map(|body| body.chars().count()).sum();
        assert!(total > 2000 * 5, "no chunk lost: {total}");
    }

    #[tokio::test]
    async fn forwarder_set_acquires_and_drops_forwarders_with_bindings() {
        let fixture = fixture(true);
        fixture.host.bind(transcript());
        let mut set = ForwarderSet::new(
            fixture.settings.clone(),
            fixture.chats.clone(),
            fixture.sink.clone(),
            fixture.host.clone(),
            true,
        );
        set.sync().await.unwrap();
        assert_eq!(set.running_len(), 1, "a new binding acquires a forwarder");

        // The acquired forwarder catches up on its own.
        wait_for(|| !fixture.sink.sent().is_empty()).await;

        fixture.settings.lock().await.clear_binding(JID).unwrap();
        set.sync().await.unwrap();
        assert_eq!(set.running_len(), 0, "an unbound chat loses its forwarder");
    }

    async fn wait_for(condition: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition timed out");
    }
}
