//! Chat directories, `meta.json`, JSONL append, rotation and the seen-id
//! dedupe window.
//!
//! One directory per chat under `<root>/chats/<slug>-<hash8>/`, created once
//! and never renamed so a contact rename does not orphan history. The jid to
//! directory index lives in `settings.json` and is authoritative: on reopen
//! the known directory name wins over a freshly computed slug, so a renamed
//! contact keeps logging into its original directory. `meta.json` records the
//! current display name for humans.
//!
//! Log records are one JSON object per line, appended and never rewritten.
//! Rotation shifts `log.N.jsonl` to `log.N+1` on whole-record boundaries,
//! keeps `log_keep` files, and always writes a single oversized record whole;
//! the next append after an oversized record rotates.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::settings::SettingsStore;

/// How many message ids per chat are remembered so at-least-once delivery
/// does not duplicate log lines.
const SEEN_WINDOW: usize = 1000;

/// Media attachment metadata recorded inside a [`LogRecord`]. `path` is
/// relative to the chat directory so the log stays portable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRecord {
    pub path: String,
    pub mime: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

/// One appended log record. Serialized as one JSON object per line; optional
/// fields are omitted rather than nulled so the log reads cleanly. `origin`,
/// `conversation` and `entry` appear only on bridge traffic (`from_me`),
/// making the log a complete transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    /// RFC 3339 UTC.
    pub ts: String,
    /// The same instant as `ts`, in seconds, for cheap tailing.
    pub ts_unix: u64,
    pub id: String,
    pub chat: String,
    pub sender: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_lid: Option<String>,
    pub push_name: String,
    pub from_me: bool,
    /// One of `text`, `image`, `video`, `audio`, `voice`, `document`,
    /// `sticker`, `reaction`, `other`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted_id: Option<String>,
    /// Bridge traffic origin (`"agent"`); absent for a human's own messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

/// `meta.json` contents: what the chat is, under its directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMeta {
    pub jid: String,
    /// `dm` or `group`.
    pub kind: String,
    /// Current display name, refreshed on every sighting.
    pub display_name: String,
    pub first_seen: u64,
    pub last_seen: u64,
}

/// Handle to an opened chat directory, returned by [`ChatStore::ensure_chat`].
#[derive(Debug, Clone)]
pub struct ChatDir {
    /// Absolute path `<root>/chats/<slug>-<hash8>`.
    pub dir: PathBuf,
    /// The directory name, as indexed in `settings.json`.
    pub name: String,
    /// True when the directory was created by this call.
    pub created: bool,
    pub meta: ChatMeta,
}

/// The on-disk chat store: chat directories, JSONL logs and rotation.
pub struct ChatStore {
    root: PathBuf,
    log_max_bytes: u64,
    log_keep: u32,
    /// jid to open directory, populated by `ensure_chat`.
    dirs: HashMap<String, PathBuf>,
    /// jid to the last `SEEN_WINDOW` message ids, in arrival order.
    seen: HashMap<String, VecDeque<String>>,
}

impl ChatStore {
    /// Create the store root at mode 0700, refusing a symlinked root,
    /// following `Store::new` in the extension's conversation store.
    pub fn new(root: &Path, log_max_bytes: u64, log_keep: u32) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        if fs::symlink_metadata(root)?.file_type().is_symlink() {
            bail!("WhatsApp chat store directory cannot be a symlink.");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        }
        fs::create_dir_all(root.join("chats"))?;
        Ok(Self {
            root: root.to_path_buf(),
            log_max_bytes,
            log_keep,
            dirs: HashMap::new(),
            seen: HashMap::new(),
        })
    }

    /// Open (creating if needed) the chat directory for `jid`, link it in the
    /// settings index, and refresh `meta.json`. The directory name comes from
    /// the settings index when known, else from the current display name, so a
    /// rename never moves or orphans history.
    ///
    /// `display_name` is contact- or group-derived. For a group it is empty:
    /// the directory slug and `display_name` then fall back to the jid, and
    /// an existing directory's `display_name` is left untouched, so member
    /// push names can neither name a new group directory nor flap an
    /// existing one's display name.
    pub fn ensure_chat(
        &mut self,
        settings: &mut SettingsStore,
        jid: &str,
        kind: &str,
        display_name: &str,
        now_unix: u64,
    ) -> Result<ChatDir> {
        let update_name = !display_name.is_empty();
        let display_name = if update_name { display_name } else { jid };
        let name = settings
            .get()
            .chats
            .get(jid)
            .and_then(|chat| chat.dir.clone())
            .unwrap_or_else(|| format!("{}-{}", slugify(display_name), hash8(jid)));
        let dir = self.root.join("chats").join(&name);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        for subdir in ["images", "video", "audio", "documents", "stickers"] {
            fs::create_dir_all(dir.join("media").join(subdir))?;
        }

        let meta_path = dir.join("meta.json");
        let created = !meta_path.is_file();
        let meta = if created {
            ChatMeta {
                jid: jid.to_string(),
                kind: kind.to_string(),
                display_name: display_name.to_string(),
                first_seen: now_unix,
                last_seen: now_unix,
            }
        } else {
            let mut meta: ChatMeta = serde_json::from_slice(&fs::read(&meta_path)?)
                .with_context(|| format!("parsing {}", meta_path.display()))?;
            if meta.jid != jid {
                bail!(
                    "Chat directory {} belongs to {}, not {}.",
                    dir.display(),
                    meta.jid,
                    jid
                );
            }
            if update_name {
                meta.display_name = display_name.to_string();
            }
            meta.last_seen = now_unix;
            meta
        };
        atomic_json(&meta_path, &meta)?;
        if settings
            .get()
            .chats
            .get(jid)
            .and_then(|chat| chat.dir.as_deref())
            != Some(name.as_str())
        {
            settings.set_dir(jid, &name)?;
        }
        self.dirs.insert(jid.to_string(), dir.clone());
        Ok(ChatDir {
            dir,
            name,
            created,
            meta,
        })
    }

    /// Open a previously created chat directory from the settings index when
    /// it exists on disk; a no-op when the chat is already open or unknown.
    /// Unlike [`ChatStore::ensure_chat`] this never creates a directory or
    /// rewrites metadata, so callers that only append (the outbound
    /// forwarder) do not invent display names.
    pub fn reopen(&mut self, settings: &mut SettingsStore, jid: &str) -> Result<Option<ChatDir>> {
        if self.dirs.contains_key(jid) {
            return Ok(None);
        }
        let Some(name) = settings
            .get()
            .chats
            .get(jid)
            .and_then(|chat| chat.dir.clone())
        else {
            return Ok(None);
        };
        let dir = self.root.join("chats").join(&name);
        let meta_path = dir.join("meta.json");
        if !meta_path.is_file() {
            return Ok(None);
        }
        let meta: ChatMeta = serde_json::from_slice(&fs::read(&meta_path)?)
            .with_context(|| format!("parsing {}", meta_path.display()))?;
        if meta.jid != jid {
            bail!(
                "Chat directory {} belongs to {}, not {}.",
                dir.display(),
                meta.jid,
                jid
            );
        }
        self.dirs.insert(jid.to_string(), dir.clone());
        Ok(Some(ChatDir {
            dir,
            name,
            created: false,
            meta,
        }))
    }

    /// Append a record to the chat's `log.jsonl`, rotating first when the
    /// append would pass `log_max_bytes`. Returns `false` without writing when
    /// the message id was already seen in this chat.
    pub fn append(&mut self, jid: &str, record: &LogRecord) -> Result<bool> {
        let window = self.seen.entry(jid.to_string()).or_default();
        if window.contains(&record.id) {
            return Ok(false);
        }
        if window.len() >= SEEN_WINDOW {
            window.pop_front();
        }
        window.push_back(record.id.clone());

        let dir = self
            .dirs
            .get(jid)
            .with_context(|| format!("chat {jid} is not open; call ensure_chat first"))?
            .clone();
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let log = dir.join("log.jsonl");
        let len = fs::metadata(&log).map(|meta| meta.len()).unwrap_or(0);
        if len > 0 && len + line.len() as u64 > self.log_max_bytes {
            self.rotate(&dir)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .with_context(|| format!("appending {}", log.display()))?;
        file.write_all(&line)?;
        Ok(true)
    }

    /// Shift `log.N.jsonl` to `log.N+1` up to `log_keep`, dropping what falls
    /// off the end, then move `log.jsonl` to `log.1.jsonl`.
    fn rotate(&self, dir: &Path) -> Result<()> {
        let log = dir.join("log.jsonl");
        if self.log_keep == 0 {
            let _ = fs::remove_file(&log);
            return Ok(());
        }
        for n in (1..self.log_keep).rev() {
            let from = dir.join(format!("log.{n}.jsonl"));
            if from.is_file() {
                rename_replace(&from, &dir.join(format!("log.{}.jsonl", n + 1)))?;
            }
        }
        if log.is_file() {
            rename_replace(&log, &dir.join("log.1.jsonl"))?;
        }
        Ok(())
    }
}

/// `<slug>` is the display name lowercased to ASCII alphanumerics and
/// hyphens, truncated to 32 bytes, empty falling back to `chat`.
fn slugify(display_name: &str) -> String {
    let mut slug = String::new();
    let mut dashed = false;
    for ch in display_name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            dashed = false;
        } else if !slug.is_empty() && !dashed {
            slug.push('-');
            dashed = true;
        }
    }
    let mut slug: String = slug.chars().take(32).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("chat");
    }
    slug
}

/// First 8 hex characters of `sha256(jid)`.
fn hash8(jid: &str) -> String {
    let digest = Sha256::digest(jid.as_bytes());
    let mut hex = String::with_capacity(8);
    for byte in &digest[..4] {
        hex.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
        hex.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap());
    }
    hex
}

/// Rename, replacing an existing destination. Windows refuses
/// rename-over-existing, so retry once after removing the destination; every
/// caller writes small files where that trade-off is acceptable.
fn rename_replace(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(first) => {
            let _ = fs::remove_file(to);
            fs::rename(from, to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))
                .map_err(|second| second.context(format!("first attempt failed: {first:#}")))
        }
    }
}

/// Write `value` as JSON to a temp file in the same directory and rename it
/// over `path`, following `store::atomic_json` in the extension: refuse to
/// replace a non-regular file, fsync the temp file and (on unix) the
/// directory, and create the temp file at mode 0600.
pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("record has no parent directory")?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.is_file()
    {
        bail!(
            "Refusing to replace a non-regular record: {}",
            path.display()
        );
    }
    let bytes = serde_json::to_vec(value)?;
    let file_name = path
        .file_name()
        .context("record has no file name")?
        .to_string_lossy();
    let tmp = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let write = || -> Result<()> {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    };
    if let Err(error) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    rename_replace(&tmp, path)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SettingsStore;

    fn store(root: &Path, max: u64, keep: u32) -> ChatStore {
        ChatStore::new(root, max, keep).unwrap()
    }

    fn settings(root: &Path) -> SettingsStore {
        SettingsStore::new(root.join("settings.json")).unwrap()
    }

    fn open(store: &mut ChatStore, settings: &mut SettingsStore, jid: &str, name: &str) -> ChatDir {
        store
            .ensure_chat(settings, jid, "dm", name, 1_700_000_000)
            .unwrap()
    }

    fn record(id: &str, jid: &str, text: &str) -> LogRecord {
        LogRecord {
            ts: "2026-09-07T12:34:56Z".to_string(),
            ts_unix: 1_757_248_496,
            id: id.to_string(),
            chat: jid.to_string(),
            sender: "5511987654321@s.whatsapp.net".to_string(),
            sender_lid: None,
            push_name: "Alex".to_string(),
            from_me: false,
            kind: "text".to_string(),
            text: Some(text.to_string()),
            media: None,
            quoted_id: None,
            origin: None,
            conversation: None,
            entry: None,
        }
    }

    fn line(record: &LogRecord) -> Vec<u8> {
        let mut line = serde_json::to_vec(record).unwrap();
        line.push(b'\n');
        line
    }

    fn read_lines(path: &Path) -> Vec<LogRecord> {
        if !path.is_file() {
            return Vec::new();
        }
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn slug_and_index_and_meta_on_first_sighting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let mut store = store(&root, 1024, 10);
        let mut settings = settings(&root);
        let chat = open(&mut store, &mut settings, "5511@s.whatsapp.net", "Alex P!");
        assert!(chat.created);
        assert!(chat.name.starts_with("alex-p-"), "slug: {}", chat.name);
        assert!(chat.dir.is_dir());
        assert_eq!(chat.meta.first_seen, 1_700_000_000);
        assert!(chat.dir.join("media/images").is_dir());
        // The jid index is written into settings.json.
        assert_eq!(
            settings.chat("5511@s.whatsapp.net").unwrap().dir.as_deref(),
            Some(chat.name.as_str())
        );
        // meta.json round-trips.
        let meta: ChatMeta =
            serde_json::from_slice(&fs::read(chat.dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.jid, "5511@s.whatsapp.net");
    }

    #[test]
    fn rename_keeps_the_same_directory_and_updates_meta() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let mut store = store(&root, 1024, 10);
        let mut settings = settings(&root);
        let first = open(&mut store, &mut settings, "5511@s.whatsapp.net", "Alex");
        let second = open(
            &mut store,
            &mut settings,
            "5511@s.whatsapp.net",
            "Alexander the Great",
        );
        assert!(!second.created);
        assert_eq!(first.name, second.name, "a rename must not move history");
        assert_eq!(second.meta.display_name, "Alexander the Great");
        assert_eq!(second.meta.first_seen, 1_700_000_000);
        assert!(second.meta.last_seen >= second.meta.first_seen);
    }

    #[test]
    fn same_name_collisions_get_distinct_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let mut store = store(&root, 1024, 10);
        let mut settings = settings(&root);
        let a = open(&mut store, &mut settings, "5511@s.whatsapp.net", "Team");
        let b = open(&mut store, &mut settings, "5522@s.whatsapp.net", "Team");
        assert_ne!(a.name, b.name);
        assert_ne!(a.hash8(), b.hash8());
        let meta_a: ChatMeta =
            serde_json::from_slice(&fs::read(a.dir.join("meta.json")).unwrap()).unwrap();
        let meta_b: ChatMeta =
            serde_json::from_slice(&fs::read(b.dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta_a.jid, "5511@s.whatsapp.net");
        assert_eq!(meta_b.jid, "5522@s.whatsapp.net");
    }

    #[test]
    fn rotation_at_the_exact_byte_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let r1 = record("id-1", "5511@s.whatsapp.net", "one");
        let r2 = record("id-2", "5511@s.whatsapp.net", "two");
        let r3 = record("id-3", "5511@s.whatsapp.net", "three");
        let max = (line(&r1).len() + line(&r2).len()) as u64;
        let mut store = store(&root, max, 10);
        let mut settings = settings(&root);
        let chat = open(&mut store, &mut settings, "5511@s.whatsapp.net", "Alex");

        assert!(store.append("5511@s.whatsapp.net", &r1).unwrap());
        assert!(store.append("5511@s.whatsapp.net", &r2).unwrap());
        // Exactly at the limit: no rotation yet.
        assert_eq!(fs::metadata(chat.dir.join("log.jsonl")).unwrap().len(), max);
        assert!(store.append("5511@s.whatsapp.net", &r3).unwrap());
        // The boundary is whole-record: r3 lands in a fresh log.jsonl and
        // r1+r2 shift to log.1.jsonl.
        assert_eq!(read_lines(&chat.dir.join("log.jsonl")), vec![r3.clone()]);
        assert_eq!(read_lines(&chat.dir.join("log.1.jsonl")), vec![r1, r2]);
    }

    #[test]
    fn log_keep_retains_the_newest_and_drops_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let r1 = record("id-1", "5511@s.whatsapp.net", "one");
        let max = line(&r1).len() as u64;
        let mut store = store(&root, max, 2);
        let mut settings = settings(&root);
        let chat = open(&mut store, &mut settings, "5511@s.whatsapp.net", "Alex");
        for n in 1..=4 {
            store
                .append(
                    "5511@s.whatsapp.net",
                    &record(&format!("id-{n}"), "5511@s.whatsapp.net", &n.to_string()),
                )
                .unwrap();
        }
        assert_eq!(read_lines(&chat.dir.join("log.jsonl")).len(), 1);
        let log1 = read_lines(&chat.dir.join("log.1.jsonl"));
        let log2 = read_lines(&chat.dir.join("log.2.jsonl"));
        assert_eq!(log1[0].id, "id-3");
        assert_eq!(log2[0].id, "id-2");
        assert!(!chat.dir.join("log.3.jsonl").exists(), "oldest must drop");
        let all: Vec<String> = fs::read_dir(&chat.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !all.iter().any(|name| name.contains("id-1")),
            "id-1 must be gone entirely: {all:?}"
        );
    }

    #[test]
    fn an_oversized_record_is_written_and_the_next_append_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let big = record("id-big", "5511@s.whatsapp.net", &"x".repeat(500));
        let next = record("id-next", "5511@s.whatsapp.net", "after");
        let mut store = store(&root, 128, 10);
        let mut settings = settings(&root);
        let chat = open(&mut store, &mut settings, "5511@s.whatsapp.net", "Alex");

        // A single record past the limit is written whole, never split.
        assert!(store.append("5511@s.whatsapp.net", &big).unwrap());
        assert_eq!(read_lines(&chat.dir.join("log.jsonl")), vec![big.clone()]);
        // The next append rotates the oversized record out of the way.
        assert!(store.append("5511@s.whatsapp.net", &next).unwrap());
        assert_eq!(read_lines(&chat.dir.join("log.jsonl")), vec![next]);
        assert_eq!(read_lines(&chat.dir.join("log.1.jsonl")), vec![big]);
    }

    #[test]
    fn jsonl_round_trip_for_text_media_quoted_and_from_me() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let jid = "5511@s.whatsapp.net";
        let mut store = store(&root, 1024 * 1024, 10);
        let mut settings = settings(&root);
        let chat = open(&mut store, &mut settings, jid, "Alex");

        let text = record("id-t", jid, "hello");
        let mut media = record("id-m", jid, "look at this");
        media.kind = "image".to_string();
        media.media = Some(MediaRecord {
            path: "media/images/1757248496-3EB0.jpg".to_string(),
            mime: "image/jpeg".to_string(),
            bytes: 123_456,
            caption: Some("a photo".to_string()),
        });
        let mut quoted = record("id-q", jid, "replying");
        quoted.quoted_id = Some("id-t".to_string());
        let mut from_me = record("id-f", jid, "the agent says hi");
        from_me.from_me = true;
        from_me.origin = Some("agent".to_string());
        from_me.conversation = Some("a1b2c3".to_string());
        from_me.entry = Some("d4e5f6".to_string());

        for record in [&text, &media, &quoted, &from_me] {
            assert!(store.append(jid, record).unwrap());
        }
        let lines = read_lines(&chat.dir.join("log.jsonl"));
        assert_eq!(lines, vec![text, media, quoted, from_me]);

        // The on-disk shape matches the plan: no null optional fields.
        let raw = fs::read_to_string(chat.dir.join("log.jsonl")).unwrap();
        let first: serde_json::Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert!(first.get("media").is_none());
        assert!(first.get("quoted_id").is_none());
        let last: serde_json::Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
        assert_eq!(last["from_me"], true);
        assert_eq!(last["origin"], "agent");
        assert_eq!(last["conversation"], "a1b2c3");
        assert_eq!(last["entry"], "d4e5f6");
    }

    #[test]
    fn a_duplicate_message_id_produces_one_log_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let mut store = store(&root, 1024, 10);
        let mut settings = settings(&root);
        let chat = open(&mut store, &mut settings, "5511@s.whatsapp.net", "Alex");
        let record = record("id-1", "5511@s.whatsapp.net", "at-least-once");
        assert!(store.append("5511@s.whatsapp.net", &record).unwrap());
        assert!(
            !store.append("5511@s.whatsapp.net", &record).unwrap(),
            "the duplicate must be skipped"
        );
        assert_eq!(read_lines(&chat.dir.join("log.jsonl")).len(), 1);
    }

    #[test]
    fn slugify_maps_names_to_safe_directories() {
        assert_eq!(slugify("Alex P!"), "alex-p");
        assert_eq!(slugify("Team Standup 🚀"), "team-standup");
        assert_eq!(slugify("!!!"), "chat");
        assert_eq!(slugify(""), "chat");
        assert_eq!(slugify(&"a".repeat(64)), "a".repeat(32));
        assert_eq!(slugify("UPPER_case"), "upper-case");
    }

    impl ChatDir {
        fn hash8(&self) -> &str {
            self.name.rsplit('-').next().unwrap()
        }
    }
}
