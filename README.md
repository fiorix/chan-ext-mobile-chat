# Mobile Chat

A [Chan](https://github.com/fiorix/chan) extension that turns an agent CLI into a chat box.

Pick an agent from a dropdown. The extension spawns it in the same pane's B side, and the tab you are looking at becomes the place you type. The agent answers by opening a survey: a blocking overlay with tappable buttons, which is the only channel that reaches someone who is not watching a terminal. On a phone that is the whole point, because driving a full-screen TUI with a soft keyboard is not a thing anyone wants to do.

```
 pane, side A                      pane, side B
 +----------------------+  flip   +----------------------+
 | [Mobile Chat]        | <-----> | [@@chat-a1b2c3]      |
 |  claude is up        |         |  the agent's TUI     |
 |  queue 0 - quiet 3s  |         |                      |
 |  [ type here      ]  |         |                      |
 +----------------------+         +----------------------+
```

The extension owns very little. Chan already knows how to spawn an agent with the right submit chord, deliver a prompt so it fires instead of parking, ask a human a question, and report a terminal's health. Mobile Chat is the phone-shaped front door onto that, plus the one part Chan leaves to its caller: waiting until the agent is actually listening before saying anything to it.

## Install

Needs Chan v0.86.0 or newer, which is where `cs terminal new` learned `--command` and `--env`; that pair is how the agent becomes the terminal's own spawn command. Chan discovers local extensions at `~/.chan/extensions`. Install the latest release with:

```sh
curl -fsSL https://github.com/fiorix/chan-ext-mobile-chat/releases/latest/download/install.sh | bash
```

The installer detects Linux x86_64 or arm64, macOS arm64, and Windows x86_64 under Git Bash. It verifies the archive against the release checksums before extracting anything, writes the executable under `~/.local/lib/mobile-chat`, and writes the declaration at `~/.chan/extensions/mobile-chat.toml` with an absolute command path.

Pin a release with `curl -fsSL ... | MOBILE_CHAT_VERSION=v0.1.0 bash`, or override the roots with `MOBILE_CHAT_INSTALL_ROOT` and `CHAN_HOME`.

Then restart Chan, and "Mobile Chat" appears in the command launcher under Apps. If it does not, the declaration was rejected: check Chan's stderr for `extension ignored`.

To build from a checkout instead, run `./scripts/install-chan-extension.sh`.

Releases ship native binaries for four targets, each compiled and tested on its own GitHub-hosted runner. The Linux builds target musl, so they carry no libc dependency.

| Platform | Archive |
|---|---|
| Linux x86_64 | `mobile-chat-linux-x86_64.tar.gz` |
| Linux aarch64 | `mobile-chat-linux-aarch64.tar.gz` |
| macOS aarch64 | `mobile-chat-macos-aarch64.tar.gz` |
| Windows x86_64 | `mobile-chat-windows-x86_64.zip` |

## Configuration

Optional, at `<chan-home>/mobile-chat.toml`. With no file you get the five agents Chan knows a submit chord for: claude, codex, kimi, gemini, opencode. Each is launched in its no-prompt mode; see [Permissions](#permissions).

```toml
# The picker, in order. The first entry is the default.
agents = ["claude", "codex", "kimi", "work-agent"]

# An agent your shell rc puts on PATH, which the spawn does not read.
[agent.kimi]
command = "/Users/me/.local/share/kimi/bin/kimi --auto"

# Any command at all, with the chord named explicitly.
[agent.work-agent]
command = "my-shell-script --profile work"
submit_chord = "opencode"

[health]
boot_timeout_secs = 45     # how long it has to come up and take its brief
poll_interval_secs = 5     # how often to check on it
stall_after_secs = 120     # queued and quiet this long means stuck
```

`command` is free-form: Chan spawns it through a shell, so arguments, wrappers, and shell syntax all work. That shell does not read your login files, so an agent only your shell rc puts on `PATH` needs an absolute path, which is what the `kimi` example above is doing. Naming a command also replaces the default's permission flag.

`submit_chord` (`submit-chord` also works) picks which chord submits your message. It must be one of the five Chan knows, because that name becomes `CHAN_AGENT`, and **Chan silently ignores a value it does not recognize** and goes back to guessing from the command. A guess that comes up empty produces a terminal that accepts messages and never submits them, so the config refuses an unknown chord at startup instead. A roster name that is not itself a known chord must declare one.

The config is read once, at Chan startup. Editing it means restarting Chan.

## Permissions

Every agent is launched in its no-prompt mode. A permission prompt renders inside the agent's TUI, which is the one place the person holding the phone is not looking, so an agent that stops to ask is an agent that has gone silent.

| Agent | Launched as |
|---|---|
| claude | `claude --permission-mode bypassPermissions` |
| codex | `codex --dangerously-bypass-approvals-and-sandbox` |
| kimi | `kimi --auto` |
| gemini | `gemini --yolo` |
| opencode | `opencode --auto` |

claude gets `--permission-mode bypassPermissions` rather than `--dangerously-skip-permissions` because the latter opens a one-time consent screen, which is precisely the prompt this is meant to avoid.

What replaces the permission check is the brief. The agent is told that it is running unattended with the checks off, that the survey is therefore its own approval gate, and which side of the line an action falls on: irreversible or outward-facing work is surveyed first (`git push`, rewriting history, deleting work, anything outside the workspace, installing packages, changing credentials, spending money, anything other people will see), while reading, searching, building, testing, and workspace edits git can undo are not. It is also told that not knowing which of the two an action is counts as a reason to ask.

That is a prompt, not a sandbox. It is the agent's judgement doing the work that a permission dialog used to do, so point this at a workspace you would let an agent loose in.

## How it works

Chan spawns the extension as a subprocess and reverse-proxies its loopback server into a sandboxed, opaque-origin iframe. From there:

- **Starting an agent** runs `cs terminal new --tab-name <handle> --command <agent> --env CHAN_AGENT=<chord>` on side B of the chat tab's pane. Making the agent the tab's own spawn command is not incidental. Chan derives a terminal's submit chord from the PTY's **spawn command** and its `CHAN_AGENT` spawn env, never from whatever is running inside it. A tab spawned as a shell stays a shell session, so a `claude` started by typing into one never earns a chord: every `cs terminal write --submit=claude` is refused with exit 69 and the text parks un-submitted in the compose box.
- **The brief** is the agent's first prompt, not a file it is pointed at. It goes in once the tab's scrollback shows bracketed-paste mode turning on, which is the same readiness signal Chan's own team spawn waits for before poking a member, and which a plain `cs terminal new` does not wait for on anyone's behalf. Until the brief lands the session stays `booting` and the composer will not send, because a message that overtook the brief would reach an agent that does not yet know the only way to answer it.
- **Your message** goes out as `cs terminal write --tab-name <handle> --submit=<chord>`, capped at 4096 bytes because that is Chan's limit and truncating a prompt is worse than refusing it.
- **The agent's reply** comes back as `cs terminal survey`, which the brief tells it to use for questions *and* for finished answers. Chan renders it as a blocking overlay in the window that owns the terminal.
- **Peek** runs `cs pane focus <pane> --side b` to flip you to the agent. The pane's own side toggle flips back.

Everything the extension does to Chan goes through the `cs` client, so Chan's semantics and typed exit codes are the contract rather than a reimplementation of them.

## Babysitting

The health strip reports one of: booting, live, stalled, dead, failed. It polls `cs terminal list --json` for the tab's presence and queue depth, and hashes `cs terminal scrollback` to notice whether output is moving at all.

A command that cannot be resolved is caught **before** anything is spawned, by asking the login shell to `command -v` it. That matters because a command that exits 127 never reaches the registry at all, so after the fact there is nothing to read and nothing to report but the boot timeout. The check runs one way round on purpose: Chan's spawn does not read your login files, so its `PATH` is the smaller of the two, and what the login shell cannot find the spawn cannot find either. The reverse case gets a dead tab rather than a wrong refusal.

When an agent stalls, the recovery row offers four levers, in order of force. None of them fire on their own.

| Lever | What it does |
|---|---|
| Nudge | Chord-only submit, which fires whatever is parked in the compose box |
| Escape | A raw ESC with no chord, for an agent sitting in a modal |
| Restart | Respawns the PTY with the same command and env, dropping the queue, and briefs it again |
| Close | Ends the session |

Nudge and Escape go through the same write queue as everything else, and that queue only drains after 800ms of output quiescence. An agent wedged **while producing output** will not see either of them. Restart is the only lever that bypasses the queue.

## Limits

- One agent at a time, per Chan workspace.
- Bracketed-paste mode says the agent's TUI is up, not that it is at a prompt. An agent parked on a first-run gate takes the brief into that dialog instead: codex, for one, asks whether it trusts the directory the first time it runs in it. Start each agent once in the workspace from an ordinary terminal to clear those, then use Peek and Escape if one still catches you.
- The extension has no host API for layout beyond what `cs` exposes, so it cannot open a window. A/B in one pane is the whole layout model, which is also what works on a phone.
- Chan does not respawn a crashed extension. A crash means dead until Chan restarts.
- `cs` must be on `PATH`, or named by `$MOBILE_CHAT_CS`.
- Liveness comes from registry presence, because `cs` exposes no process exit code. That is sound while the agent tab is mounted, which it is in this layout, and unreliable otherwise.
- The Windows build compiles and its tests pass on a Windows runner, but nobody has run it against a real Chan on Windows yet. Discovery looks for Chan's control sockets as named pipes under `\\.\pipe\`, and the missing-command pre-flight is skipped there because Chan picks between PowerShell, cmd, and a POSIX shell at runtime.

## Development

```sh
./scripts/gate.sh    # fmt, clippy -D warnings, tests
```

Release tooling has its own checks, which CI runs on every tag:

```sh
shellcheck install.sh scripts/*.sh scripts/tests/*.sh
python -m unittest discover -s scripts/tests -p 'test_*.py'
./scripts/tests/install-release.sh
```

`install-release.sh` packages all four targets, serves them over `file://`, and runs the real `install.sh` for each platform with `uname` and `cygpath` faked, then checks that a corrupt archive and an unsupported architecture are both refused with nothing installed.

Tagging `vX.Y.Z` publishes a release. The tag must match `version` in the workspace `Cargo.toml`; CI refuses the mismatch rather than shipping a misnamed build.
