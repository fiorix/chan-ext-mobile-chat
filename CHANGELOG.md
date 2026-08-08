# Changelog

This file records notable development history and design decisions. Reference documentation describes only the repository's current behavior and contracts.

## 2026-08-08

### The team of one is gone

Chan v0.86.0 taught `cs terminal new` the `--command` and `--env` flags, which is the whole reason the team scaffolding existed. Spawning is now one call:

```
cs terminal new --tab-name @@chat-a1b2c3 --tab-group mobile-chat \
  --command 'claude --permission-mode bypassPermissions' \
  --env CHAN_AGENT=claude --pane pane-1 --side b
```

Verified against a live Chan 0.86.0: `cs terminal list --json` reports the new tab as `agent: claude`, which is the same server-side derivation `cs terminal write --submit` runs, so the exit-69 shell-session failure that forced the team route cannot occur. That takes the generated team config, the brief file, the `tempfile` scratch directory, and the `.chan/mobile-chat/` directory in the workspace with it. Nothing is written to disk to start an agent any more.

Two things that came free with `cs terminal team` did not come with `new`, and are now the extension's own:

**Readiness.** Team spawn waits for each member's PTY to enable bracketed-paste mode before poking it; `new` pokes nothing. The health poll watches the tab's scrollback for that same DECSET 2004 sequence and holds the session in `booting` until it appears. The gate is the honest one: it says the TUI is up, not that it is at a prompt, so an agent parked on a first-run gate still takes the brief into that dialog. Chan's own gate has the same blind spot.

**The brief.** It used to be folded into the team's `bootstrap.md` and reached the agent through Chan's identity poke. It is now the agent's literal first prompt, one `cs terminal write --submit`, which is why it has to fit in 4096 bytes and why there is a test that says so. `booting` therefore means "not yet briefed", and the composer refuses to send until it clears: a user message that overtook the brief would reach an agent that does not know surveys are the only way back.

Two consequences worth naming. Restart now re-briefs, because a respawned agent remembers nothing and `cs terminal restart` clears the replay ring (measured), so the readiness gate reads the new run rather than the old one's tail. And a spawn failure is now asynchronous: `cs terminal new` acks that the request was queued, not that a tab exists, so every way it can still fail belongs to the health poll.

### Agents run with permission checks off

A permission prompt renders in the agent's TUI, on the side of the pane nobody on a phone is looking at. Answering it needs a flip, a soft keyboard, and a TUI, which is the exact thing this extension exists to avoid, so an agent that stops to ask has silently gone dead. Each of the five defaults now launches in its no-prompt mode: `--permission-mode bypassPermissions`, `--dangerously-bypass-approvals-and-sandbox`, `--auto`, `--yolo`, `--auto`.

claude's flag was picked by measurement, not by name. `--dangerously-skip-permissions` opens a one-time consent screen ("WARNING: Claude Code running in Bypass Permissions mode", with a `1. No, exit` / `2. Yes, I accept` prompt) and parks there. `--permission-mode bypassPermissions` reaches the composer with bypass already on and no dialog at all. The former would have stranded the first session on a new machine at exactly the prompt the flag was chosen to eliminate.

Turning the checks off deletes a safety gate, so the brief now supplies the replacement: the agent is told it is unattended, that `cs terminal survey` is therefore its own approval gate, and where the line is. Irreversible or outward-facing actions are surveyed first; reading, searching, building, testing, and workspace edits git can undo are not; and not being sure which one an action is counts as a reason to ask. That is judgement rather than enforcement, and the README says so rather than implying a sandbox.

### The login shell's PATH was never the spawn's PATH

Measured while replacing the spawn: a member command and a `cs terminal new --command` both run without reading login files, so `~/.kimi-code/bin/kimi` resolves in `$SHELL -lc` and does not resolve in the spawn. The tab never reaches the registry. The preflight check has always asked the login shell, and the code and the README both claimed that was the same question Chan asks. It is not; it is the larger one. That makes the check's refusals sound (what the login shell cannot find, the spawn cannot find either) and its silence incomplete, which is now what both say. The behavior is unchanged, and the `kimi` config example, which existed for this case already, now names the real reason.

## 2026-08-06

### v0.1.0

First release. Tagging `vX.Y.Z` builds and tests on four native runners (Linux x86_64 and aarch64 against musl, macOS aarch64, Windows x86_64), packages a reproducible archive per platform, and publishes them with `SHA256SUMS` and `install.sh`. CI refuses a tag that does not match the workspace version.

### What the Windows runner caught

A `cargo xwin check` cross-compile passes on code that is wrong at runtime, and it did. Two bugs only showed up once the tests actually ran on Windows:

`socket_tail` treated the `.sock` suffix as optional there, so every `chan-control-*` name matched whatever its extension. `\\.\pipe\` lists every named pipe on the machine, so that would have probed unrelated processes' pipes. A name now qualifies only with `.sock`, or with no extension at all on Windows.

The candidate-ordering test asserted `parent_pid()` was present, which is a unix-only guarantee, and re-implemented the classification instead of calling it. Ranking became a pure function that the test drives directly on every platform.

The lesson is the boring one: a compile check is not a test, and the dry run before the tag is what turns that from an outage into a commit.

### Prototype

First working version, verified end to end against Chan 0.84.1: pick an agent, it spawns on side B, a message typed in the chat tab reaches it and submits, the agent answers with a survey overlay, and the health strip tracks it.

### Why the agent is spawned as a team of one

The obvious approach is `cs terminal new` followed by `cs terminal write $'claude\n'`. It does not work, and the failure is silent in the worst way.

Chan derives a terminal's submit chord from the PTY's spawn command and its `CHAN_AGENT` spawn env (`terminal_sessions::derived_submit_agent`), never from what is running inside the PTY. A tab from `cs terminal new` spawns the tenant shell with `command: None`, so it stays a shell session forever. Measured against a live Chan with claude's TUI up in the tab:

```
$ cs terminal write --tab-name @@spike --submit=claude 'say PONG'
exit=69
queued at position 1; @@spike is a shell session: no claude chord applied
```

The text arrives and parks un-submitted in the compose box. A chord-only follow-up is refused the same way.

`cs terminal team new` is the only route from the control socket to a PTY whose spawn command is the agent itself. It also brings Chan's bracketed-paste readiness gate and the `window_id` binding that `cs terminal survey` needs to resolve a window, so the reply path works for free.

Chan is fixing the underlying limitation in v0.86.0.

### Why the team directory is in the workspace

`cs terminal team new` writes through a workspace-scoped handle (`chan_workspace::Workspace`), so a path outside the workspace root is not reachable. One reusable directory at `.chan/mobile-chat/` is the closest thing to a scratch location: Chan's workspace walker, indexer, and file watcher all hard-skip `.chan/`, so nothing in it surfaces in the tree, in search, or in the graph.

### Why a missing command is caught before spawning

A command that is not on the login shell's PATH exits 127 immediately, and the session leaves the registry before `cs terminal scrollback` can be read. All that is left afterwards is Chan's own report, "terminal ended before enabling bracketed-paste mode", which describes the symptom and not the cause. Asking `$SHELL -lc 'command -v'` first (the same shell Chan will use) turns that into "not on the login shell's PATH", and leaves no dead tab behind.

### Why every mutation rides one WebSocket

Chan's extension proxy answers 403 to POST, PUT, and DELETE from non-owner tunnel participants, while GET and WebSocket upgrades pass. A phone reaching this over the Chan tunnel is exactly that case, so a REST-shaped API would have worked locally and broken for the intended user.

### Notes from the first browser walk

Two bugs that only a real render exposed:

- Every section styled `display: flex` ignored the `hidden` attribute the render pass sets, because a class-level `display` outranks the user-agent stylesheet's `[hidden] { display: none }`. The composer and the recovery row were visible with no agent running.
- The declared `open` command duplicated the launcher row Chan already derives from the declaration's `name`.
