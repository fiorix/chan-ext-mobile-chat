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

The extension owns very little. Chan already knows how to spawn an agent with the right submit chord, wait for its TUI to come up, deliver a prompt so it fires instead of parking, ask a human a question, and report a terminal's health. Mobile Chat is the phone-shaped front door onto that.

## Install

Chan v0.83.0 or newer discovers local extensions at `~/.chan/extensions`. Install the latest release with:

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

Optional, at `<chan-home>/mobile-chat.toml`. With no file you get the five agents Chan knows a submit chord for: claude, codex, kimi, gemini, opencode.

```toml
# The picker, in order. The first entry is the default.
agents = ["claude", "codex", "kimi", "work-agent"]

# An agent that is installed somewhere other than PATH.
[agent.kimi]
command = "/Users/me/.local/share/kimi/bin/kimi"

# Any command at all, with the chord named explicitly.
[agent.work-agent]
command = "my-shell-script --profile work"
submit_chord = "opencode"

[health]
boot_timeout_secs = 45     # how long to wait for the agent to appear
poll_interval_secs = 5     # how often to check on it
stall_after_secs = 120     # queued and quiet this long means stuck
```

`command` is free-form. Chan runs a member command through `$SHELL -lc`, so arguments, wrappers, and shell syntax all work, and the login shell's PATH applies.

`submit_chord` (`submit-chord` also works) picks which chord submits your message. It must be one of the five Chan knows, because that name becomes `CHAN_AGENT`, and **Chan silently ignores a value it does not recognize** and goes back to guessing from the command. A guess that comes up empty produces a terminal that accepts messages and never submits them, so the config refuses an unknown chord at startup instead. A roster name that is not itself a known chord must declare one.

The config is read once, at Chan startup. Editing it means restarting Chan.

## How it works

Chan spawns the extension as a subprocess and reverse-proxies its loopback server into a sandboxed, opaque-origin iframe. From there:

- **Starting an agent** runs `cs terminal team new` with a generated one-member team. This is not incidental. Chan derives a terminal's submit chord from the PTY's **spawn command** and its `CHAN_AGENT` spawn env, never from whatever is running inside it. A tab made with plain `cs terminal new` is a shell, so a `claude` started by typing into it stays a shell session: every `cs terminal write --submit=claude` is refused with exit 69 and the text parks un-submitted in the compose box. Making the agent the member's command fixes the chord, and brings along Chan's own bracketed-paste readiness gate and the `window_id` binding that `cs terminal survey` needs.
- **The team directory** is `.chan/mobile-chat/` inside the workspace, reused across sessions. Chan's workspace walker, indexer, and file watcher all hard-skip `.chan/`, so it never shows up in the tree, in search, or in the graph. Chan writes team files through a workspace-scoped handle, so this cannot live in `/tmp`.
- **Your message** goes out as `cs terminal write --tab-name <handle> --submit=<chord>`, capped at 4096 bytes because that is Chan's limit and truncating a prompt is worse than refusing it.
- **The agent's reply** comes back as `cs terminal survey`, which the brief tells it to use for questions *and* for finished answers. Chan renders it as a blocking overlay in the window that owns the terminal.
- **Peek** runs `cs pane focus <pane> --side b` to flip you to the agent. The pane's own side toggle flips back.

Everything the extension does to Chan goes through the `cs` client, so Chan's semantics and typed exit codes are the contract rather than a reimplementation of them.

## Babysitting

The health strip reports one of: booting, live, stalled, dead, failed. It polls `cs terminal list --json` for the tab's presence and queue depth, and hashes `cs terminal scrollback` to notice whether output is moving at all.

A command that cannot be resolved is caught **before** anything is spawned, by asking the login shell the same question Chan will (`command -v`). That matters because a command that exits 127 leaves the registry before its scrollback can be read, so after the fact the only available report is Chan's own "terminal ended before enabling bracketed-paste mode", which does not say why.

When an agent stalls, the recovery row offers four levers, in order of force. None of them fire on their own.

| Lever | What it does |
|---|---|
| Nudge | Chord-only submit, which fires whatever is parked in the compose box |
| Escape | A raw ESC with no chord, for an agent sitting in a modal |
| Restart | Respawns the PTY with the same command and env, dropping the queue |
| Close | Ends the session |

Nudge and Escape go through the same write queue as everything else, and that queue only drains after 800ms of output quiescence. An agent wedged **while producing output** will not see either of them. Restart is the only lever that bypasses the queue.

## Limits

- One agent at a time, per Chan workspace.
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
