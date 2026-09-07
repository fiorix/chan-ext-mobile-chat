//! WhatsApp bridge for the Mobile Chat extension.
//!
//! Constructed once in `main`, next to `AppState`, not per tenant. It holds
//! the WhatsApp client, the on-disk chat store, the settings and the routing
//! tables. Everything except the socket is testable through the `WaSink` seam
//! in [`client`], so routing, allowlist, rotation and chunking logic run under
//! unit test without a network or a WhatsApp account.
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`client`] | whatsapp-rust wiring, `WaSink` implementation, event to internal type conversion |
//! | [`pairing`] | Pairing state machine, QR SVG rendering, reconnect after ref exhaustion |
//! | [`store`] | Chat directories, jid index, `meta.json`, JSONL append, rotation |
//! | [`media`] | Type and size gating, destination paths, streaming download |
//! | [`settings`] | `settings.json` schema, atomic load/save, live reload |
//! | [`command`] | `/agent` grammar, allowlist decisions, auto-reply debounce |
//! | [`route_in`] | Binding resolution, envelope construction, `Chat::send` |
//! | [`route_out`] | Per-conversation forwarder, entry diffing, question rendering |
//! | [`whatsapp_text`] | Markdown to WhatsApp formatting, chunking |

pub mod client;
pub mod command;
pub mod media;
pub mod pairing;
pub mod route_in;
pub mod route_out;
pub mod settings;
pub mod store;
pub mod whatsapp_text;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::Serialize;

/// Startup values of the `[whatsapp]` config section that the bridge needs at
/// construction time. `root` is resolved by the caller (config override or
/// `<chan-home>/mobile-chat/whatsapp`) and passed separately to [`Bridge::new`].
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub log_max_bytes: u64,
    pub log_keep: u32,
    pub media_max_bytes: u64,
    pub media: Vec<String>,
    pub reply_progress: bool,
}

/// Process-wide lifecycle state, surfaced to the UI through the control
/// protocol. Serialized as `snake_case` because these reach the browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The `[whatsapp]` section has `enabled = false`; no disk or network use.
    Disabled,
    /// Another process holds the lock file; this instance stays inert.
    Locked,
    /// Enabled and locked, but no linked device session exists yet.
    Unpaired,
    /// A pairing QR code is outstanding, waiting to be scanned.
    Pairing,
    /// Linked and connected.
    Connected,
    /// Something went wrong; `detail` is the human-readable reason.
    Error { detail: String },
}

/// The bridge: construction, lock, lifecycle, event loop. Later steps add the
/// client, store and routing tables; step 1 only constructs and locks.
pub struct Bridge {
    state: State,
    #[expect(dead_code, reason = "step 2+ hands it to the store")]
    config: BridgeConfig,
    #[expect(dead_code, reason = "step 2+ uses it for session.db")]
    root: Option<PathBuf>,
    // Held for the lifetime of the Bridge; dropping it releases the lock.
    lock: Option<fs::File>,
}

impl Bridge {
    /// Construct the bridge. When `enabled` is false this reports
    /// [`State::Disabled`] without touching disk or network. Otherwise it
    /// creates `root` at mode 0700 (refusing a symlinked root, following the
    /// conversation store's conventions), opens the `lock` file, and takes an
    /// exclusive non-blocking lock; a contended lock reports [`State::Locked`]
    /// rather than failing, so the second Chan home fails visibly but the
    /// process keeps running.
    pub fn new(enabled: bool, config: BridgeConfig, root: &Path) -> Result<Self> {
        if !enabled {
            return Ok(Self {
                state: State::Disabled,
                config,
                root: None,
                lock: None,
            });
        }
        create_root(root)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("lock"))
            .with_context(|| format!("opening {}", root.join("lock").display()))?;
        if let Err(error) = lock.try_lock_exclusive() {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(Self {
                    state: State::Locked,
                    config,
                    root: Some(root.to_path_buf()),
                    lock: Some(lock),
                });
            }
            return Err(error).context("locking the WhatsApp bridge lock file");
        }
        Ok(Self {
            state: State::Unpaired,
            config,
            root: Some(root.to_path_buf()),
            lock: Some(lock),
        })
    }

    /// The current lifecycle state, as reported by `whatsapp_status`.
    pub fn state(&self) -> &State {
        &self.state
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(lock) = &self.lock {
            let _ = lock.unlock();
        }
    }
}

/// Create `root` at mode 0700, refusing a symlinked root, following
/// `Store::new` in the extension's conversation store.
fn create_root(root: &Path) -> Result<()> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        bail!("WhatsApp bridge directory cannot be a symlink.");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BridgeConfig {
        BridgeConfig {
            log_max_bytes: 1024 * 1024,
            log_keep: 10,
            media_max_bytes: 16 * 1024 * 1024,
            media: vec!["image".to_string()],
            reply_progress: true,
        }
    }

    #[test]
    fn disabled_bridge_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let bridge = Bridge::new(false, config(), &root).unwrap();
        assert_eq!(bridge.state(), &State::Disabled);
        assert!(!root.exists(), "a disabled bridge must not create its root");
    }

    #[test]
    fn enabled_bridge_creates_root_locks_and_reports_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let bridge = Bridge::new(true, config(), &root).unwrap();
        assert_eq!(bridge.state(), &State::Unpaired);
        assert!(root.join("lock").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn a_second_bridge_in_one_process_reports_locked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("whatsapp");
        let first = Bridge::new(true, config(), &root).unwrap();
        assert_eq!(first.state(), &State::Unpaired);
        let second = Bridge::new(true, config(), &root).unwrap();
        assert_eq!(second.state(), &State::Locked);
        // Once the first releases, the same root locks again.
        drop(first);
        let third = Bridge::new(true, config(), &root).unwrap();
        assert_eq!(third.state(), &State::Unpaired);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_is_refused() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("whatsapp");
        symlink(&target, &link).unwrap();
        let result = Bridge::new(true, config(), &link);
        assert!(result.is_err(), "a symlinked root must be refused");
    }
}
