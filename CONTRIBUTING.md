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

## Testing what cannot be unit tested

The parts of this extension that break are the parts that talk to Chan, and none of them are reachable from a unit test. The tests that exist pin the contracts instead: that the handshake satisfies Chan's validation, that the generated team config satisfies Chan's, that no asset uses an origin-rooted URL (the iframe has an opaque origin at a capability path), and that `[hidden]` still outranks the flex layout rules.

For anything behavioural, run a real Chan against a throwaway workspace rather than reasoning about it:

```sh
CHAN_HOME=/tmp/mc-home chan devserver --service none --bind 127.0.0.1 --port 7788
```

with the declaration copied into `$CHAN_HOME/extensions/`, then mount the workspace over the management API and open the tenant in a browser. A `chan open` without `CHAN_NO_DESKTOP_HANDOFF=1` will hand off to a running Chan desktop instead.

Reinstall before restarting. A stale binary against a new config produces a confusing error that looks like a bug in the config.
