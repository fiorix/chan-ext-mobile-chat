# Mobile Chat

A [Chan](https://github.com/fiorix/chan) extension for talking to Claude, Codex, and Kimi from a phone.

Each Mobile Chat tab opens one conversation. Choose an agent and send a first message; its terminal starts directly on side B while you stay in chat. Claude and Codex connect automatically. For Kimi, finish startup through **Peek** if needed, then choose **Connect chat** once its normal input is ready. Replies, progress, and questions appear in the iframe. Questions accept choices or free text and stay pending while you are away.

Conversations, composer drafts, partial answers, and reading position are saved on the devserver. Closing a chat tab leaves its agent running. Reopen it from **Saved conversations**, or use **Stop agent** to end that agent and retain its history. **Peek** selects its terminal for native CLI permission or login prompts; Chan's A/B toggle returns to chat.

## Install from this checkout

```sh
./scripts/install-chan-extension.sh
```

This builds the extension, installs it under `~/.local/lib/mobile-chat`, and writes its declaration at `~/.chan/extensions/mobile-chat.toml`. Override the roots with `MOBILE_CHAT_INSTALL_ROOT` and `CHAN_HOME`. Restart Chan, then open **Mobile Chat** from the command launcher's Apps category. Rebuild and reinstall after changing embedded browser assets.

Use Chan v0.86.0 or newer for `cs terminal new --command` and repeated `--env`. The complete restoration flow also needs the small [companion Chan patch](host/README.md), tested against Chan 0.96.0. It supplies persistent extension-instance and workspace identities, plus exact terminal selection for Peek. Without it, reopen saved conversations manually after restoring a window; history across a Chan restart remains scoped to that older runtime. No host changes are installed by the extension installer.

The published v0.2.0 release uses the earlier survey interface. Build this checkout to test the conversation flow described here.

Releases ship native binaries for four targets, each compiled and tested on its own GitHub-hosted runner. The Linux builds target musl, so they carry no libc dependency.

| Platform | Archive |
|---|---|
| Linux x86_64 | `mobile-chat-linux-x86_64.tar.gz` |
| Linux aarch64 | `mobile-chat-linux-aarch64.tar.gz` |
| macOS aarch64 | `mobile-chat-macos-aarch64.tar.gz` |
| Windows x86_64 | `mobile-chat-windows-x86_64.zip` |

## Configuration

Optional, at `<chan-home>/mobile-chat.toml`. The default picker contains `claude`, `codex`, and `kimi`.

```toml
agents = ["claude", "codex", "kimi"]

[agent.kimi]
command = "/Users/me/.kimi-code/bin/kimi --auto"

# Commands can include existing arguments, wrappers, and shell syntax.
[agent.codex]
command = "codex --profile work"
submit_chord = "codex"

[health]
boot_timeout_secs = 45
poll_interval_secs = 5
stall_after_secs = 120
```

Chan spawns the command through a shell without reading login files. Set an absolute command if only your shell configuration puts an agent on PATH. Custom roster entries must specify a `submit_chord`: `claude`, `codex`, `kimi`, `gemini`, or `opencode`. An explicit command replaces the default command and its permission flags. On Unix, Claude and Codex commands receive the brief as a quoted positional argument after `--`; wrappers must forward arguments. Set `prompt_argument = false` for a wrapper that needs the explicit **Connect chat** path. Kimi and Windows default to that explicit path. Configuration changes take effect when Chan restarts.

The startup timeout controls terminal discovery and the hint to inspect startup prompts. A long queued message produces a Peek hint after `stall_after_secs`; silence never triggers a restart or proves completion.

## Permissions

Default commands retain the agents' no-prompt modes:

| Agent | Command |
|---|---|
| claude | `claude --permission-mode bypassPermissions` |
| codex | `codex --dangerously-bypass-approvals-and-sandbox` |
| kimi | `kimi --auto` |
| gemini (optional) | `gemini --yolo` |
| opencode (optional) | `opencode --auto` |

The chat brief tells the agent to ask inline before irreversible or outward-facing actions, including pushing commits, deleting work, installing packages, and changing credentials or durable configuration. Existing explicit authorization applies; reading, builds, tests, and reversible workspace edits can proceed. This relies on the agent following instructions; it is not an enforced sandbox. Custom commands control their own permission flags, and any remaining native prompts are available through Peek.

## Agent channel

The extension launches each agent through `cs terminal new`, with a unique terminal handle and an extension-owned chat brief. It does not create a team or write project instructions. The spawn environment includes `MOBILE_CHAT_HELPER` (this executable) and `MOBILE_CHAT_SESSION` (a private connection descriptor).

Claude and Codex receive the brief through their CLI initial-prompt argument, so startup dialogs never receive queued bootstrap keystrokes. Kimi's `--prompt` runs non-interactively; its brief is queued only after the user chooses **Connect chat**. The agent first acknowledges readiness. Only then does the extension deliver saved user messages as short `cs terminal write` references. The helper retrieves the full message, avoiding the terminal queue's 4096-byte limit. Message bodies may contain up to 64 KiB of UTF-8.

```sh
"$MOBILE_CHAT_HELPER" agent ready
"$MOBILE_CHAT_HELPER" agent read MESSAGE_ID

"$MOBILE_CHAT_HELPER" agent reply --id MESSAGE_ID-progress --to MESSAGE_ID \
  --progress "Checking the implementation."
"$MOBILE_CHAT_HELPER" agent reply --id MESSAGE_ID-final --to MESSAGE_ID \
  --file reply.md

"$MOBILE_CHAT_HELPER" agent ask --id MESSAGE_ID-question --to MESSAGE_ID \
  --option Local --option Remote "Where should we run this?"
```

Reply and question bodies accept a positional argument, `--file`, or stdin. Request IDs contain letters, digits, underscores, or hyphens. Retry the same operation with the same ID and content to recover a lost acknowledgment without creating a duplicate.

An `ask` returns immediately. The agent finishes any independent work, calls `agent ready`, and yields; an answer arrives later as another queued message. Delivery is limited to one message per completed turn. A final reply or explicit `ready` allows the next message; progress updates do not. `read` returns its original `question_id`. Questions never time out or imply consent. `cs terminal survey` is not part of this channel.

Markdown replies support code, lists, links, and tables. Raw HTML is escaped, unsafe link schemes are disabled, and remote images are omitted.

## Persistence and recovery

Private atomic snapshots live under `<chan-home>/mobile-chat/`, separated by trusted workspace identity when the companion host bridge is available. Browser tabs have persistent bindings; they do not own the conversation data. Per-run helper credentials stay out of browser snapshots.

A send is saved before acknowledgment. Its delivery state distinguishes waiting, queued, read by the agent, and uncertain delivery. Browser retries use the same request IDs. A lost terminal-write acknowledgment is retained as uncertain and is never automatically replayed. Inspect the agent through Peek before sending the work again.

The composer displays **Saved**, **Saving...**, or an offline warning. Reload restores the last server-saved state; text typed while disconnected exists only in the current iframe until it reconnects. Return inserts a newline; Ctrl/Cmd+Enter sends on desktop.

With the companion host bridge, restarting Chan restores conversations and tab bindings while marking old agent runs stopped. It does not claim to resume the agent's model context. Start a new conversation to launch another agent.

## Limits

- Native trust and login prompts, and permission prompts enabled by custom commands, require Peek.
- Startup depends on the agent following the chat brief and calling the helper. If chat remains connecting, Peek shows its terminal.
- Chan must remain running. Chan does not respawn a crashed extension automatically.
- `cs` must be on PATH, a sibling of the extension executable, or selected by `MOBILE_CHAT_CS`.
- Conversation snapshots have a 64 MiB size limit. Storage failures are reported without discarding acknowledged history.
- Windows compilation and unit checks are covered by release CI; this conversation flow has been exercised locally on macOS.

## Development

```sh
./scripts/gate.sh
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for disposable Chan and browser integration checks. Release tooling checks remain:

```sh
shellcheck install.sh scripts/*.sh scripts/tests/*.sh
python -m unittest discover -s scripts/tests -p 'test_*.py'
./scripts/tests/install-release.sh
```

Tagging `vX.Y.Z` publishes a release. The tag must match the workspace `Cargo.toml` version. No tag is needed to install a local checkout.
