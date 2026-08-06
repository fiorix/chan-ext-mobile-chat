//! One chat session: spawn an agent, keep talking to it, watch it for trouble.
//!
//! The agent runs as a Chan "team of one". That is not decoration: Chan derives
//! a terminal's submit chord from the PTY's spawn command and `CHAN_AGENT` spawn
//! env, never from what is running inside it. A plain `cs terminal new` tab
//! spawns the tenant shell, so a `claude` started by typing into it stays a
//! shell session and every `cs terminal write --submit=claude` is refused with
//! exit 69, parking the text un-submitted in the agent's compose box (measured,
//! not assumed). A team member's command IS the agent, which fixes the chord and
//! additionally buys Chan's own bracketed-paste readiness gate and the
//! `window_id` binding that `cs terminal survey` needs to find a window.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Mutex, broadcast};

use crate::config::{Agent, Config};
use crate::control::{Cs, EXIT_SUBMIT_REFUSED};

/// The reusable team directory, workspace-relative. `.chan/` is hard-skipped by
/// Chan's workspace walker, indexer, and file watcher, so the scaffolding never
/// shows up in the tree, in search, or in the graph. One stable directory is
/// reused across sessions rather than accumulating one per run.
pub const TEAM_DIR: &str = ".chan/mobile-chat";

/// Terminal group for every spawned agent.
const TAB_GROUP: &str = "mobile-chat";

/// `cs terminal write` refuses anything larger, and truncating a prompt is worse
/// than refusing it.
pub const MAX_WRITE_BYTES: usize = 4096;

/// Substrings that mean the agent command never really started.
const FATAL_MARKERS: &[&str] = &[
    "command not found",
    "not found",
    "No such file or directory",
    "is not recognized",
    "Permission denied",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// No agent picked yet.
    Idle,
    /// `cs terminal team new` is running.
    Spawning,
    /// Spawn returned cleanly; waiting for the tab to appear in the registry.
    Booting,
    /// The tab is in the registry under the expected agent.
    Live,
    /// Queued input is not draining and output has not changed.
    Stalled,
    /// The tab was in the registry and is gone.
    Dead,
    /// The spawn failed, or boot timed out.
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub r#type: &'static str,
    pub phase: Phase,
    pub handle: Option<String>,
    pub agent: Option<String>,
    pub pane: Option<String>,
    pub queue_depth: u64,
    pub idle_secs: u64,
    /// Human-readable cause or last message. Empty when there is nothing to say.
    pub detail: String,
    pub agents: Vec<String>,
}

/// Live state for one chat session.
struct Session {
    handle: String,
    agent: Agent,
    window_id: String,
    pane_id: Option<String>,
    phase: Phase,
    detail: String,
    queue_depth: u64,
    /// Last time the queue depth or the scrollback digest moved.
    last_change: Instant,
    started: Instant,
    digest: u64,
    /// Rolling copy of the scrollback: a session that leaves the registry takes
    /// its scrollback with it, so the post-mortem has to be captured in advance.
    last_scrollback: String,
}

pub struct Chat {
    cs: Arc<Cs>,
    config: Config,
    session: Mutex<Option<Session>>,
    updates: broadcast::Sender<String>,
}

impl Chat {
    pub fn new(cs: Arc<Cs>, config: Config) -> Arc<Self> {
        let (updates, _) = broadcast::channel(32);
        Arc::new(Self {
            cs,
            config,
            session: Mutex::new(None),
            updates,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.updates.subscribe()
    }

    pub fn agent_names(&self) -> Vec<String> {
        self.config.roster().into_iter().map(|a| a.name).collect()
    }

    /// The status the UI renders.
    pub async fn status(&self) -> Status {
        let guard = self.session.lock().await;
        let agents = self.agent_names();
        match guard.as_ref() {
            None => Status {
                r#type: "status",
                phase: Phase::Idle,
                handle: None,
                agent: None,
                pane: None,
                queue_depth: 0,
                idle_secs: 0,
                detail: String::new(),
                agents,
            },
            Some(session) => Status {
                r#type: "status",
                phase: session.phase,
                handle: Some(session.handle.clone()),
                agent: Some(session.agent.name.clone()),
                pane: session.pane_id.clone(),
                queue_depth: session.queue_depth,
                idle_secs: session.last_change.elapsed().as_secs(),
                detail: session.detail.clone(),
                agents,
            },
        }
    }

    async fn publish(&self) {
        if let Ok(text) = serde_json::to_string(&self.status().await) {
            // No subscribers is normal (nobody has the tab open).
            let _ = self.updates.send(text);
        }
    }

    /// Settle the control socket for this window before the user needs it.
    /// Resolution probes every candidate socket, so doing it on connect keeps
    /// that cost off the first click.
    pub async fn hello(&self, window_id: &str) -> Result<String> {
        let out = self.cs.run(window_id, ["pane", "list", "--json"]).await?;
        if !out.ok() {
            anyhow::bail!("{}", out.message());
        }
        Ok(String::new())
    }

    /// Pick an agent and bring it up on side B of the chat tab's pane.
    pub async fn start(self: &Arc<Self>, agent_name: &str, window_id: &str) -> Result<()> {
        let agent = self
            .config
            .roster()
            .into_iter()
            .find(|a| a.name == agent_name)
            .with_context(|| format!("{agent_name:?} is not in the configured agent roster"))?;

        {
            let guard = self.session.lock().await;
            if let Some(existing) = guard.as_ref()
                && matches!(
                    existing.phase,
                    Phase::Spawning | Phase::Booting | Phase::Live | Phase::Stalled
                )
            {
                anyhow::bail!(
                    "{} is already running as {}; close it first",
                    existing.agent.name,
                    existing.handle
                );
            }
        }

        // Check the command resolves BEFORE spawning anything. A command that
        // is not on the login shell's PATH exits 127 immediately, and the
        // registry drops the session before its scrollback can be read, so
        // after the fact all Chan can report is "terminal ended before
        // enabling bracketed-paste mode". Asking first turns that into the
        // real answer and leaves no dead tab behind.
        preflight(&agent.command).await?;

        let handle = format!("@@chat-{}", short_id());
        let pane_id = self.find_own_pane(window_id).await;

        *self.session.lock().await = Some(Session {
            handle: handle.clone(),
            agent: agent.clone(),
            window_id: window_id.to_string(),
            pane_id: pane_id.clone(),
            phase: Phase::Spawning,
            detail: String::new(),
            queue_depth: 0,
            last_change: Instant::now(),
            started: Instant::now(),
            digest: 0,
            last_scrollback: String::new(),
        });
        self.publish().await;

        let scratch = tempfile::tempdir().context("creating the team scratch directory")?;
        let config_path = scratch.path().join("config.toml");
        let brief_path = scratch.path().join("brief.md");
        std::fs::write(&config_path, team_config_toml(&handle, &agent))
            .context("writing the team config")?;
        std::fs::write(&brief_path, brief(&handle)).context("writing the brief")?;

        let mut args = vec![
            "terminal".to_string(),
            "team".to_string(),
            "new".to_string(),
            TEAM_DIR.to_string(),
            "--config".to_string(),
            config_path.display().to_string(),
            "--brief".to_string(),
            brief_path.display().to_string(),
        ];
        if let Some(pane) = &pane_id {
            args.push("--pane".to_string());
            args.push(pane.clone());
        }
        args.push("--side".to_string());
        args.push("b".to_string());

        let out = self.cs.run(window_id, &args).await?;
        if !out.ok() {
            // Chan reports a failed spawn in its own terms ("terminal ended
            // before enabling bracketed-paste mode"), which describes the
            // symptom. The tab is still there holding the shell's actual
            // complaint, so read it and lead with the real cause.
            let cause = cause_suffix(self.captured_cause(window_id, &handle).await);
            self.fail(format!(
                "could not start {}{cause}: {}",
                agent.name,
                out.message()
            ))
            .await;
            return Ok(());
        }

        {
            let mut guard = self.session.lock().await;
            if let Some(session) = guard.as_mut() {
                session.phase = Phase::Booting;
                session.started = Instant::now();
                session.last_change = Instant::now();
            }
        }
        self.publish().await;

        let watcher = Arc::clone(self);
        tokio::spawn(async move { watcher.watch().await });
        Ok(())
    }

    /// Send one user message to the agent, submitted rather than parked.
    pub async fn send(&self, text: &str) -> Result<String> {
        let (handle, agent, window_id) = self.target().await?;
        if text.trim().is_empty() {
            anyhow::bail!("nothing to send");
        }
        if text.len() > MAX_WRITE_BYTES {
            anyhow::bail!(
                "message is {} bytes; the limit is {MAX_WRITE_BYTES}. Write it to a file and send the path instead.",
                text.len()
            );
        }
        let out = self
            .cs
            .run(
                &window_id,
                [
                    "terminal",
                    "write",
                    "--tab-name",
                    &handle,
                    &format!("--submit={}", agent.submit_chord),
                    text,
                ],
            )
            .await?;
        if out.code == EXIT_SUBMIT_REFUSED {
            anyhow::bail!(
                "chan applied no submit chord, so the text is parked un-submitted: {}",
                out.message()
            );
        }
        if !out.ok() {
            anyhow::bail!("{}", out.message());
        }
        Ok(out.message())
    }

    /// Flip the pane to the agent's side.
    pub async fn peek(&self) -> Result<String> {
        let guard = self.session.lock().await;
        let session = guard.as_ref().context("no agent is running")?;
        let window_id = session.window_id.clone();
        let pane = session.pane_id.clone();
        drop(guard);

        let mut args = vec!["pane".to_string(), "focus".to_string()];
        match pane {
            Some(pane) => args.push(pane),
            // `cs pane focus` takes the pane id positionally; without one we
            // cannot name a target, so say so instead of guessing.
            None => anyhow::bail!("this chat tab's pane could not be identified"),
        }
        args.push("--side".to_string());
        args.push("b".to_string());

        let out = self.cs.run(&window_id, &args).await?;
        if !out.ok() {
            anyhow::bail!("{}", out.message());
        }
        Ok(out.message())
    }

    /// Chord-only submit: fires whatever is parked in the agent's compose box.
    pub async fn nudge(&self) -> Result<String> {
        let (handle, agent, window_id) = self.target().await?;
        self.expect_ok(
            &window_id,
            &[
                "terminal".into(),
                "write".into(),
                "--tab-name".into(),
                handle,
                format!("--submit={}", agent.submit_chord),
                String::new(),
            ],
        )
        .await
    }

    /// Raw ESC with no chord, for an agent sitting in a modal it should leave.
    pub async fn escape(&self) -> Result<String> {
        let (handle, _, window_id) = self.target().await?;
        self.expect_ok(
            &window_id,
            &[
                "terminal".into(),
                "write".into(),
                "--tab-name".into(),
                handle,
                "\u{1b}".into(),
            ],
        )
        .await
    }

    /// Respawn the PTY with the same command and env, dropping the write queue.
    /// The only lever that bypasses a wedged queue.
    pub async fn restart(&self) -> Result<String> {
        let (handle, _, window_id) = self.target().await?;
        let result = self
            .expect_ok(
                &window_id,
                &[
                    "terminal".into(),
                    "restart".into(),
                    "--tab-name".into(),
                    handle,
                ],
            )
            .await?;
        let mut guard = self.session.lock().await;
        if let Some(session) = guard.as_mut() {
            session.phase = Phase::Booting;
            session.detail = "restarted".into();
            session.started = Instant::now();
            session.last_change = Instant::now();
            session.queue_depth = 0;
        }
        drop(guard);
        self.publish().await;
        Ok(result)
    }

    /// Close the agent tab and free the session.
    pub async fn close(&self) -> Result<String> {
        let (handle, _, window_id) = self.target().await?;
        let out = self
            .cs
            .run(
                &window_id,
                ["terminal", "close", "--tab-name", &handle, "--force"],
            )
            .await?;
        *self.session.lock().await = None;
        self.publish().await;
        if !out.ok() {
            anyhow::bail!("{}", out.message());
        }
        Ok(out.message())
    }

    async fn expect_ok(&self, window_id: &str, args: &[String]) -> Result<String> {
        let out = self.cs.run(window_id, args).await?;
        if !out.ok() && out.code != EXIT_SUBMIT_REFUSED {
            anyhow::bail!("{}", out.message());
        }
        Ok(out.message())
    }

    async fn target(&self) -> Result<(String, Agent, String)> {
        let guard = self.session.lock().await;
        let session = guard.as_ref().context("no agent is running")?;
        Ok((
            session.handle.clone(),
            session.agent.clone(),
            session.window_id.clone(),
        ))
    }

    /// Read a just-failed tab's scrollback and reduce it to a cause, if it
    /// names one. Best effort: a tab that is already gone yields nothing.
    async fn captured_cause(&self, window_id: &str, handle: &str) -> Option<String> {
        self.cs
            .run(window_id, ["terminal", "scrollback", "--tab-name", handle])
            .await
            .ok()
            .filter(|out| out.ok())
            .and_then(|out| post_mortem(&out.stdout))
    }

    async fn fail(&self, detail: String) {
        let mut guard = self.session.lock().await;
        if let Some(session) = guard.as_mut() {
            session.phase = Phase::Failed;
            session.detail = detail;
        }
        drop(guard);
        self.publish().await;
    }

    /// Locate the pane holding this extension's tab, so the agent lands on the
    /// same pane's other side. Falls back to the window's active pane.
    async fn find_own_pane(&self, window_id: &str) -> Option<String> {
        let out = self
            .cs
            .run(window_id, ["pane", "list", "--json"])
            .await
            .ok()?;
        if !out.ok() {
            return None;
        }
        let layout: Value = serde_json::from_str(&out.stdout).ok()?;
        own_pane_from_layout(&layout)
    }

    /// The health loop. Runs while a session exists.
    async fn watch(self: Arc<Self>) {
        let interval = Duration::from_secs(self.config.health.poll_interval_secs.max(1));
        let boot_timeout = Duration::from_secs(self.config.health.boot_timeout_secs.max(1));
        let stall_after = Duration::from_secs(self.config.health.stall_after_secs.max(1));

        loop {
            tokio::time::sleep(interval).await;

            let Some((handle, agent, window_id, phase)) = ({
                let guard = self.session.lock().await;
                guard.as_ref().map(|s| {
                    (
                        s.handle.clone(),
                        s.agent.clone(),
                        s.window_id.clone(),
                        s.phase,
                    )
                })
            }) else {
                return;
            };
            if matches!(phase, Phase::Failed | Phase::Dead) {
                return;
            }

            let listed = self
                .cs
                .run(&window_id, ["terminal", "list", "--json"])
                .await;
            let entry = listed
                .as_ref()
                .ok()
                .filter(|out| out.ok())
                .and_then(|out| serde_json::from_str::<Value>(&out.stdout).ok())
                .and_then(|value| find_session(&value, &handle));

            let scrollback = self
                .cs
                .run(
                    &window_id,
                    ["terminal", "scrollback", "--tab-name", &handle],
                )
                .await
                .ok()
                .filter(|out| out.ok())
                .map(|out| out.stdout);

            let mut guard = self.session.lock().await;
            let Some(session) = guard.as_mut() else {
                return;
            };

            if let Some(text) = scrollback {
                let digest = fnv1a(&text);
                if digest != session.digest {
                    session.digest = digest;
                    session.last_change = Instant::now();
                }
                session.last_scrollback = text;
            }

            match entry {
                Some(entry) => {
                    let depth = entry
                        .get("queue_depth")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if depth != session.queue_depth {
                        session.queue_depth = depth;
                        session.last_change = Instant::now();
                    }
                    let listed_agent = entry.get("agent").and_then(Value::as_str);
                    if session.phase == Phase::Booting {
                        session.phase = Phase::Live;
                        session.detail = match listed_agent {
                            Some(name) => format!("{name} is up"),
                            None => "up, but chan derived no agent for this tab".into(),
                        };
                        session.last_change = Instant::now();
                    } else if depth > 0 && session.last_change.elapsed() >= stall_after {
                        session.phase = Phase::Stalled;
                        session.detail = format!(
                            "{depth} message(s) queued and nothing has moved for {}s",
                            session.last_change.elapsed().as_secs()
                        );
                    } else if session.phase == Phase::Stalled && depth == 0 {
                        session.phase = Phase::Live;
                        session.detail = "recovered".into();
                    }
                }
                None if session.phase == Phase::Booting => {
                    if session.started.elapsed() >= boot_timeout {
                        session.phase = Phase::Failed;
                        session.detail = format!(
                            "{} did not start within {}s{}",
                            agent.name,
                            boot_timeout.as_secs(),
                            cause_suffix(post_mortem(&session.last_scrollback))
                        );
                    }
                }
                None => {
                    session.phase = Phase::Dead;
                    session.detail = format!(
                        "{} exited{}",
                        agent.name,
                        cause_suffix(post_mortem(&session.last_scrollback))
                    );
                }
            }

            let done = matches!(session.phase, Phase::Failed | Phase::Dead);
            drop(guard);
            self.publish().await;
            if done {
                return;
            }
        }
    }
}

/// Find the pane whose side A holds this extension's tab. Falls back to the
/// active pane, which is where a fresh terminal would land anyway.
fn own_pane_from_layout(layout: &Value) -> Option<String> {
    let panes = layout.get("panes")?.as_array()?;
    for pane in panes {
        for side in ["a", "b"] {
            let tabs = pane
                .get("sides")
                .and_then(|s| s.get(side))
                .and_then(|s| s.get("tabs"))
                .and_then(Value::as_array);
            let Some(tabs) = tabs else { continue };
            if tabs
                .iter()
                .any(|tab| tab.get("kind").and_then(Value::as_str) == Some("extension"))
            {
                return pane.get("id").and_then(Value::as_str).map(str::to_string);
            }
        }
    }
    layout
        .get("activePaneId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Pull one session out of `cs terminal list --json` by tab name.
fn find_session(listed: &Value, handle: &str) -> Option<Value> {
    let groups = listed.get("groups")?.as_object()?;
    for sessions in groups.values() {
        for session in sessions.as_array()? {
            if session.get("name").and_then(Value::as_str) == Some(handle) {
                return Some(session.clone());
            }
        }
    }
    None
}

/// Refuse a command the login shell cannot resolve, before anything is spawned.
///
/// Chan runs a member command through `$SHELL -lc`, so this asks the same shell
/// the same question. Only the leading word is checked, and only when it is a
/// plain program name or path: anything with shell syntax in it is left alone
/// rather than guessed at, so this can add a clear error but never block a
/// command that would have worked.
async fn preflight(command: &str) -> Result<()> {
    // Windows: Chan picks between PowerShell, cmd, and a POSIX shell at
    // runtime, so there is no single question to ask here. Skipping leaves the
    // generic failure report rather than risking a wrong refusal.
    if cfg!(windows) {
        return Ok(());
    }
    let Some(program) = checkable_program(command) else {
        return Ok(());
    };
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    let resolved = tokio::process::Command::new(&shell)
        .args(["-lc", &format!("command -v -- {program}")])
        .output()
        .await
        .with_context(|| format!("asking {shell} to resolve {program:?}"))?;
    if resolved.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "{program:?} is not on the login shell's PATH, so the terminal would exit \
         immediately. Point at it with an absolute path in the agent's `command`."
    )
}

/// The leading word of `command`, when checking it is meaningful. Returns
/// `None` for anything carrying shell syntax, an environment assignment, or
/// quoting, because the leading word is then not the program.
fn checkable_program(command: &str) -> Option<&str> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    if command.contains(['|', '&', ';', '<', '>', '(', ')', '`', '$', '\'', '"', '\\']) {
        return None;
    }
    let first = command.split_whitespace().next()?;
    (!first.contains('=')).then_some(first)
}

/// Turn a captured scrollback into a one-line cause, when it names one.
fn post_mortem(scrollback: &str) -> Option<String> {
    let stripped = strip_ansi(scrollback);
    stripped
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| FATAL_MARKERS.iter().any(|marker| line.contains(marker)))
        .map(|line| line.chars().take(200).collect())
}

/// Render a cause as a parenthetical suffix, or nothing when there is none.
fn cause_suffix(cause: Option<String>) -> String {
    cause.map(|cause| format!(" ({cause})")).unwrap_or_default()
}

/// Drop CSI/OSC escape sequences so a captured error line is readable. Enough
/// for grepping a shell's failure message; not a terminal emulator.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            if !ch.is_control() || ch == '\n' || ch == '\t' {
                out.push(ch);
            }
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte.
            Some('[') => {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() || next == '~' {
                        break;
                    }
                }
            }
            // OSC: runs until BEL or ST.
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// FNV-1a over the scrollback, used only to notice that output changed.
fn fnv1a(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

fn short_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 3];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A Chan team config with exactly one member. Chan requires 1 to 9 members and
/// exactly one lead, so the solo agent is the lead.
fn team_config_toml(handle: &str, agent: &Agent) -> String {
    format!(
        "team_name = \"mobile-chat\"\n\
         host_name = \"You\"\n\
         host_handle = \"@@You\"\n\
         tab_group = \"{TAB_GROUP}\"\n\
         \n\
         [[members]]\n\
         handle = \"{handle}\"\n\
         command = \"{command}\"\n\
         is_lead = true\n\
         env = {{ CHAN_AGENT = \"{submit}\" }}\n",
        command = toml_escape(&agent.command),
        submit = toml_escape(&agent.submit_chord),
        handle = toml_escape(handle),
    )
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The brief Chan folds verbatim into the team's `bootstrap.md`. It tells the
/// agent the one thing it cannot observe: nobody is watching its terminal.
fn brief(handle: &str) -> String {
    format!(
        "You are talking to a person on a phone through Chan's Mobile Chat extension.\n\
         \n\
         Your terminal is side B of a pane. They are looking at side A, a chat box, and they cannot see anything you print unless they deliberately flip to your side. Assume they will not.\n\
         \n\
         Their messages arrive as ordinary prompts. Everything you want to say back has to go out as a survey:\n\
         \n\
         ```\n\
         cs terminal survey --tab-name {handle} --title \"<short question>\" --option \"<label>\" --option \"<label>\" '<markdown body>'\n\
         ```\n\
         \n\
         That opens a blocking overlay on their phone with tappable buttons and prints their choice on stdout. Rules that matter:\n\
         \n\
         - 1 to 4 options. Keep labels to a couple of words.\n\
         - It blocks for up to 600 seconds, then exits 124 with no answer. That is a person not looking at their phone, not an error.\n\
         - \"host will follow up later\" means they deferred. Stop and wait rather than asking again.\n\
         - To deliver a finished answer that needs no decision, still send a survey with a single `OK` option. The survey is your only way to reach someone who is not watching the terminal.\n\
         \n\
         Put the substance in the markdown body, not in the title. Keep your terminal output short: it is a log nobody reads, not your reply.\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> Agent {
        Agent {
            name: "claude".into(),
            command: "claude".into(),
            submit_chord: "claude".into(),
        }
    }

    #[test]
    fn the_team_config_satisfies_chans_validation() {
        let toml_text = team_config_toml("@@chat-a1b2c3", &agent());
        let parsed: toml::Value = toml::from_str(&toml_text).expect("valid toml");
        let members = parsed["members"].as_array().expect("members array");
        assert_eq!(members.len(), 1, "chan requires 1 to 9 members");
        assert_eq!(
            members
                .iter()
                .filter(|m| m["is_lead"].as_bool() == Some(true))
                .count(),
            1,
            "chan requires exactly one lead"
        );
        assert_eq!(members[0]["handle"].as_str(), Some("@@chat-a1b2c3"));
        assert_eq!(members[0]["command"].as_str(), Some("claude"));
        assert_eq!(members[0]["env"]["CHAN_AGENT"].as_str(), Some("claude"));
        for key in ["team_name", "host_name", "host_handle"] {
            assert!(
                !parsed[key].as_str().unwrap_or_default().is_empty(),
                "{key} must be non-empty"
            );
        }
    }

    #[test]
    fn a_wrapper_command_still_pins_the_chord_through_chan_agent() {
        let wrapper = Agent {
            name: "mine".into(),
            command: "./my \"agent\".sh".into(),
            submit_chord: "claude".into(),
        };
        let parsed: toml::Value = toml::from_str(&team_config_toml("@@chat-1", &wrapper)).unwrap();
        assert_eq!(
            parsed["members"][0]["command"].as_str(),
            Some("./my \"agent\".sh"),
            "quotes in a command survive escaping"
        );
        assert_eq!(
            parsed["members"][0]["env"]["CHAN_AGENT"].as_str(),
            Some("claude")
        );
    }

    #[test]
    fn the_brief_names_the_handle_and_the_survey_contract() {
        let text = brief("@@chat-a1b2c3");
        assert!(text.contains("cs terminal survey --tab-name @@chat-a1b2c3"));
        assert!(text.contains("1 to 4 options"));
        assert!(text.contains("single `OK` option"));
    }

    #[test]
    fn the_extension_tab_identifies_its_own_pane() {
        let layout = serde_json::json!({
            "activePaneId": "pane-1",
            "panes": [
                {"id": "pane-1", "sides": {
                    "a": {"tabs": [{"id": "term-2", "kind": "terminal", "title": "Terminal-1"}]},
                    "b": {"tabs": []}}},
                {"id": "pane-2", "sides": {
                    "a": {"tabs": [{"id": "ext-1", "kind": "extension", "title": "Mobile Chat"}]},
                    "b": {"tabs": []}}}
            ]
        });
        assert_eq!(own_pane_from_layout(&layout).as_deref(), Some("pane-2"));
    }

    #[test]
    fn a_layout_without_our_tab_falls_back_to_the_active_pane() {
        let layout = serde_json::json!({
            "activePaneId": "pane-7",
            "panes": [{"id": "pane-7", "sides": {"a": {"tabs": []}, "b": {"tabs": []}}}]
        });
        assert_eq!(own_pane_from_layout(&layout).as_deref(), Some("pane-7"));
    }

    #[test]
    fn a_listed_session_is_found_by_tab_name_across_groups() {
        let listed = serde_json::json!({
            "groups": {
                "default": [{"name": "Terminal-1", "queue_depth": 0, "agent": null}],
                "mobile-chat": [{"name": "@@chat-a1", "queue_depth": 3, "agent": "claude"}]
            }
        });
        let found = find_session(&listed, "@@chat-a1").expect("found");
        assert_eq!(found["queue_depth"].as_u64(), Some(3));
        assert_eq!(found["agent"].as_str(), Some("claude"));
        assert!(find_session(&listed, "@@chat-nope").is_none());
    }

    #[test]
    fn a_post_mortem_pulls_the_failing_line_out_of_ansi_noise() {
        // The real shape of a missing agent: chan reports only that the
        // terminal ended, so the actual reason has to come from the ring.
        let scrollback = "\u{1b}[2J\u{1b}[Hsome banner\r\n\u{1b}[31m/bin/bash: kimi: command not found\u{1b}[0m\r\nprocess exited (127)\r\n";
        let cause = post_mortem(scrollback).expect("names a cause");
        assert!(cause.contains("kimi: command not found"), "{cause}");
        assert!(!cause.contains('\u{1b}'), "escapes are stripped");
        assert_eq!(
            cause_suffix(Some(cause)),
            " (/bin/bash: kimi: command not found)"
        );
    }

    #[test]
    fn preflight_checks_a_plain_program_and_leaves_shell_syntax_alone() {
        assert_eq!(checkable_program("claude"), Some("claude"));
        assert_eq!(
            checkable_program("  /opt/bin/kimi  "),
            Some("/opt/bin/kimi")
        );
        assert_eq!(
            checkable_program("my-shell-script --flag"),
            Some("my-shell-script"),
            "arguments do not change which word is the program"
        );
        // Anything where the leading word is not the program is left alone, so
        // the check can never reject a command that would have worked.
        for command in [
            "FOO=1 claude",
            "claude | tee log",
            "cd /tmp && claude",
            "sh -c 'claude'",
            "$MY_AGENT",
            "",
        ] {
            assert_eq!(checkable_program(command), None, "{command:?}");
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn preflight_names_the_missing_command_and_passes_a_real_one() {
        let error = preflight("/nonexistent/place/kimi")
            .await
            .expect_err("a missing command is refused");
        let error = error.to_string();
        assert!(error.contains("/nonexistent/place/kimi"), "{error}");
        assert!(
            error.contains("absolute path"),
            "guidance included: {error}"
        );

        preflight("sh").await.expect("sh resolves everywhere");
        preflight("cd /tmp && whatever")
            .await
            .expect("shell syntax is not second-guessed");
    }

    /// Chan resolves PowerShell, cmd, or a POSIX shell at runtime on Windows,
    /// so there is no single question to ask. Never refuse on a guess.
    #[cfg(windows)]
    #[tokio::test]
    async fn preflight_is_a_no_op_where_the_shell_is_not_knowable() {
        preflight("definitely not a real program").await.unwrap();
    }

    #[test]
    fn a_healthy_scrollback_yields_no_cause() {
        assert_eq!(post_mortem("\u{1b}[32mall good\u{1b}[0m\n"), None);
        assert_eq!(cause_suffix(None), "");
    }

    #[test]
    fn the_digest_moves_only_when_the_output_does() {
        assert_eq!(fnv1a("same"), fnv1a("same"));
        assert_ne!(fnv1a("same"), fnv1a("different"));
    }

    #[test]
    fn handles_are_unique_enough_to_avoid_write_fan_out() {
        // Tab names are not unique in chan and `cs terminal write` fans out to
        // every holder, so two sessions must not collide.
        let ids: std::collections::HashSet<_> = (0..64).map(|_| short_id()).collect();
        assert!(ids.len() > 60, "short ids should rarely repeat");
    }
}
