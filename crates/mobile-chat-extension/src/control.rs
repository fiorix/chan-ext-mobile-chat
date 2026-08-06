//! Driving Chan through the `cs` client.
//!
//! Chan spawns extensions with no environment of its own
//! (`chan-server::extensions::start_extension` sets none), so `$CHAN_CONTROL_SOCKET`
//! is not inherited. We rediscover it the way `chan ps` does: the extension's
//! parent process IS the serving `chan-server`, and its control sockets are named
//! `chan-control-<pid>-<rand>.sock` in a small set of runtime directories.
//!
//! One pid can own several sockets (one per served tenant), and stale sockets
//! from dead processes linger, so the pid is a filter and not an answer. The
//! socket is settled per window by probing candidates with a window-scoped
//! `cs pane list --window <id>`: the socket whose server knows our window is
//! ours by construction, and a dead socket fails the connect immediately.
//!
//! Every operation shells out to `cs` rather than re-implementing the control
//! wire, so the CLI's semantics and typed exit codes are the contract.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

/// `cs` exits 69 (sysexits `EX_UNAVAILABLE`) when a matched target was a shell
/// session that received no submit chord.
pub const EXIT_SUBMIT_REFUSED: i32 = 69;

/// One finished `cs` invocation.
#[derive(Debug, Clone)]
pub struct CsOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CsOutput {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// The most useful line to show a human: `cs` reports errors on stderr and
    /// structured results on stdout.
    pub fn message(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_string();
        }
        self.stdout.trim().to_string()
    }
}

/// A located `cs` binary plus the per-window socket cache.
pub struct Cs {
    binary: PathBuf,
    sockets: Mutex<HashMap<String, PathBuf>>,
}

impl Cs {
    /// Locate `cs`: an explicit `$MOBILE_CHAT_CS`, then `$PATH`, then a sibling
    /// of this executable (an install that ships both side by side).
    pub fn locate() -> Result<Arc<Self>> {
        let binary = locate_binary().context(
            "cannot find the `cs` binary; put it on PATH or set $MOBILE_CHAT_CS to its path",
        )?;
        Ok(Arc::new(Self {
            binary,
            sockets: Mutex::new(HashMap::new()),
        }))
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// A driver pointed at an arbitrary path, for tests that never run it.
    #[cfg(test)]
    pub fn for_test(binary: PathBuf) -> Self {
        Self {
            binary,
            sockets: Mutex::new(HashMap::new()),
        }
    }

    /// Run `cs` against the control socket serving `window_id`.
    ///
    /// `CHAN_WORKSPACE_PATH` is deliberately NOT set: `cs terminal team`
    /// anchors a relative team dir to the caller's cwd only when that variable
    /// is present, and passes it through as workspace-relative otherwise. We
    /// want the pass-through, because the extension's cwd
    /// (`~/.chan/extensions`) is nowhere near the workspace.
    pub async fn run<I, S>(&self, window_id: &str, args: I) -> Result<CsOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let socket = self.socket_for(window_id).await?;
        let output = self.run_on(&socket, window_id, args).await?;
        // A socket that stopped answering means the server behind it exited
        // (a devserver restart mints a new one). Drop the cache so the next
        // call rediscovers rather than failing forever.
        if !output.ok() && output.stderr.contains("no longer running") {
            self.sockets.lock().await.remove(window_id);
        }
        Ok(output)
    }

    async fn run_on<I, S>(&self, socket: &Path, window_id: &str, args: I) -> Result<CsOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = tokio::process::Command::new(&self.binary)
            .args(args)
            .env("CHAN_CONTROL_SOCKET", socket)
            .env("CHAN_WINDOW_ID", window_id)
            .env_remove("CHAN_WORKSPACE_PATH")
            .env_remove("CHAN_TAB_NAME")
            .env_remove("CHAN_TAB_GROUP")
            .output()
            .await
            .with_context(|| format!("running {}", self.binary.display()))?;
        Ok(CsOutput {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Settle (and cache) the control socket that serves `window_id`.
    async fn socket_for(&self, window_id: &str) -> Result<PathBuf> {
        if let Some(cached) = self.sockets.lock().await.get(window_id) {
            return Ok(cached.clone());
        }
        for candidate in socket_candidates() {
            let probe = self
                .run_on(&candidate, window_id, ["pane", "list", "--json"])
                .await?;
            if probe.ok() {
                self.sockets
                    .lock()
                    .await
                    .insert(window_id.to_string(), candidate.clone());
                return Ok(candidate);
            }
        }
        anyhow::bail!(
            "no chan control socket serves window {window_id}; \
             is this window still connected?"
        )
    }
}

fn locate_binary() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("MOBILE_CHAT_CS")
        && !explicit.trim().is_empty()
    {
        let path = PathBuf::from(explicit.trim());
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(cs_file_name());
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let sibling = std::env::current_exe().ok()?.parent()?.join(cs_file_name());
    sibling.is_file().then_some(sibling)
}

fn cs_file_name() -> &'static str {
    if cfg!(windows) { "cs.exe" } else { "cs" }
}

/// Control-socket candidates, best first: sockets named for our parent pid,
/// then stable-named devserver sockets, then anything else that looks like a
/// chan control socket. Dead entries cost only a failed connect.
fn socket_candidates() -> Vec<PathBuf> {
    let parent = parent_pid();
    let mut pid_named = Vec::new();
    let mut stable = Vec::new();
    let mut rest = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();

    for dir in socket_dirs() {
        if seen.contains(&dir) {
            continue;
        }
        seen.push(dir.clone());
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rank) = socket_rank(name, parent) else {
                continue;
            };
            let path = entry.path();
            match rank {
                Rank::ParentPid => pid_named.push(path),
                Rank::Stable => stable.push(path),
                Rank::Other => rest.push(path),
            }
        }
    }

    pid_named.sort();
    stable.sort();
    rest.sort();
    pid_named.extend(stable);
    pid_named.extend(rest);
    pid_named
}

/// How promising a candidate is, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rank {
    /// Named for the pid of our parent, which is the serving chan-server.
    ParentPid,
    /// A stable-named devserver socket, which carries no pid to match on.
    Stable,
    /// Some other chan server's socket. Still worth probing, just last.
    Other,
}

/// The part of a chan control socket's file name after the prefix, or `None`
/// when the name is not one.
///
/// The `.sock` suffix is a unix convention. The same socket on Windows is a
/// named pipe under `\\.\pipe\` with no extension at all, which is the rule
/// `chan ps` applies. That directory lists every named pipe on the machine, so
/// the check still has to be exact: a name carrying some other extension is
/// somebody else's pipe, not ours.
fn socket_tail(name: &str) -> Option<&str> {
    let tail = name.strip_prefix("chan-control-")?;
    let tail = match tail.strip_suffix(".sock") {
        Some(stripped) => stripped,
        // An extension-less name is a Windows pipe; on unix it is not ours.
        None if cfg!(windows) => tail,
        None => return None,
    };
    (!tail.is_empty() && !tail.contains('.')).then_some(tail)
}

/// Rank one directory entry, or `None` when it is not a chan control socket.
fn socket_rank(name: &str, parent: Option<u32>) -> Option<Rank> {
    let tail = socket_tail(name)?;
    Some(match parent {
        Some(pid) if tail.starts_with(&format!("{pid}-")) => Rank::ParentPid,
        _ if tail.starts_with('s') => Rank::Stable,
        _ => Rank::Other,
    })
}

/// Where chan keeps its control sockets: a named pipe directory on Windows, a
/// runtime or temp directory on unix.
fn socket_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        return vec![PathBuf::from(r"\\.\pipe\")];
    }
    let mut dirs = Vec::new();
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR")
        && !runtime.trim().is_empty()
    {
        dirs.push(PathBuf::from(runtime));
    }
    dirs.push(std::env::temp_dir());
    dirs.push(PathBuf::from("/tmp"));
    dirs
}

/// Our parent is the serving `chan-server`, so its pid narrows the candidate
/// list. `None` only costs ordering: every candidate is still probed, and the
/// one that answers for our window is the right one either way.
#[cfg(unix)]
fn parent_pid() -> Option<u32> {
    Some(std::os::unix::process::parent_id())
}

#[cfg(not(unix))]
fn parent_pid() -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_wins_over_stdout_when_reporting_a_failure() {
        let out = CsOutput {
            code: 1,
            stdout: "queued".into(),
            stderr: "  no live terminal session matched  ".into(),
        };
        assert_eq!(out.message(), "no live terminal session matched");
        assert!(!out.ok());
    }

    #[test]
    fn a_clean_run_reports_its_stdout() {
        let out = CsOutput {
            code: 0,
            stdout: "queued at position 1\n".into(),
            stderr: String::new(),
        };
        assert_eq!(out.message(), "queued at position 1");
        assert!(out.ok());
    }

    #[test]
    fn candidates_prefer_our_parents_socket_and_skip_unrelated_names() {
        let parent = Some(4242);
        assert_eq!(
            socket_rank("chan-control-4242-deadbeef.sock", parent),
            Some(Rank::ParentPid),
            "our parent's socket is tried first"
        );
        assert_eq!(
            socket_rank("chan-control-s0123456789abcdef.sock", parent),
            Some(Rank::Stable),
            "a stable devserver socket carries no pid, so it is a fallback"
        );
        assert_eq!(
            socket_rank("chan-control-99999999-cafe.sock", parent),
            Some(Rank::Other),
            "another server's socket is still probed, just last"
        );
        for unrelated in ["not-a-chan-socket", "chan-control-broken.txt", "sshd.pipe"] {
            assert_eq!(socket_rank(unrelated, parent), None, "{unrelated}");
        }
    }

    #[test]
    fn without_a_parent_pid_every_socket_is_still_a_candidate() {
        // Windows has no portable parent pid. Losing it costs ordering, never
        // reachability: the probe settles which socket serves our window.
        assert_eq!(
            socket_rank("chan-control-4242-deadbeef.sock", None),
            Some(Rank::Other)
        );
        assert_eq!(
            socket_rank("chan-control-s0123456789abcdef.sock", None),
            Some(Rank::Stable)
        );
    }

    #[test]
    fn socket_dirs_point_at_the_platforms_control_socket_home() {
        let dirs = socket_dirs();
        assert!(!dirs.is_empty());
        if cfg!(windows) {
            assert_eq!(
                dirs,
                vec![PathBuf::from(r"\\.\pipe\")],
                "chan's Windows control sockets are named pipes"
            );
        }
    }

    #[test]
    fn socket_names_are_recognized_with_the_platforms_suffix_rule() {
        // A unix socket carries `.sock`; the same pipe on Windows has no
        // extension, which is the rule `chan ps` applies.
        assert_eq!(socket_tail("chan-control-42-abc.sock"), Some("42-abc"));
        assert_eq!(socket_tail("not-a-chan-socket"), None);
        assert_eq!(
            socket_tail("chan-control-42-abc"),
            cfg!(windows).then_some("42-abc"),
            "an extension-less name is a pipe on Windows and noise on unix"
        );
        // `\\.\pipe\` lists every named pipe on the machine, so a name with
        // some other extension must be rejected on Windows too.
        for foreign in [
            "chan-control-broken.txt",
            "chan-control-42-abc.log",
            "chan-control-.sock",
            "chan-control-",
        ] {
            assert_eq!(socket_tail(foreign), None, "{foreign}");
        }
    }
}
