# Mobile Chat user flow

Agreed design. This describes the intended experience; `README.md` describes the current implementation.

## Agreed decisions

- One conversation per Chan tab, with its agent terminal on side B.
- Support Claude, Codex, and Kimi through a shared interaction model.
- Launch agents directly as terminals, with a bootstrap owned by Mobile Chat. Do not provision a Chan team.
- Show replies and extension-owned surveys inside the chat iframe.
- Preserve conversation state across reloads and support multiple independent conversations.
- Closing a chat tab hides the conversation and leaves its agent running. A separate **Stop agent** action ends execution while retaining history.
- Keep **Peek** as the fallback for native CLI permission prompts. Custom integrations for those prompts are outside this design.
- Investigate the required Chan host changes later.

### WhatsApp bridge

- The WhatsApp client lives in a new workspace library crate, `whatsapp-bridge`, linked into the existing binary behind a default-on cargo feature, not in a separate process. `whatsapp-rust` re-exports its whole stack as one dependency, and Chan already supervises exactly one extension process, so an in-process session is a Chan-wide singleton for free while the extension's own server owns the process lifetime; a second process would add a lifecycle to supervise for no benefit.
- `/agent` targets only a chat explicitly bound to an existing Mobile Chat conversation, never spawns. Spawning from a text message would put an unreviewed launch decision in the path of untrusted input, while a binding makes the target visible and reviewable in `settings.json`; an unbound chat or a stopped agent gets an honest auto-reply instead.
- Final replies, progress updates, and questions all flow back to WhatsApp, and questions are answerable from the phone. A phone-only user must see everything the iframe would show, and a question that cannot be answered remotely would strand the agent exactly like an unanswered permission prompt does, so the `/agent` grammar doubles as the answer channel.
- The agent trust dialog is left alone. No CLI flag exists to pre-accept it, and pre-seeding the Claude or Codex trust keys would mean the extension writing durable user config outside the workspace, so the design accepts the one-time Peek instead and records the known keys as alternatives only.

## User flow

### Open a conversation

A new Mobile Chat tab offers **New conversation** and a list of saved conversations to reopen. Each opened tab displays one conversation.

For a new conversation, choose an agent and enter the first message. The extension starts the agent on side B and shows a connecting state. Claude and Codex receive a native initial prompt. Kimi uses an explicit **Connect chat** step after its normal terminal input is ready; native startup prompts stay in Peek. The agent acknowledges that its chat channel is ready before the first user message is delivered. Failed startup preserves the message for retry.

Reopening a conversation restores its history and reconnects to its existing agent when available. If the agent has stopped, the transcript remains visible with a stopped status; reopening does not silently start another agent.

### Talk to the agent

The transcript contains user messages, agent replies, brief progress updates, and questions. Replies render as Markdown and need no acknowledgment button. The composer stays at the bottom; **Peek** remains available in the header.

Follow-up messages sent while the agent is working queue for later delivery. Sending a message does not implicitly interrupt the agent. Message delivery state should distinguish a saved or queued message from one the agent has acknowledged.

### Answer a question

The agent's question appears as an inline card with choices and a free-text answer. The submitted answer becomes part of the conversation, and the card remains visible as answered.

Questions stay pending until answered or explicitly cancelled, including while the phone is disconnected. The agent waits on work that needs the answer and may continue independent work. It calls the helper's **Ready** operation before yielding to receive the queued answer. A question does not require keeping a blocking command alive while the user is away.

### Leave and return

Reload restores the conversation, latest saved draft, reading position, and pending questions. Reconnection does not itself spawn an agent or resend user messages. Connection loss is visible, including when a draft has not yet been saved.

Closing the tab leaves work running. The saved-conversation list provides a way back. **Stop agent** stops execution without deleting the transcript; pending questions from that execution become inactive.

## Agent interaction

The extension owns the conversation record on the devserver. User messages and survey answers reach the agent through the terminal queue. An extension-owned helper gives the agent a way to acknowledge readiness, post replies and progress, and ask questions for that conversation.

Agents launch through `cs terminal new` with their configured command and `CHAN_AGENT` spawn environment. A startup brief explains this mode, with a short reminder attached to subsequent queued prompts. Terminal output remains available through **Peek**; it is not the source of the chat transcript.

## Host boundary

Reliable restoration of the binding between a Chan tab and its conversation needs the companion host bridge in `host/chan.patch`. The extension owns conversation content; the host supplies persistent instance and workspace identities plus exact terminal selection for Peek. Older hosts retain the saved-list recovery path within the current runtime.
