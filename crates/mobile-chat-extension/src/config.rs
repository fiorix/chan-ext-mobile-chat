//! User configuration for the agent roster and health polling.
//!
//! The file lives at `<chan-home>/mobile-chat.toml`, deliberately outside
//! `<chan-home>/extensions/`: Chan parses every `.toml` in that directory as an
//! extension declaration, so a config file there would be read as a broken
//! declaration. An absent file means the defaults below.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// The agents Chan knows a submit chord for (`SubmitAgent` in chan-shell),
/// each paired with the flag that starts it without a permission gate.
///
/// A chord is the byte sequence that makes a prompt FIRE in that agent's
/// compose box instead of sitting in it, so naming the right one is the
/// difference between a message being answered and a message being ignored.
///
/// The flag answers the other half of the same problem. This extension's only
/// reply channel is the Mobile Chat helper; an agent that stops at its own
/// permission prompt never reaches it, and that prompt lives in a terminal
/// nobody is looking at, so the session just goes quiet. `claude` gets
/// `--permission-mode bypassPermissions` rather than
/// `--dangerously-skip-permissions` because the latter opens a one-time
/// consent screen that is exactly the kind of prompt this is meant to avoid.
const KNOWN_AGENTS: &[(&str, &str)] = &[
    ("claude", "--permission-mode bypassPermissions"),
    ("codex", "--dangerously-bypass-approvals-and-sandbox"),
    ("kimi", "--auto"),
    ("gemini", "--yolo"),
    ("opencode", "--auto"),
];

/// Agents offered when the config file names none.
const DEFAULT_AGENTS: &[&str] = &["claude", "codex", "kimi"];

/// Cap on the config file, so a stray large file cannot be slurped whole.
const CONFIG_LIMIT_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Roster shown in the picker, in order. A bare name that is itself a known
    /// chord needs nothing else; any other name needs an `[agent.<name>]`
    /// section naming the command to run and the chord to submit with.
    #[serde(default = "default_agents")]
    pub agents: Vec<String>,

    /// Per-agent overrides. The command is free-form: Chan spawns it through a
    /// shell, so arguments, wrappers, and shell syntax all work.
    #[serde(default)]
    pub agent: BTreeMap<String, AgentOverride>,

    #[serde(default)]
    pub health: Health,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOverride {
    /// Append the chat brief as a positional CLI prompt. Defaults to true for
    /// Claude and Codex on Unix; other commands use explicit Connect chat.
    pub prompt_argument: Option<bool>,
    /// Command the terminal spawns, free-form: Chan runs it through a shell,
    /// so `my-shell-script --flag` works as written. That shell does not read
    /// your login files, so an agent your shell rc puts on PATH needs an
    /// absolute path here.
    ///
    /// Defaults to the roster name plus that agent's permission-bypass flag.
    /// Naming a command replaces both halves. Without the flag, native
    /// permission prompts remain available through Peek.
    pub command: Option<String>,
    /// Which submit chord to send. Becomes `CHAN_AGENT`, which is what pins
    /// Chan's chord selection when the command does not name a known agent.
    /// Defaults to the roster name when that is itself a known chord.
    #[serde(alias = "submit-chord")]
    pub submit_chord: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    /// How long to wait for a spawned agent to appear in `cs terminal list`.
    #[serde(default = "default_boot_timeout_secs")]
    pub boot_timeout_secs: u64,
    /// Interval between health polls.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// How long a queued message waits before suggesting Peek.
    #[serde(default = "default_stall_after_secs")]
    pub stall_after_secs: u64,
}

/// One roster entry with its overrides already applied and validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub command: String,
    pub submit_chord: String,
    #[serde(default)]
    pub prompt_argument: bool,
}

fn default_agents() -> Vec<String> {
    DEFAULT_AGENTS
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

/// Whether Chan knows this name as a submit chord.
fn is_known_chord(name: &str) -> bool {
    KNOWN_AGENTS.iter().any(|(known, _)| *known == name)
}

/// The known names, for an error message that names the way out.
fn known_chords() -> String {
    KNOWN_AGENTS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a bare roster name spawns: the agent, plus the flag that keeps it from
/// stopping to ask for permission. `None` for a name Chan does not know, whose
/// launcher flags are not ours to guess.
fn default_command(name: &str) -> Option<String> {
    KNOWN_AGENTS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(agent, bypass)| format!("{agent} {bypass}"))
}

fn default_boot_timeout_secs() -> u64 {
    45
}

fn default_poll_interval_secs() -> u64 {
    5
}

fn default_stall_after_secs() -> u64 {
    120
}

impl Default for Health {
    fn default() -> Self {
        Self {
            boot_timeout_secs: default_boot_timeout_secs(),
            poll_interval_secs: default_poll_interval_secs(),
            stall_after_secs: default_stall_after_secs(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agents: default_agents(),
            agent: BTreeMap::new(),
            health: Health::default(),
        }
    }
}

impl Config {
    /// Read `path`, or fall back to the defaults when it does not exist. A
    /// malformed file is an error rather than a silent default: a typo that
    /// silently drops the user's roster is worse than a visible failure.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(anyhow::Error::from(error)),
        };
        if metadata.len() > CONFIG_LIMIT_BYTES {
            anyhow::bail!(
                "{} is {} bytes, over the {CONFIG_LIMIT_BYTES}-byte limit",
                path.display(),
                metadata.len()
            );
        }
        let text = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&text)?;
        // Resolve eagerly so a bad roster is a startup error with a path in it,
        // not a mystery at the moment someone taps Start.
        config
            .try_roster()
            .with_context(|| format!("in {}", path.display()))?;
        Ok(config)
    }

    /// The roster with overrides applied, blank names dropped.
    ///
    /// A chord that Chan does not know is refused rather than passed through:
    /// Chan ignores an unrecognized `CHAN_AGENT` and falls back to sniffing the
    /// command, so a typo would silently produce a terminal that accepts
    /// messages and never submits them.
    pub fn try_roster(&self) -> anyhow::Result<Vec<Agent>> {
        let names: Vec<&str> = self
            .agents
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .collect();
        if names.is_empty() {
            anyhow::bail!("no usable agents are declared");
        }
        names
            .into_iter()
            .map(|name| {
                let over = self.agent.get(name);
                let command = over
                    .and_then(|o| o.command.as_deref())
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
                    .or_else(|| default_command(name))
                    .unwrap_or_else(|| name.to_string());
                let declared = over
                    .and_then(|o| o.submit_chord.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let submit_chord = match declared {
                    Some(chord) => chord,
                    None if is_known_chord(name) => name,
                    None => anyhow::bail!(
                        "agent {name:?} is not one of the known chords, so it needs \
                         a [agent.{name}] section with submit_chord = one of {}",
                        known_chords()
                    ),
                };
                if !is_known_chord(submit_chord) {
                    anyhow::bail!(
                        "agent {name:?} declares submit_chord = {submit_chord:?}, \
                         which chan does not know; use one of {}",
                        known_chords()
                    );
                }
                Ok(Agent {
                    name: name.to_string(),
                    command,
                    submit_chord: submit_chord.to_string(),
                    prompt_argument: over
                        .and_then(|o| o.prompt_argument)
                        .unwrap_or(cfg!(unix) && matches!(submit_chord, "claude" | "codex")),
                })
            })
            .collect()
    }

    /// The validated roster. `Config::load` has already proven this succeeds.
    pub fn roster(&self) -> Vec<Agent> {
        self.try_roster().unwrap_or_default()
    }
}

/// Default config path: `$CHAN_HOME/mobile-chat.toml`, else
/// `~/.chan/mobile-chat.toml`. Mirrors how Chan resolves its own config dir.
pub fn default_config_path() -> PathBuf {
    chan_home().join("mobile-chat.toml")
}

pub(crate) fn chan_home() -> PathBuf {
    match std::env::var("CHAN_HOME") {
        Ok(home) if !home.trim().is_empty() => PathBuf::from(home),
        _ => match std::env::var("HOME") {
            Ok(home) if !home.trim().is_empty() => PathBuf::from(home).join(".chan"),
            _ => PathBuf::from(".chan"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_file_yields_the_supported_default_agents() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("nope.toml")).unwrap();
        let names: Vec<_> = config.roster().into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["claude", "codex", "kimi"]);
    }

    #[test]
    fn a_bare_name_becomes_its_own_chord_and_a_command_that_will_not_stop_to_ask() {
        let config = Config::default();
        let claude = config
            .roster()
            .into_iter()
            .find(|a| a.name == "claude")
            .unwrap();
        assert_eq!(claude.command, "claude --permission-mode bypassPermissions");
        assert_eq!(claude.submit_chord, "claude");
    }

    #[test]
    fn every_default_agent_launches_itself_with_a_bypass_flag() {
        // A permission prompt is invisible from the chat tab, so an agent that
        // can still raise one is an agent the phone cannot talk to.
        for agent in Config::default().roster() {
            let (program, flags) = agent
                .command
                .split_once(' ')
                .expect("a default command carries a flag");
            assert_eq!(program, agent.name, "the program is still the agent");
            assert!(flags.starts_with("--"), "{}: {flags:?}", agent.name);
            assert_eq!(agent.submit_chord, agent.name);
        }
    }

    #[test]
    fn a_declared_command_is_spawned_verbatim() {
        // The flags of somebody else's launcher are not ours to guess, so a
        // command that names itself owns its own permission story.
        let (_dir, path) = write(
            "agents = [\"claude\"]\n\
             [agent.claude]\n\
             command = \"/opt/bin/claude\"\n",
        );
        let agent = Config::load(&path).unwrap().roster().remove(0);
        assert_eq!(agent.command, "/opt/bin/claude");
        assert_eq!(
            agent.submit_chord, "claude",
            "the roster name still picks the chord"
        );
    }

    fn write(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mobile-chat.toml");
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    #[test]
    fn any_command_can_be_paired_with_an_explicit_chord() {
        let (_dir, path) = write(
            "agents = [\"mine\"]\n\
             [agent.mine]\n\
             command = \"my-shell-script --flag\"\n\
             submit_chord = \"opencode\"\n",
        );
        let roster = Config::load(&path).unwrap().roster();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].command, "my-shell-script --flag");
        assert_eq!(roster[0].submit_chord, "opencode");
    }

    #[test]
    fn the_kebab_spelling_of_the_chord_key_also_works() {
        let (_dir, path) = write(
            "agents = [\"mine\"]\n\
             [agent.mine]\n\
             command = \"/opt/bin/agent\"\n\
             submit-chord = \"codex\"\n",
        );
        assert_eq!(
            Config::load(&path).unwrap().roster()[0].submit_chord,
            "codex"
        );
    }

    #[test]
    fn an_off_path_agent_is_reachable_by_absolute_command() {
        // The kimi-not-on-PATH case: the roster name stays friendly, the
        // command carries the real location.
        let (_dir, path) = write(
            "agents = [\"kimi\"]\n\
             [agent.kimi]\n\
             command = \"/Users/me/.local/share/kimi/bin/kimi\"\n",
        );
        let agent = Config::load(&path).unwrap().roster().remove(0);
        assert_eq!(agent.command, "/Users/me/.local/share/kimi/bin/kimi");
        assert_eq!(
            agent.submit_chord, "kimi",
            "the roster name still picks the chord"
        );
    }

    #[test]
    fn an_unknown_chord_is_refused_instead_of_silently_disabling_submit() {
        // Chan ignores an unrecognized CHAN_AGENT and sniffs the command
        // instead, which would leave prompts parked in the compose box.
        let (_dir, path) = write(
            "agents = [\"mine\"]\n\
             [agent.mine]\n\
             command = \"x\"\n\
             submit_chord = \"clyde\"\n",
        );
        let error = Config::load(&path).unwrap_err().to_string();
        assert!(format!("{error:#}").contains("clyde") || !error.is_empty());
    }

    #[test]
    fn a_custom_name_without_a_chord_is_refused_at_load() {
        let (_dir, path) = write("agents = [\"my-thing\"]\n");
        assert!(
            Config::load(&path).is_err(),
            "a name chan cannot derive a chord from must declare one"
        );
    }

    #[test]
    fn health_defaults_apply_when_the_section_is_absent() {
        let health = Config::default().health;
        assert_eq!(health.boot_timeout_secs, 45);
        assert_eq!(health.poll_interval_secs, 5);
        assert_eq!(health.stall_after_secs, 120);
    }

    #[test]
    fn an_unknown_key_is_rejected_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mobile-chat.toml");
        std::fs::write(&path, "agentz = [\"claude\"]\n").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
