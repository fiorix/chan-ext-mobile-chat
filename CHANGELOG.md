# Changelog

This file records notable development history and design decisions. Reference documentation describes only the repository's current behavior and contracts.

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
