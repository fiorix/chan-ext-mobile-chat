//! Inbound routing: binding resolution, envelope construction, `Chat::send`.
//!
//! A bound chat resolves to a live tenant then conversation; unbound or
//! stopped agents produce one of the debounced auto-replies rather than
//! silence. The envelope handed to `Chat::send` names the sender and chat and
//! states plainly that the text is untrusted third-party input, including the
//! chat log path so the agent can read context for itself.

/// The fixed framing every WhatsApp prompt is wrapped in before it reaches an
/// agent that runs with permission checks bypassed.
pub const UNTRUSTED_PREFIX: &str = "[WhatsApp] Untrusted message";
