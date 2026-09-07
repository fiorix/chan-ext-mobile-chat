use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    chat: Arc<Chat>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Runtime {
            active: AtomicBool::new(true),
            config: Config::default(),
            cs: Arc::new(Cs::for_test(dir.path().join("cs"))),
            store: Store::new(dir.path().to_path_buf()).unwrap(),
            owner: "owner".into(),
            scope: "scope".into(),
            address: "127.0.0.1:1234".parse().unwrap(),
            executable: PathBuf::from("/helper"),
        });
        let (updates, _) = broadcast::channel(16);
        let chat = Chat::create(
            NewConversation {
                id: "conversation-a".into(),
                fingerprint: "create-fingerprint".into(),
                agent: Agent {
                    name: "claude".into(),
                    command: "wrapper --profile 'mobile chat'".into(),
                    submit_chord: "claude".into(),
                    prompt_argument: false,
                },
                body: "First task".into(),
                window_id: "window-a".into(),
                pane_id: Some("pane-a".into()),
            },
            runtime,
            updates,
        )
        .unwrap();
        Self { _dir: dir, chat }
    }

    async fn agent(&self, id: &str, action: Operation) -> Result<Value> {
        self.chat
            .agent(
                self.chat.data().run.token,
                Request {
                    id: id.into(),
                    action,
                },
            )
            .await
    }

    async fn ready_and_read(&self) -> String {
        self.agent("ready", Operation::Ready).await.unwrap();
        let message = self.chat.data().entries[0].id.clone();
        self.agent(
            "read",
            Operation::Read {
                message: message.clone(),
            },
        )
        .await
        .unwrap();
        message
    }
}

#[tokio::test]
async fn replies_and_questions_round_trip_without_a_blocking_survey() {
    let fixture = Fixture::new();
    let message = fixture.ready_and_read().await;
    let reply = Operation::Reply {
        to: message.clone(),
        body: "**Working**".into(),
        progress: true,
    };
    let first = fixture.agent("progress", reply.clone()).await.unwrap();
    assert_eq!(fixture.agent("progress", reply).await.unwrap(), first);
    let question = fixture
        .agent(
            "question",
            Operation::Ask {
                to: message,
                body: "Which target?".into(),
                options: vec!["Local".into(), "Remote".into()],
            },
        )
        .await
        .unwrap();
    let question_id = question["question_id"].as_str().unwrap().to_string();
    assert_eq!(fixture.chat.data().pending_questions(), 1);
    let answer = fixture
        .chat
        .send(
            "answer-request".into(),
            "answer-fingerprint".into(),
            "Remote, with the existing settings".into(),
            Some(question_id.clone()),
            false,
        )
        .await
        .unwrap();
    let repeated = fixture
        .chat
        .send(
            "answer-request".into(),
            "answer-fingerprint".into(),
            "Remote, with the existing settings".into(),
            Some(question_id.clone()),
            false,
        )
        .await
        .unwrap();
    assert_eq!(answer, repeated);
    assert!(
        fixture
            .chat
            .send(
                "different-answer".into(),
                "different".into(),
                "Local".into(),
                Some(question_id),
                false
            )
            .await
            .is_err()
    );
    let read = fixture
        .agent(
            "read-answer",
            Operation::Read {
                message: answer["message_id"].as_str().unwrap().into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(read["body"], "Remote, with the existing settings");
    assert_eq!(fixture.chat.data().pending_questions(), 0);
    let (saved, errors) = fixture.chat.runtime.store.load_all().unwrap();
    assert!(errors.is_empty());
    assert_eq!(saved[0].entries.len(), 4);
    assert!(
        !saved[0]
            .snapshot()
            .to_string()
            .contains(&saved[0].run.token)
    );
}

#[tokio::test]
async fn duplicate_ids_are_idempotent_and_cannot_change_content() {
    let fixture = Fixture::new();
    let message = fixture.ready_and_read().await;
    let reply = Operation::Reply {
        to: message.clone(),
        body: "Done".into(),
        progress: false,
    };
    let (a, b) = tokio::join!(
        fixture.agent("same-id", reply.clone()),
        fixture.agent("same-id", reply)
    );
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(fixture.chat.data().entries.len(), 2);
    assert!(
        fixture
            .agent(
                "same-id",
                Operation::Reply {
                    to: message,
                    body: "Different".into(),
                    progress: false
                }
            )
            .await
            .is_err()
    );
    assert!(
        fixture
            .chat
            .agent(
                "wrong-token".into(),
                Request {
                    id: "wrong".into(),
                    action: Operation::Ready
                }
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.chat.data().entries.len(), 2);
}

#[tokio::test]
async fn saved_draft_uses_revision_checks_and_survives_reload() {
    let fixture = Fixture::new();
    let view = ViewState {
        draft: "A draft".into(),
        answers: BTreeMap::from([("question".into(), "Partial answer".into())]),
        anchor: Some("message".into()),
        offset: -12.0,
        at_bottom: false,
    };
    let result = fixture.chat.save_view(0, view.clone()).await.unwrap();
    assert_eq!(result["view_revision"], 1);
    assert_eq!(
        fixture.chat.save_view(0, view.clone()).await.unwrap()["view_revision"],
        1
    );
    assert!(
        fixture
            .chat
            .save_view(0, ViewState::default())
            .await
            .is_err()
    );
    let (saved, _) = fixture.chat.runtime.store.load_all().unwrap();
    assert_eq!(saved[0].view, view);
}

#[tokio::test]
async fn a_failed_disk_write_changes_neither_memory_nor_acknowledged_revision() {
    let fixture = Fixture::new();
    let data = fixture.chat.data();
    let path = fixture._dir.path().join(format!("{}.json", data.id));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(
        fixture
            .chat
            .save_view(
                0,
                ViewState {
                    draft: "Must not commit".into(),
                    ..ViewState::default()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.chat.data().revision, data.revision);
    assert!(fixture.chat.data().view.draft.is_empty());
}

#[tokio::test]
async fn replacement_scope_keeps_history_and_revokes_the_old_agent() {
    let fixture = Fixture::new();
    let message = fixture.ready_and_read().await;
    fixture
        .agent(
            "question",
            Operation::Ask {
                to: message,
                body: "Continue?".into(),
                options: vec![],
            },
        )
        .await
        .unwrap();
    let mut data = fixture.chat.data();
    data.run.scope = "previous-boot".into();
    let chat = Chat::open(
        data,
        Arc::clone(&fixture.chat.runtime),
        fixture.chat.updates.clone(),
    )
    .unwrap();
    assert_eq!(chat.data().run.phase, Phase::Stopped);
    assert!(chat.data().run.token.is_empty());
    assert_eq!(chat.data().entries.len(), 2);
    assert_eq!(
        chat.data().entries[1].question.as_ref().unwrap().status,
        QuestionStatus::Inactive
    );
}

#[tokio::test]
async fn interrupted_writes_are_not_returned_to_the_delivery_queue() {
    let fixture = Fixture::new();
    let mut data = fixture.chat.data();
    data.run.launch = Delivery::Sending;
    data.run.bootstrap = Delivery::Sending;
    data.entries[0].delivery = Some(Delivery::Sending);
    let chat = Chat::open(
        data,
        Arc::clone(&fixture.chat.runtime),
        fixture.chat.updates.clone(),
    )
    .unwrap();
    assert_eq!(chat.data().run.launch, Delivery::Uncertain);
    assert_eq!(chat.data().run.bootstrap, Delivery::Uncertain);
    assert_eq!(chat.data().entries[0].delivery, Some(Delivery::Uncertain));
}

#[test]
fn launch_uses_the_real_agent_command_and_independent_environment() {
    let fixture = Fixture::new();
    let data = fixture.chat.data();
    let args = spawn_args(
        &data,
        "pane-a",
        &PathBuf::from("/helper with spaces"),
        &PathBuf::from("/private/session.json"),
    );
    assert_eq!(&args[..2], ["terminal", "new"]);
    assert!(!args.iter().any(|arg| arg == "team"));
    assert_eq!(
        args[args.iter().position(|arg| arg == "--command").unwrap() + 1],
        data.agent.command
    );
    assert!(args.contains(&"CHAN_AGENT=claude".to_string()));
    assert!(args.contains(&"MOBILE_CHAT_HELPER=/helper with spaces".to_string()));
    assert!(brief().len() < 4096);
    assert!(message_prompt(&new_id()).len() < 4096);
}

#[tokio::test]
async fn larger_messages_use_short_queue_references_and_history_pages() {
    let fixture = Fixture::new();
    let text = "x".repeat(16 * 1024);
    fixture
        .chat
        .send(
            "long-message".into(),
            "long-fingerprint".into(),
            text.clone(),
            None,
            false,
        )
        .await
        .unwrap();
    let data = fixture.chat.data();
    assert_eq!(data.entries[1].body, text);
    assert!(message_prompt(&data.entries[1].id).len() < 4096);
    let page = data.page(Some(&data.entries[1].id)).unwrap();
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert!(data.page(Some("not-a-message")).is_err());
}

#[cfg(unix)]
impl Fixture {
    async fn executable_cs(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self._dir.path().join("cs");
        std::fs::write(
            &path,
            r#"#!/bin/sh
printf '%s\000' "$@" >> "$0.calls"
case "$1 $2" in
  'pane list') printf '{"activePaneId":"pane-a","panes":[{"id":"pane-a"}]}' ;;
  'terminal list') cat "$0.inventory" ;;
  'terminal write') if [ -f "$0.fail" ]; then echo 'lost acknowledgment' >&2; exit 1; fi ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        self.chat
            .runtime
            .cs
            .set_test_socket("window-a", self._dir.path().join("socket"))
            .await;
        path
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self._dir.path().join("cs.calls"))
            .unwrap()
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn launch_returns_to_chat_and_does_not_send_user_work_before_ready() {
    let fixture = Fixture::new();
    fixture.executable_cs().await;
    fixture.chat.launch().await.unwrap();
    assert_eq!(fixture.chat.data().run.launch, Delivery::Queued);
    assert!(
        fixture
            .calls()
            .windows(5)
            .any(|args| args == ["pane", "focus", "pane-a", "--side", "a"])
    );
    assert!(
        !fixture
            .calls()
            .iter()
            .any(|arg| arg == "write" || arg == "team")
    );
    fixture.chat.bootstrap().await.unwrap();
    assert_eq!(fixture.chat.data().run.bootstrap, Delivery::Queued);
    assert_eq!(
        fixture.chat.data().entries[0].delivery,
        Some(Delivery::Saved)
    );
    fixture.agent("ready", Operation::Ready).await.unwrap();
    fixture.chat.deliver().await.unwrap();
    fixture.chat.deliver().await.unwrap();
    assert_eq!(
        fixture.chat.data().entries[0].delivery,
        Some(Delivery::Queued)
    );
    assert_eq!(
        fixture
            .calls()
            .iter()
            .filter(|arg| arg.starts_with("Mobile Chat message "))
            .count(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ambiguous_delivery_is_retained_and_never_requeued_by_health_checks() {
    let fixture = Fixture::new();
    fixture.executable_cs().await;
    std::fs::write(fixture._dir.path().join("cs.fail"), "").unwrap();
    fixture.agent("ready", Operation::Ready).await.unwrap();
    fixture.chat.deliver().await.unwrap();
    fixture.chat.deliver().await.unwrap();
    assert_eq!(
        fixture.chat.data().entries[0].delivery,
        Some(Delivery::Uncertain)
    );
    assert_eq!(
        fixture
            .calls()
            .iter()
            .filter(|arg| arg.as_str() == "write")
            .count(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stop_targets_only_its_agent_and_retains_history_and_inactive_questions() {
    let fixture = Fixture::new();
    fixture.executable_cs().await;
    let message = fixture.ready_and_read().await;
    fixture
        .agent(
            "question",
            Operation::Ask {
                to: message,
                body: "Continue?".into(),
                options: vec![],
            },
        )
        .await
        .unwrap();
    let handle = fixture.chat.data().run.handle;
    fixture
        .chat
        .stop("stop".into(), "fingerprint".into())
        .await
        .unwrap();
    fixture
        .chat
        .stop("stop".into(), "fingerprint".into())
        .await
        .unwrap();
    assert_eq!(
        fixture.calls(),
        ["terminal", "close", "--tab-name", &handle]
    );
    assert_eq!(fixture.chat.data().entries.len(), 2);
    assert_eq!(
        fixture.chat.data().entries[1]
            .question
            .as_ref()
            .unwrap()
            .status,
        QuestionStatus::Inactive
    );
    assert!(fixture.agent("old-agent", Operation::Ready).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn native_initial_prompt_does_not_type_into_startup_dialogs() {
    let fixture = Fixture::new();
    fixture.executable_cs().await;
    fixture
        .chat
        .change(|data| {
            data.agent.prompt_argument = true;
            Ok(())
        })
        .await
        .unwrap();
    fixture.chat.launch().await.unwrap();
    assert_eq!(fixture.chat.data().run.bootstrap, Delivery::Queued);
    assert!(!fixture.calls().iter().any(|arg| arg == "write"));
    assert!(fixture.calls().iter().any(
        |arg| arg.starts_with("wrapper --profile 'mobile chat' -- '")
            && arg.contains("You are in Mobile Chat.")
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn manual_bootstrap_requires_connect_and_is_never_replayed() {
    let fixture = Fixture::new();
    fixture.executable_cs().await;
    let inventory = json!({"groups":{"mobile-chat":[{"name":fixture.chat.data().run.handle,"session_id":"session-a","queue_depth":0}]}});
    std::fs::write(
        fixture._dir.path().join("cs.inventory"),
        inventory.to_string(),
    )
    .unwrap();
    let mut missing = 0;
    fixture.chat.tick(&mut missing).await.unwrap();
    fixture.chat.tick(&mut missing).await.unwrap();
    assert_eq!(fixture.chat.data().run.bootstrap, Delivery::Saved);
    assert!(!fixture.calls().iter().any(|arg| arg == "write"));
    fixture.chat.connect_agent().await.unwrap();
    fixture.chat.connect_agent().await.unwrap();
    assert_eq!(
        fixture.calls().iter().filter(|arg| *arg == "write").count(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn only_one_message_is_delivered_until_the_agent_finishes_its_turn() {
    let fixture = Fixture::new();
    fixture.executable_cs().await;
    fixture.agent("ready", Operation::Ready).await.unwrap();
    fixture
        .chat
        .send(
            "second".into(),
            "second".into(),
            "Follow up".into(),
            None,
            false,
        )
        .await
        .unwrap();
    fixture.chat.deliver().await.unwrap();
    fixture.chat.deliver().await.unwrap();
    assert_eq!(
        fixture.calls().iter().filter(|arg| *arg == "write").count(),
        1
    );
    let message = fixture.chat.data().entries[0].id.clone();
    fixture
        .agent(
            "read",
            Operation::Read {
                message: message.clone(),
            },
        )
        .await
        .unwrap();
    fixture
        .agent(
            "progress",
            Operation::Reply {
                to: message.clone(),
                body: "Waiting on a native prompt".into(),
                progress: true,
            },
        )
        .await
        .unwrap();
    fixture.chat.deliver().await.unwrap();
    assert_eq!(
        fixture.calls().iter().filter(|arg| *arg == "write").count(),
        1
    );
    fixture
        .agent(
            "done",
            Operation::Reply {
                to: message,
                body: "Done".into(),
                progress: false,
            },
        )
        .await
        .unwrap();
    fixture.chat.deliver().await.unwrap();
    assert_eq!(
        fixture.calls().iter().filter(|arg| *arg == "write").count(),
        2
    );
}
