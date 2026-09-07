# Mobile Chat

Mobile Chat is a conversation surface for interacting with terminal agents inside Chan.

## Language

**Conversation**:
An exchange of messages and questions between the user and one agent.

**Chat tab**:
A Chan tab dedicated to one Mobile Chat conversation.

Implementation now includes a separate companion Chan patch for restoration and Peek. Real CLI testing established native initial prompts for Claude/Codex and explicit Connect chat for Kimi. Replies, questions, and delivery are gated by the helper; one queued user message is released per completed turn.
