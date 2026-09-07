//! `settings.json` schema, atomic load/save, live reload.
//!
//! Everything a person changes from the phone lives here rather than in
//! `mobile-chat.toml`, because the config file needs a Chan restart and an
//! allowlist that cannot be edited from the phone is useless. The file is
//! process-wide while conversations are per workspace: bindings name a
//! specific `(owner, conversation)` pair.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Per-chat enablement and binding, keyed by jid in [`Settings::chats`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatSettings {
    /// Directory name under `chats/`, the authoritative jid index target.
    pub dir: Option<String>,
    /// Record this chat's messages to its log.
    #[serde(default)]
    pub record: bool,
    /// Bound Mobile Chat conversation id, if any.
    pub conversation: Option<String>,
    /// Owning tenant key (`owner_key(scope, workspace)`) of the binding.
    pub owner: Option<String>,
    /// Last assistant entry forwarded back, so a restart does not replay.
    pub last_forwarded_entry: Option<String>,
}

/// Process-wide settings: chat enablement and bindings, the default-deny
/// allowlist keyed on E.164 without a plus, and observed LID counterparts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    pub version: u32,
    #[serde(default)]
    pub chats: BTreeMap<String, ChatSettings>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub lids: BTreeMap<String, String>,
}
