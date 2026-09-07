//! The `/agent` command grammar, allowlist decisions, and auto-reply debounce.
//!
//! Commands are parsed only for an allowlisted sender in a recorded chat;
//! everything else is logged only, so the bridge never announces itself to
//! strangers. Auto-replies are debounced to one per chat per 60 seconds so a
//! loop cannot form.

/// One parsed `/agent` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `/agent` or `/agent status`: report the bound conversation's state.
    Status,
    /// `/agent <prompt>`: send `text` to the bound conversation.
    Prompt { text: String },
    /// `/agent <answer>` where a bare integer selects an option by position.
    Answer { text: String },
    /// `/agent cancel`: cancel the oldest pending question.
    Cancel,
}

/// Debounce window for auto-replies, one per chat.
pub const AUTO_REPLY_DEBOUNCE_SECS: u64 = 60;

/// Parse a message body. The prefix match is case-insensitive on `/agent`,
/// requires it at the start (leading whitespace is tolerated), and a mere
/// mention mid-sentence does not match. Returns `None` for non-commands.
pub fn parse(body: &str) -> Option<Command> {
    let body = body.trim_start();
    let rest = body.strip_prefix("/agent")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let arg = rest.trim();
    match arg.to_ascii_lowercase().as_str() {
        "" => Some(Command::Status),
        "status" => Some(Command::Status),
        "cancel" => Some(Command::Cancel),
        _ => Some(Command::Prompt {
            text: arg.to_string(),
        }),
    }
}
