//! `settings.json` schema, atomic load/save, live reload.
//!
//! Everything a person changes from the phone lives here rather than in
//! `mobile-chat.toml`, because the config file needs a Chan restart and an
//! allowlist that cannot be edited from the phone is useless. The file is
//! process-wide while conversations are per workspace: bindings name a
//! specific `(owner, conversation)` pair.
//!
//! The file is this process's own record plus an external edit surface (it is
//! rewritten by control-socket actions and can be hand-edited), so reads
//! reload whenever the file's mtime changes. A file that momentarily cannot
//! be parsed — for example caught mid-rename by a naive editor — never fails
//! a read: the in-memory copy keeps serving until the file parses again.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::store::atomic_json;

const SETTINGS_VERSION: u32 = 1;

fn default_version() -> u32 {
    SETTINGS_VERSION
}

/// Per-chat enablement and binding, keyed by jid in [`Settings::chats`]. The
/// jid to directory index (`dir`) is authoritative for the on-disk layout.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatSettings {
    /// Directory name under `chats/`, the authoritative jid index target.
    pub dir: Option<String>,
    /// Record this chat's messages to its log.
    pub record: bool,
    /// Bound Mobile Chat conversation id, if any.
    pub conversation: Option<String>,
    /// Owning tenant key (`owner_key(scope, workspace)`) of the binding.
    pub owner: Option<String>,
    /// Last assistant entry forwarded back, so a restart does not replay.
    pub last_forwarded_entry: Option<String>,
}

/// Process-wide settings: chat enablement and bindings, the default-deny
/// allowlist keyed on E.164 without a plus, and observed LID counterparts
/// (`phone -> lid jid`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    #[serde(default = "default_version")]
    pub version: u32,
    pub chats: BTreeMap<String, ChatSettings>,
    pub allow: Vec<String>,
    pub lids: BTreeMap<String, String>,
}

/// Live-reloading access to `settings.json`. Every accessor reloads first
/// when the file's mtime changes; a missing or momentarily unparsable file
/// falls back to the in-memory copy. Mutations save atomically (temp file in
/// the same directory, then rename) and update the cached copy.
pub struct SettingsStore {
    path: PathBuf,
    settings: Settings,
    mtime: Option<SystemTime>,
}

impl SettingsStore {
    /// Load the file when it exists, else start from defaults. A file with an
    /// unsupported version is an error at construction, not a silent reset.
    pub fn new(path: PathBuf) -> Result<Self> {
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path,
                settings: Settings {
                    version: SETTINGS_VERSION,
                    ..Settings::default()
                },
                mtime: None,
            }),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
            Ok(metadata) => {
                if !metadata.is_file() {
                    bail!("Expected a regular file: {}", path.display());
                }
                let settings: Settings = serde_json::from_slice(&fs::read(&path)?)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if settings.version != SETTINGS_VERSION {
                    bail!(
                        "Unsupported settings.json version {} (want {}).",
                        settings.version,
                        SETTINGS_VERSION
                    );
                }
                Ok(Self {
                    path,
                    settings,
                    mtime: metadata.modified().ok(),
                })
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current settings, reloading from disk first when the file changed.
    pub fn get(&mut self) -> &Settings {
        self.refresh();
        &self.settings
    }

    /// A copy of one chat's settings, if the jid is known.
    pub fn chat(&mut self, jid: &str) -> Option<ChatSettings> {
        self.refresh();
        self.settings.chats.get(jid).cloned()
    }

    /// Enable or disable recording for a chat.
    pub fn set_record(&mut self, jid: &str, record: bool) -> Result<()> {
        self.refresh();
        self.settings
            .chats
            .entry(jid.to_string())
            .or_default()
            .record = record;
        self.save()
    }

    /// Bind a chat to a Mobile Chat conversation and its owning tenant.
    pub fn set_binding(&mut self, jid: &str, conversation: &str, owner: &str) -> Result<()> {
        self.refresh();
        let chat = self.settings.chats.entry(jid.to_string()).or_default();
        chat.conversation = Some(conversation.to_string());
        chat.owner = Some(owner.to_string());
        self.save()
    }

    /// Remove a binding, if any; recording is untouched.
    pub fn clear_binding(&mut self, jid: &str) -> Result<()> {
        self.refresh();
        let Some(chat) = self.settings.chats.get_mut(jid) else {
            return Ok(());
        };
        chat.conversation = None;
        chat.owner = None;
        self.save()
    }

    /// Persist the last forwarded entry so a restart does not replay the
    /// transcript.
    pub fn set_last_forwarded_entry(&mut self, jid: &str, entry: &str) -> Result<()> {
        self.refresh();
        self.settings
            .chats
            .entry(jid.to_string())
            .or_default()
            .last_forwarded_entry = Some(entry.to_string());
        self.save()
    }

    /// Record the chat's directory name in the jid index.
    pub fn set_dir(&mut self, jid: &str, dir: &str) -> Result<()> {
        self.refresh();
        self.settings.chats.entry(jid.to_string()).or_default().dir = Some(dir.to_string());
        self.save()
    }

    /// Default-deny allowlist check on a phone number in E.164 without `+`.
    pub fn allows_phone(&mut self, phone: &str) -> bool {
        self.refresh();
        self.settings
            .allow
            .contains(&phone.trim_start_matches('+').to_string())
    }

    /// Add or remove a phone number (E.164, optional `+`) on the default-deny
    /// allowlist. The first mutation on a fresh install creates
    /// `settings.json`.
    pub fn set_allow(&mut self, phone: &str, allow: bool) -> Result<()> {
        self.refresh();
        let phone = phone.trim_start_matches('+');
        if allow {
            if !self.settings.allow.iter().any(|listed| listed == phone) {
                self.settings.allow.push(phone.to_string());
            }
        } else {
            self.settings.allow.retain(|listed| listed != phone);
        }
        self.save()
    }

    /// Record the LID counterpart observed for a phone number, so a
    /// LID-addressed sender resolves to the same person. No-op (and no
    /// rewrite) when already recorded.
    pub fn record_lid(&mut self, phone: &str, lid: &str) -> Result<()> {
        self.refresh();
        let phone = phone.trim_start_matches('+');
        if self
            .settings
            .lids
            .get(phone)
            .is_some_and(|known| known == lid)
        {
            return Ok(());
        }
        self.settings
            .lids
            .insert(phone.to_string(), lid.to_string());
        self.save()
    }

    /// Reload from disk when the file's mtime changed. Any read or parse
    /// failure keeps the in-memory copy and leaves the mtime cache untouched,
    /// so the next read retries.
    fn refresh(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if !metadata.is_file() {
            return;
        }
        let Ok(mtime) = metadata.modified() else {
            return;
        };
        if self.mtime == Some(mtime) {
            return;
        }
        let Ok(bytes) = fs::read(&self.path) else {
            return;
        };
        let Ok(settings) = serde_json::from_slice::<Settings>(&bytes) else {
            return;
        };
        if settings.version != SETTINGS_VERSION {
            return;
        }
        self.settings = settings;
        self.mtime = Some(mtime);
    }

    fn save(&mut self) -> Result<()> {
        atomic_json(&self.path, &self.settings)?;
        self.mtime = fs::symlink_metadata(&self.path)
            .and_then(|metadata| metadata.modified())
            .ok();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("settings.json")
    }

    #[test]
    fn defaults_when_missing_and_round_trip_when_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SettingsStore::new(path(&dir)).unwrap();
        assert_eq!(store.get().version, 1);
        assert!(!path(&dir).exists(), "nothing is written until a mutation");

        store.set_record("5511@s.whatsapp.net", true).unwrap();
        store
            .set_binding("5511@s.whatsapp.net", "a1b2c3", "owner:ws")
            .unwrap();
        store
            .set_last_forwarded_entry("5511@s.whatsapp.net", "d4e5f6")
            .unwrap();
        store
            .set_dir("5511@s.whatsapp.net", "alex-0123abcd")
            .unwrap();
        store
            .record_lid("447700900123", "12345678901234@lid")
            .unwrap();
        // Record the allowlist by editing the file directly, as a human would.
        let mut settings: Settings =
            serde_json::from_slice(&fs::read(path(&dir)).unwrap()).unwrap();
        settings.allow.push("447700900123".to_string());
        fs::write(path(&dir), serde_json::to_vec(&settings).unwrap()).unwrap();

        // A fresh store over the same file sees everything.
        let mut store = SettingsStore::new(path(&dir)).unwrap();
        let chat = store.chat("5511@s.whatsapp.net").unwrap();
        assert!(chat.record);
        assert_eq!(chat.conversation.as_deref(), Some("a1b2c3"));
        assert_eq!(chat.owner.as_deref(), Some("owner:ws"));
        assert_eq!(chat.last_forwarded_entry.as_deref(), Some("d4e5f6"));
        assert_eq!(chat.dir.as_deref(), Some("alex-0123abcd"));
        assert!(store.allows_phone("447700900123"));
        assert!(!store.allows_phone("5511987654321"), "default deny");
        assert_eq!(
            store.get().lids.get("447700900123").map(String::as_str),
            Some("12345678901234@lid")
        );
    }

    #[test]
    fn mutations_survive_a_reload_and_external_edits_win() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SettingsStore::new(path(&dir)).unwrap();
        store.set_record("5511@s.whatsapp.net", true).unwrap();

        // External edit (as if written by a control-socket action in another
        // process): a live read must pick it up without a new store.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut settings: Settings =
            serde_json::from_slice(&fs::read(path(&dir)).unwrap()).unwrap();
        settings.allow.push("5511987654321".to_string());
        fs::write(path(&dir), serde_json::to_vec(&settings).unwrap()).unwrap();

        assert!(store.allows_phone("5511987654321"), "live reload on mtime");
        assert!(store.chat("5511@s.whatsapp.net").unwrap().record);
    }

    #[test]
    fn an_unparsable_file_falls_back_to_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SettingsStore::new(path(&dir)).unwrap();
        store.set_record("5511@s.whatsapp.net", true).unwrap();

        // Caught mid-write: truncated JSON on disk.
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(path(&dir), "{\"version\": 1, \"chats\": {").unwrap();

        let chat = store.chat("5511@s.whatsapp.net").unwrap();
        assert!(chat.record, "the in-memory copy keeps serving");
        // And the next mutation repairs the file.
        store
            .set_last_forwarded_entry("5511@s.whatsapp.net", "e7")
            .unwrap();
        let settings: Settings = serde_json::from_slice(&fs::read(path(&dir)).unwrap()).unwrap();
        assert_eq!(
            settings.chats["5511@s.whatsapp.net"]
                .last_forwarded_entry
                .as_deref(),
            Some("e7")
        );
    }

    #[test]
    fn clear_binding_keeps_recording_and_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SettingsStore::new(path(&dir)).unwrap();
        store.set_record("5511@s.whatsapp.net", true).unwrap();
        store
            .set_binding("5511@s.whatsapp.net", "a1b2c3", "owner:ws")
            .unwrap();
        store.clear_binding("5511@s.whatsapp.net").unwrap();
        let chat = store.chat("5511@s.whatsapp.net").unwrap();
        assert!(chat.record);
        assert!(chat.conversation.is_none());
        assert!(chat.owner.is_none());
        // Clearing an unknown jid is a no-op, not an error.
        store.clear_binding("unknown@s.whatsapp.net").unwrap();
    }

    #[test]
    fn set_allow_adds_removes_and_creates_the_file_on_first_use() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SettingsStore::new(path(&dir)).unwrap();
        // Fresh install: no settings.json yet; the first allowlist mutation
        // creates it.
        assert!(!path(&dir).exists());
        store.set_allow("+447700900123", true).unwrap();
        assert!(path(&dir).is_file());
        assert!(store.allows_phone("447700900123"));
        assert!(store.allows_phone("+447700900123"));

        // Adding twice does not duplicate; removal is a no-op when absent.
        store.set_allow("447700900123", true).unwrap();
        assert_eq!(store.get().allow.len(), 1);
        store.set_allow("5511987654321", false).unwrap();
        assert_eq!(store.get().allow.len(), 1);

        store.set_allow("447700900123", false).unwrap();
        assert!(!store.allows_phone("447700900123"));
    }

    #[test]
    fn record_lid_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SettingsStore::new(path(&dir)).unwrap();
        store.record_lid("+447700900123", "1234@lid").unwrap();
        let before = fs::read(path(&dir)).unwrap();
        store.record_lid("447700900123", "1234@lid").unwrap();
        let after = fs::read(path(&dir)).unwrap();
        assert_eq!(before, after, "an unchanged counterpart is not rewritten");
    }

    #[test]
    fn an_unsupported_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(path(&dir), "{\"version\": 99}").unwrap();
        assert!(SettingsStore::new(path(&dir)).is_err());
    }
}
