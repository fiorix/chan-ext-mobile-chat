# Mobile Chat

A [Chan](https://github.com/fiorix/chan) extension for talking to Claude, Codex, and Kimi from a phone.

Each Mobile Chat tab opens one conversation. Choose an agent and send a first message; its terminal starts directly on side B while you stay in chat. Claude and Codex connect automatically. For Kimi, finish startup through **Peek** if needed, then choose **Connect chat** once its normal input is ready. Replies, progress, and questions appear in the iframe. Questions accept choices or free text and stay pending while you are away.

Conversations, composer drafts, partial answers, and reading position are saved on the devserver. Closing a chat tab leaves its agent running. Reopen it from **Saved conversations**, or use **Stop agent** to end that agent and retain its history. **Peek** selects its terminal for native CLI permission or login prompts; Chan's A/B toggle returns to chat.

## Install

Install v0.3.0 from the release assets:

```sh
curl -fsSL https://github.com/fiorix/chan-ext-mobile-chat/releases/download/v0.3.0/install.sh | MOBILE_CHAT_VERSION=v0.3.0 bash
```

Or build and install this checkout:

```sh
./scripts/install-chan-extension.sh
```

Both methods install the extension under `~/.local/lib/mobile-chat` and write its declaration at `~/.chan/extensions/mobile-chat.toml`. Override the roots with `MOBILE_CHAT_INSTALL_ROOT` and `CHAN_HOME`. Restart Chan, then open **Mobile Chat** from the command launcher's Apps category. Rebuild and reinstall after changing embedded browser assets.

Use Chan v0.86.0 or newer for `cs terminal new --command` and repeated `--env`. The complete restoration flow also needs the small [companion Chan patch](host/README.md), tested against Chan 0.96.0. It supplies persistent extension-instance and workspace identities, plus exact terminal selection for Peek. Without it, reopen saved conversations manually after restoring a window; history across a Chan restart remains scoped to that older runtime. No host changes are installed by the extension installer.

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

## WhatsApp bridge

Opt-in second surface: pair the extension to a WhatsApp account as a linked device, record enabled chats to rotating on-disk logs, and let allowlisted people drive an already running agent with `/agent`. Off by default; enable it in `<chan-home>/mobile-chat.toml`:

```toml
[whatsapp]
enabled = true            # default false; off means no directory, no lock, no network
root = "/optional/override"  # data root; default <chan-home>/mobile-chat/whatsapp
log_max_bytes = 1048576   # rotate log.jsonl when appending would pass this size; default 1 MiB
log_keep = 10             # rotated logs retained per chat
media_max_bytes = 16777216  # a single media file larger than this is not stored; default 16 MiB
media = ["image", "video", "audio", "document", "sticker"]
reply_progress = true     # forward agent progress updates, not just final replies
```

The bridge is a `whatsapp-bridge` library crate linked into the binary behind the default-on `whatsapp` cargo feature; build with `--no-default-features` for a core-only build. A `[whatsapp]` section is unknown to a core-only binary, so keep it out of the config there. Keys here are read once at startup and take effect when Chan restarts; everything a person changes from the phone lives in the bridge's own `settings.json` and reloads live.

The welcome screen's WhatsApp panel is the pairing surface: it shows the QR as inline SVG with a countdown to the next rotation, the raw payload in a copyable field, the `wa.me` deep link, and a Regenerate button. The panel drives the same control-socket actions a client can send by hand: `whatsapp_pair` starts or restarts pairing, `whatsapp_unpair` logs out and deletes the linked-device session, and `whatsapp_status` pushes the status payload to every connected socket as codes rotate. The server hands out six refs per connection (60 seconds for the first, 20 for each of the other five); when they run out the bridge rebuilds the connection and a fresh code appears in the next push. `qr_svg` is the scan code, `qr_raw` the raw payload, and `qr_deep_link` a `wa.me` link openable from the phone's camera.

Everything the bridge keeps lives under `<chan-home>/mobile-chat/whatsapp/`, created at mode 0700 with an exclusive process lock so two Chan homes fail visibly rather than corrupt the store:

```
lock                       exclusive process lock
session.db                 linked-device session: device keys, Signal sessions
settings.json              chat enablement, bindings, allowlist, jid-to-directory index
chats/<slug>-<hash8>/
  meta.json                jid, kind, display name, first_seen, last_seen
  log.jsonl                current log
  log.1.jsonl .. log.N.jsonl  rotated logs, log_keep retained
  media/images/...
  media/video/...
  media/audio/...
  media/documents/...
  media/stickers/...
```

Each recorded chat gets one directory, named from its display name and the first 8 hex characters of the sha256 of its jid, created once and never renamed so a contact rename does not orphan history. `log.jsonl` holds one JSON object per line, appended and never rewritten, rotated on whole-record boundaries. Attachments land under `media/` gated by the configured types and `media_max_bytes`. Messages the bridge itself sends are logged too, with the conversation and entry ids attached, so each log is a complete transcript rather than only the inbound half.

Recording, binding, and permission are separate switches. Recording decides whether a chat's messages reach the log at all. Binding names an `(owner, conversation)` pair, so a chat bound in one workspace does not resolve from another. The allowlist is default deny, keyed on the sender's phone number in E.164 without a plus, with LID counterparts recorded as they are observed so a LID-addressed sender resolves to the same person; a message from a non-allowlisted sender in a recorded chat is logged and silently ignored, so the bridge never announces itself to strangers.

Commands parse only for an allowlisted sender in a recorded chat; everything else is logged only.

| Input | Effect |
|---|---|
| `/agent <prompt>` | Sends the prompt to the bound conversation. |
| `/agent <answer>` with a question pending | Answers the oldest pending question. A bare integer selects that option by position (1-based); anything else is free text. |
| `/agent cancel` with a question pending | Cancels the oldest pending question. |
| `/agent status` or `/agent` with no argument | Reports the bound conversation's title, phase, queue depth, and pending question count. |

The prefix match is case-insensitive on `/agent`, requires it at the start of the message, and tolerates leading whitespace. Failure paths answer with one auto-reply per chat per 60 seconds, debounced so a loop between the bridge and an agent cannot form: unbound chat, no agent running, or the unchanged error text of a refused send.

Assistant output routes back into the same chat through one forwarder task per bound conversation. Final replies always forward, progress updates forward when `reply_progress` is set, and questions forward as the body with numbered options plus a one-line instruction for answering from the phone. Markdown is rendered to WhatsApp's formatting and chunked at 3500 characters with `(1/3)` markers, and the last forwarded entry persists in `settings.json` so a restart does not replay the transcript.

WhatsApp text reaches an agent that runs with its permission checks bypassed, so treat it as the sharpest edge here: every prompt is wrapped in an envelope naming the sender and chat and stating plainly that the text is an untrusted third-party request, never authorization, with the chat log path included so the agent can read context for itself. The chat brief tells the agent the same, and the existing inline-question approval gate still applies to anything irreversible or outward facing. The default-deny allowlist is the only access control; keep it small.

The bridge uses `whatsapp-rust`, an unofficial client: using it may violate Meta's terms and can get the account suspended. Pair a secondary number, and expect the pinned `=0.7.0` dependency to need deliberate work to upgrade.

One trust-dialog note: the extension launches agents interactively, and neither Claude nor Codex offers a flag that pre-accepts the per-directory trust dialog. A first launch in an untrusted directory therefore needs one **Peek** to accept it; after that the directory is remembered. The known alternatives are Claude's `hasTrustDialogAccepted` per-project key and top-level `bypassPermissionsModeAccepted` in `~/.claude.json`, and Codex's `trust_level = "trusted"` per path in `~/.codex/config.toml`. The extension deliberately does not pre-seed them, because that would mean writing durable user config outside the workspace on every spawn.

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
