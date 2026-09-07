# Chan companion bridge

[chan.patch](chan.patch) adds the host support needed to restore the right conversation after a Chan window or workspace restarts. It was built and tested in an isolated checkout of Chan 0.96.0, commit `c3536fe78110ecc0d7e970f190ba6aeb37eca0a0`.

From a Chan checkout:

```sh
git apply --check /path/to/chan-ext-mobile-chat/host/chan.patch
git apply /path/to/chan-ext-mobile-chat/host/chan.patch
cd web
npm ci
npm run build
cd ..
cargo build --release -p chan --no-default-features
```

Use that Chan build on the devserver, install this extension checkout, and restart Chan. The extension installer does not apply this patch or replace a running Chan installation.

The patch adds three contracts:

- Each extension tab has a random `instanceId`, serialized as `xe` in its saved tab state. New tabs get new identities; moving or restoring a tab retains its identity. Session context exposes it as `tab_id`, alongside the current `pane_id` and existing window `self_id`.
- The extension proxy injects `X-Chan-Extension-Workspace` from Chan's workspace metadata key, alongside the existing runtime scope. Browser-supplied `X-Chan-*` headers are stripped. This stable identity lets the extension retain history across tenant restarts without conflating workspaces.
- A session-context extension may send `chan:extension-focus-terminal:v1` with `window_id`, `pane_id`, and `tab_id`. The host accepts it only from that extension's iframe, for a terminal in the current window. This lets Peek select the exact agent when side B has several terminals.

Existing extension tabs without an instance identity acquire one on restoration. Existing runtime-scoped Mobile Chat records remain in their original storage directory; the extension does not guess which workspace owns them.

Validation: 16 Chan proxy tests, 324 focused workspace tests, Svelte checking, Rust and web builds, and the Mobile Chat browser flow. Full browser-tab duplication copies a saved layout identity; open a new Mobile Chat tab from the command launcher for an independent conversation. Cross-window extension-tab dragging is not supported by this Chan version.

To remove the host changes from an otherwise unchanged checkout, use `git apply -R /path/to/chan-ext-mobile-chat/host/chan.patch`, rebuild, and restart. Conversation files remain in the extension's private storage.
