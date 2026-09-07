# Mobile Chat implementation plan

Implement the user flow in [DESIGN.md](DESIGN.md). Keep the existing Rust binary, embedded browser assets, and `cs` control boundary. Build the extension work first; integrate the deferred Chan host changes separately.

## Work status

- [x] Persistent conversation model and storage.
- [x] Agent helper and direct launch.
- [x] Conversation protocol and chat interface.
- [x] Host integration and restoration (separate companion patch).
- [x] Regression tests, browser verification, and test instructions.

## Constraints verified in the current code

- The installed `cs terminal new` supports `--command`, repeated `--env`, and explicit window, pane, and side placement. Its success acknowledgment means the terminal creation request was queued, not that the agent is ready.
- The current extension provisions a one-member team and reuses `.chan/mobile-chat`. Chan regenerates that directory's bootstrap with team coordination and `cs terminal survey` instructions. Direct terminal launch removes that conflicting bootstrap.
- The extension currently holds one chat per tenant scope in memory and broadcasts status only. Browser requests have no conversation or operation IDs.
- The iframe has an opaque origin and cannot use browser storage. Saved drafts, transcripts, and view state need server persistence.
- Chan sends a window ID to the iframe, but does not send its tab or pane identity. Restored extension tabs get fresh tab IDs and retain no conversation binding.
- `X-Chan-Extension-Scope` is the tenant's randomly generated runtime instance ID. It is useful for live request isolation, but changes when the tenant restarts.

## Implementation sequence

### 1. Define conversations and persist their state

Separate the retained conversation from the running agent and the connected browser view. Give conversations, agent runs, messages, questions, and mutating requests stable IDs. Keep the live tenant scope as an access boundary around conversation lookup.

Add a small persistence module using atomic JSON snapshots, one per conversation, under the extension's private data directory. Persist messages, question state, the saved draft, reading position, operation IDs, and agent association. Serialize mutations per conversation and persist before acknowledging success or broadcasting an update. Keep corrupt or unreadable records visible as errors rather than silently replacing them with empty conversations.

Represent execution and message delivery separately. An agent can be starting, connected to the chat channel, waiting for an answer, or stopped. A message can be saved, accepted by the terminal queue, acknowledged by the agent, or have uncertain delivery. Terminal silence alone does not prove task completion or a pending permission prompt.

Keep the permanent conversation store ID separate from Chan's runtime scope. Durable rebinding to the correct workspace after a tenant restart depends on the host identity contract below; do not silently merge records across scopes or claim restart restoration before that binding exists.

### 2. Add the agent helper to the existing binary

Add helper subcommands to `mobile-chat-extension`, while preserving its existing server invocation for the extension declaration. Use a conversation-bound local endpoint and credentials supplied through the agent's spawn environment. Bind callbacks to the current agent run so an old process cannot update a replacement run.

The helper needs four operations:

- **Ready:** acknowledge the chat-mode bootstrap.
- **Read:** retrieve and acknowledge a saved user message or survey answer by ID.
- **Reply:** post Markdown, identifying progress versus a completed reply and the message being answered.
- **Ask:** post a question with choices and optional explanatory Markdown; return its ID immediately.

Accept message bodies through stdin or a file argument so the agent does not have to shell-quote long Markdown. The extension writes the durable transcript; agents do not edit the transcript store directly. Repeated requests with the same operation ID return the recorded outcome instead of creating duplicate replies or questions.

Queue short message references and the chat-mode reminder through `cs terminal write`. The helper reads the full body from the extension, keeping user content separate from the queue's 4096-byte transport limit. Survey answers use the same route and identify the original question. Questions remain pending without holding a helper process open.

### 3. Replace team provisioning with direct terminal launch

Update `session.rs` and `control.rs` to use `cs terminal new --command ... --env CHAN_AGENT=...`, with a unique terminal handle for each agent run. Supply the helper's discovery environment at spawn. Retain configurable commands and submit chords, including wrappers, without rewriting agent-owned configuration or permission settings.

Require a Chan version that supports these direct-spawn options and report a clear upgrade requirement when unavailable. Do not retain team provisioning as a compatibility fallback.

Use the configured command as the PTY spawn command. Real CLI tests showed that automatic queued bootstrap text can interfere with native startup dialogs. Claude and Codex therefore receive the brief as a quoted initial-prompt argument. Kimi uses explicit **Connect chat** once its normal input is ready. Both paths wait for the helper's **Ready** acknowledgment. Preserve the first user message until that acknowledgment arrives. Keep terminal appearance, bootstrap delivery, and chat readiness as distinct states; test startup timing against real agents.

The brief explains where replies and questions go, how to acknowledge queued message IDs, how to yield so queued input can arrive, and that native CLI prompts remain accessible through **Peek**. Reinforce the helper contract in later queued messages. Remove team configuration generation and its tests once the direct launch path is verified.

Give each agent run one owned health task. Reopening a conversation subscribes to that run; it does not spawn another process or watcher. Closing a browser connection does not stop it. **Stop agent** targets only that conversation's terminal, retains history, and deactivates its pending questions after the stop outcome is established. Do not automatically restart or resend work after an ambiguous failure.

### 4. Make the browser protocol conversation-aware

Extend the existing WebSocket protocol with conversation listing, creation, attachment, message submission, survey answers, saved view state, and stopping an agent. Include conversation and request IDs on mutations and responses.

Return a current snapshot on connection or attachment. Full snapshots are sufficient initially and recover from missed broadcasts without an event-replay subsystem. Reconnection requests state; it does not replay terminal writes. Deduplicate browser retries, and show uncertain terminal delivery explicitly because a lost `cs` acknowledgment cannot prove that a write did not reach the queue.

Preserve the proxy authentication, trusted scope boundary, relative URLs, and parent-source checks. Agent helper credentials stay separate from the browser proxy token. Validate conversation ownership and active agent-run identity at the server boundary.

### 5. Build the chat interface

Keep the existing plain JavaScript and CSS structure. Replace the status-and-composer surface with:

- A new-conversation form and a saved-conversation list.
- A conversation header with agent state, **Peek**, and an action menu containing **Stop agent**.
- A Markdown transcript containing messages, concise progress updates, and inline question cards.
- Choice selection or a free-text survey answer, submitted explicitly and retained in the transcript.
- A composer with visible saved, queued, and failed states. Clear submitted text only after the server accepts it, without erasing text typed while an earlier send was pending.
- A visible reconnecting state and an indication when a draft has not been saved.

Save reading position using a message anchor and offset. Follow new messages when the user is at the bottom; preserve their position when reading earlier content. Keep mobile keyboard behavior, safe-area spacing, focus, and touch targets usable. Render Markdown without executable HTML or unsafe link schemes.

Remove the singleton declaration so multiple tabs can open independent conversations. Until the host binding is available, explicitly reopening from the saved list is the recovery path. Do not replace the missing binding with a global last-used conversation or an arbitrary match for the first extension pane.

### 6. Integrate the deferred host contract

Keep this work separate in Chan. The minimum contract needs:

- A persistent extension-instance identity, exposed to its iframe and retained through reload and layout restoration.
- The current pane identity, updated when the tab moves, so direct launch and **Peek** address the intended location.
- A stable, trusted workspace storage identity in addition to the runtime scope, so conversation ownership can be rebound after tenant restart.

Associate the extension-instance identity with a conversation in the extension's store. The host persists the binding identity, while the extension persists conversation content. Define duplication as creating a fresh extension instance; moving a tab retains its binding.

Verify this contract against the actual Chan host before implementing it. Existing `session-context` supplies none of these guarantees. Automatic restoration to the right conversation and exact pane placement are completion requirements for this integration, not claims about the extension-only work.

## Verification

Use focused Rust tests for persistence and state transitions, plus a fake `cs` executable for observable launch, queue, stop, and failure behavior. Test the helper over its real local transport. Browser checks must cover user-visible state rather than source-string matches.

Required scenarios:

- Two independent conversations, including two using the same agent, never share messages, questions, credentials, or stop targets.
- Direct launch preserves the agent command and submit identity, waits for chat readiness, and retains the first message on startup failure.
- Replies and progress appear in chat; a survey answer is recorded once, reaches the correct question, and needs no blocking CLI survey.
- A dropped browser connection or duplicate request does not duplicate messages, answers, agent launches, or terminal writes.
- Failed and ambiguous `cs` calls retain honest delivery state. A quiet or disconnected terminal is not automatically restarted.
- Closing and reopening a chat leaves a running agent intact. Stopping one agent retains its transcript and leaves other conversations running.
- Reload restores saved content, pending questions, drafts, and reading position. Unsaved offline drafts are not reported as durable.
- With the host contract integrated, reload and tab movement retain the right binding; duplicate tabs get independent bindings; tenant restart restores history without claiming that the agent's model context survived.

Run `./scripts/gate.sh` after implementation. Use a throwaway Chan workspace and a deterministic agent fixture for the full browser flow, then exercise Claude, Codex, and Kimi with their existing interactive settings. Verify the real gateway path and mobile layout, including **Peek** for a native permission prompt. Local smoke testing cannot establish behavior on the user's authenticated gateway or phone. Reinstall the built extension before integration testing; leave the user's running devservers and durable configuration unchanged.

Update `README.md` and `CHANGELOG.md` to describe the behavior actually verified, including the required Chan version or feature check and any host-dependent behavior still pending.

## Verification results

- Extension gate: 39 tests, fmt, and clippy with warnings denied, including the retained v0.2.0 launch-default tests.
- Companion host: 16 proxy tests, 324 workspace tests, Svelte checking, and Rust/web builds.
- Real Chan plus the deterministic fixture: all three submit chords, explicit Kimi connection, inline answers, reload, saved drafts, offline/retry deduplication, independent conversations, large messages, paginated history and reading position, Peek, Stop, and close/reopen.
- Real Claude and Codex: bootstrap, first message, reply, inline question, answer, and final reply all completed through the helper.
- Real Kimi: startup and login state inspected. Its local OAuth provider needs login; model replies were tested with the fixture.
- Host restart: saved histories and separate tab bindings restored, old runs marked stopped, no agents respawned.
- User acceptance remaining: the actual gateway and phone, including the soft keyboard. The test server and host checkout were isolated from the user's devservers.
