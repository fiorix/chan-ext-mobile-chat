//! Chat directories, the jid index, `meta.json`, JSONL append and rotation.
//!
//! One directory per chat under `<root>/chats/<slug>-<hash8>/`, created once
//! and never renamed so a contact rename does not orphan history. The jid to
//! directory index lives in `settings.json`; `meta.json` records the current
//! display name for humans.
//!
//! Log records are one JSON object per line, appended and never rewritten.
//! Rotation shifts `log.N.jsonl` to `log.N+1` on whole-record boundaries,
//! keeps `log_keep` files, and always writes a single oversized record whole.

/// One appended log record. Serialized as one JSON object per line.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogRecord {
    /// RFC 3339 UTC.
    pub ts: String,
    pub ts_unix: u64,
    pub id: String,
    pub chat: String,
    pub sender: String,
    pub push_name: String,
    pub from_me: bool,
    pub kind: String,
}
