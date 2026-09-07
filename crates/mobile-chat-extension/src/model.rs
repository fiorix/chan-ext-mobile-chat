//! Retained conversation data and the public chat snapshots.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::Agent;

pub(crate) const MAX_BODY: usize = 64 * 1024;
pub(crate) const MAX_FRAME: usize = 1024 * 1024;
const PAGE_BYTES: usize = 256 * 1024;
const PAGE_MESSAGES: usize = 60;

pub(crate) fn new_id() -> String {
    let mut bytes = [0; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn check_body(body: &str) -> Result<()> {
    if body.trim().is_empty() {
        bail!("Enter a message first.");
    }
    if body.len() > MAX_BODY {
        bail!("Message is too long (maximum {} KiB).", MAX_BODY / 1024);
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Starting,
    Connecting,
    Ready,
    Working,
    Waiting,
    Stopping,
    Stopped,
    Failed,
}

impl Phase {
    pub(crate) fn ended(&self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Delivery {
    Saved,
    Sending,
    Queued,
    Read,
    Uncertain,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct AgentRun {
    pub id: String,
    pub token: String,
    pub scope: String,
    pub handle: String,
    pub window_id: String,
    pub pane_id: Option<String>,
    pub phase: Phase,
    pub detail: String,
    pub bootstrap: Delivery,
    pub launch: Delivery,
    pub started_at: u64,
    pub queue_depth: u64,
    pub session_id: Option<String>,
    #[serde(default)]
    pub accepts_input: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QuestionStatus {
    Pending,
    Answered,
    Cancelled,
    Inactive,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Question {
    pub options: Vec<String>,
    pub status: QuestionStatus,
    pub answer_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub id: String,
    pub role: String,
    pub kind: String,
    pub body: String,
    pub created_at: u64,
    pub reply_to: Option<String>,
    pub delivery: Option<Delivery>,
    pub detail: String,
    pub question: Option<Question>,
}

impl Entry {
    pub(crate) fn user(id: String, body: String, reply_to: Option<String>) -> Self {
        Self {
            id,
            role: "user".into(),
            kind: if reply_to.is_some() {
                "answer"
            } else {
                "message"
            }
            .into(),
            body,
            created_at: now(),
            reply_to,
            delivery: Some(Delivery::Saved),
            detail: String::new(),
            question: None,
        }
    }

    fn public(&self) -> Value {
        let mut value = serde_json::to_value(self).expect("entry is serializable");
        value["html"] = Value::String(crate::markdown::render(&self.body));
        value
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct ViewState {
    pub draft: String,
    pub answers: BTreeMap<String, String>,
    pub anchor: Option<String>,
    pub offset: f64,
    pub at_bottom: bool,
}

impl ViewState {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.draft.len() > MAX_BODY
            || self.answers.len() > 100
            || self.answers.values().any(|v| v.len() > MAX_BODY)
            || self.answers.values().map(String::len).sum::<usize>() > MAX_BODY
            || self.anchor.as_ref().is_some_and(|id| !valid_id(id))
            || !self.offset.is_finite()
            || self.offset.abs() > 100_000.0
        {
            bail!("Saved view is too large or invalid.");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub fingerprint: String,
    pub result: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Conversation {
    pub version: u32,
    pub id: String,
    pub title: String,
    pub agent: Agent,
    pub created_at: u64,
    pub updated_at: u64,
    pub revision: u64,
    pub view_revision: u64,
    pub view: ViewState,
    pub run: AgentRun,
    pub entries: Vec<Entry>,
    pub receipts: BTreeMap<String, Receipt>,
}

impl Conversation {
    pub(crate) fn receipt(&self, id: &str, fingerprint: &str) -> Result<Option<Value>> {
        if !valid_id(id) {
            bail!("Invalid request ID.");
        }
        if let Some(receipt) = self.receipts.get(id) {
            if receipt.fingerprint != fingerprint {
                bail!("This request ID was already used for different content.");
            }
            return Ok(Some(receipt.result.clone()));
        }
        Ok(None)
    }

    pub(crate) fn record(&mut self, id: String, fingerprint: String, result: Value) {
        self.receipts.insert(
            id,
            Receipt {
                fingerprint,
                result,
            },
        );
    }

    pub(crate) fn pending_questions(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| {
                entry
                    .question
                    .as_ref()
                    .is_some_and(|q| q.status == QuestionStatus::Pending)
            })
            .count()
    }

    pub(crate) fn stop(&mut self, phase: Phase, detail: impl Into<String>) {
        self.run.phase = phase;
        self.run.detail = detail.into();
        self.run.token.clear();
        for entry in &mut self.entries {
            if let Some(question) = &mut entry.question
                && question.status == QuestionStatus::Pending
            {
                question.status = QuestionStatus::Inactive;
            }
            if entry.delivery == Some(Delivery::Sending) {
                entry.delivery = Some(Delivery::Uncertain);
            }
        }
    }

    pub(crate) fn summary(&self) -> Value {
        json!({
            "id": self.id, "title": self.title, "agent": self.agent.name,
            "updated_at": self.updated_at, "phase": self.run.phase,
            "pending_questions": self.pending_questions(),
        })
    }

    pub(crate) fn page(&self, before: Option<&str>) -> Result<Value> {
        let end = match before {
            Some(id) => self
                .entries
                .iter()
                .position(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("Message is not in this conversation."))?,
            None => self.entries.len(),
        };
        let mut bytes = 0;
        let mut start = end;
        for entry in self.entries[..end].iter().rev().take(PAGE_MESSAGES) {
            bytes += entry.body.len();
            if bytes > PAGE_BYTES && start < end {
                break;
            }
            start -= 1;
        }
        Ok(json!({
            "entries": self.entries[start..end].iter().map(Entry::public).collect::<Vec<_>>(),
            "has_more": start > 0,
        }))
    }

    pub(crate) fn snapshot(&self) -> Value {
        let page = self.page(None).expect("last page exists");
        json!({
            "id": self.id, "title": self.title, "agent": self.agent.name,
            "revision": self.revision, "view_revision": self.view_revision,
            "view": self.view, "phase": self.run.phase, "detail": self.run.detail,
            "handle": self.run.handle, "queue_depth": self.run.queue_depth,
            "can_connect": self.run.phase == Phase::Connecting && self.run.bootstrap == Delivery::Saved
                && self.run.session_id.is_some(),
            "pending_questions": self.pending_questions(),
            "entries": page["entries"], "has_more": page["has_more"],
            "question_states": self.entries.iter().filter_map(|entry| entry.question.as_ref()
                .map(|q| (entry.id.clone(), json!(q)))).collect::<BTreeMap<_, _>>(),
        })
    }
}
