# Contributing

Keep changes narrow, use conventional commit messages, stage explicit pathspecs, review the staged diff, and run the repository checks before committing.

## Documentation

- Describe the repository as it works now. Do not organize reference documentation around staged delivery labels, task or review language, or development-process framing.
- Keep development history only in `CHANGELOG.md`.
- Do not use em dashes. Prefer a colon, period, comma, or parentheses.
- Keep each prose paragraph and list item on one logical line. Tables, code fences, and license text retain their native formatting.

## Checks

```sh
./scripts/gate.sh
```

That runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`. Run it after the last edit, not before one: a check that ran before a later change proves nothing about the change.

## Integration checks

Rust tests cover atomic persistence, helper authentication over a real loopback WebSocket, scope retirement, duplicate requests, drafts, direct launch, explicit connection, terminal-write failures, and stopping. A fake `cs` executable checks observable command arguments and delivery counts. The gate therefore needs permission to bind loopback sockets.

The browser regression uses real Chan terminals with a deterministic interactive agent. Start with a throwaway Chan home and workspace, using a Chan build with the [companion bridge](host/README.md):

```sh
mc_test=$(mktemp -d)
mkdir -p "$mc_test/home" "$mc_test/workspace"
CHAN_HOME="$mc_test/home" MOBILE_CHAT_INSTALL_ROOT="$mc_test/install" ./scripts/install-chan-extension.sh
```

Write `$mc_test/home/mobile-chat.toml` with the following content, replacing both absolute paths. The Node fixture accepts the initial prompt used by Claude/Codex and all three submit chords. Kimi exercises explicit **Connect chat**.

```toml
agents = ["claude", "codex", "kimi"]
[health]
poll_interval_secs = 1
[agent.claude]
command = "node /absolute/checkout/scripts/tests/agent-fixture.mjs /absolute/test/agent-events.jsonl"
[agent.codex]
command = "node /absolute/checkout/scripts/tests/agent-fixture.mjs /absolute/test/agent-events.jsonl"
[agent.kimi]
command = "node /absolute/checkout/scripts/tests/agent-fixture.mjs /absolute/test/agent-events.jsonl"
```

Start Chan in a separate terminal and use its printed authenticated URL:

```sh
CHAN_HOME="$mc_test/home" CHAN_UPDATE_CHECK=0 chan serve "$mc_test/workspace" --standalone --port 0 --no-browser
```

Install the browser test dependency outside the repository and run the flow:

```sh
npm install --prefix "$mc_test/tools" playwright
PLAYWRIGHT_BROWSERS_PATH="$mc_test/browsers" "$mc_test/tools/node_modules/.bin/playwright" install chromium
MOBILE_CHAT_PLAYWRIGHT="$mc_test/tools/node_modules/playwright/index.mjs" \
PLAYWRIGHT_BROWSERS_PATH="$mc_test/browsers" \
MOBILE_CHAT_TEST_URL='http://127.0.0.1:PORT/?t=TOKEN' \
MOBILE_CHAT_TEST_EVENTS="$mc_test/agent-events.jsonl" \
MOBILE_CHAT_TEST_OUTPUT="$mc_test/screenshots" \
node scripts/tests/browser-flow.mjs
```

The browser checks reload, drafts, pending answers, reconnect, lost acknowledgments, independent conversations, long history, reading position, messages above 4096 bytes, Peek, stopping one agent, and closing/reopening a chat. It saves a mobile screenshot and captures the page on failure. Shut down the disposable Chan process afterward; closing the test browser intentionally leaves agents running.

Also smoke-test installed Claude, Codex, and Kimi with their existing settings. Native trust, login, or permission prompts belong in Peek. For Kimi, wait for the normal prompt before choosing **Connect chat**. Verify a completed reply, a question, and a response to the answer. Do not automatically submit bootstrap text into a native startup dialog.

Finally, verify through `https://gw.chan.app` on a phone. A local browser check does not establish gateway authentication, forwarding, or soft-keyboard behavior on the user's device.

Reinstall the extension before every integration run involving asset or Rust changes. The embedded assets come from the built executable.
