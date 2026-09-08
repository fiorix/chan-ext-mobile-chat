//! The `/agent` command grammar, allowlist decisions, and auto-reply debounce.
//!
//! Commands are parsed only for an allowlisted sender in a recorded chat;
//! everything else is logged only, so the bridge never announces itself to
//! strangers. Auto-replies are debounced to one per chat per 60 seconds so a
//! loop cannot form.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::settings::Settings;

/// One parsed `/agent` invocation. Argument-bearing input parses as
/// [`Command::Answer`]; whether it is an answer or a prompt depends on
/// whether a question is pending, which only the caller knows — see
/// [`resolve`].
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

/// Auto-reply for an unbound chat (debounced).
pub const REPLY_UNBOUND: &str = "This chat is not bound to a Mobile Chat conversation.";

/// Auto-reply when the bound conversation has no running agent (debounced).
pub const REPLY_NO_AGENT: &str = "No agent is running for this conversation.";

const PREFIX: &str = "/agent";

/// Parse a message body. The prefix match is case-insensitive on `/agent`,
/// requires it at the start (leading whitespace is tolerated), and a mere
/// mention mid-sentence does not match. Argument-bearing input is returned as
/// [`Command::Answer`]; [`resolve`] reclassifies it as a prompt when no
/// question is pending. Returns `None` for non-commands.
pub fn parse(body: &str) -> Option<Command> {
    let body = body.trim_start();
    let head = body.get(..PREFIX.len())?;
    if !head.eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    let rest = &body[PREFIX.len()..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let arg = rest.trim();
    match arg.to_ascii_lowercase().as_str() {
        "" | "status" => Some(Command::Status),
        "cancel" => Some(Command::Cancel),
        _ => Some(Command::Answer {
            text: arg.to_string(),
        }),
    }
}

/// Answer routing, decided by whether a question is pending: with one
/// pending, argument text answers it (a bare integer selects the option by
/// position, anything else is free text — see [`answer_option`]); with none,
/// the same text is a prompt for the bound conversation.
pub fn resolve(command: Command, question_pending: bool) -> Command {
    match (command, question_pending) {
        (Command::Answer { text }, false) => Command::Prompt { text },
        (command, _) => command,
    }
}

/// A bare 1-based integer selects the pending question's option by position;
/// anything else is a free-text answer.
pub fn answer_option(text: &str) -> Option<usize> {
    let n: usize = text.parse().ok()?;
    (n > 0).then_some(n)
}

/// Default-deny allowlist decision for a sender jid. Phone-addressed senders
/// key on the jid's user part in E.164 without `+`; a LID-addressed sender
/// resolves through the recorded `lids` counterparts to the same person.
pub fn sender_allowed(settings: &Settings, sender: &str) -> bool {
    let Some(phone) = resolve_phone(settings, sender) else {
        return false;
    };
    settings.allow.contains(&phone.to_string())
}

/// The allowlist phone number a sender jid resolves to, if any.
pub fn resolve_phone<'a>(settings: &'a Settings, sender: &'a str) -> Option<&'a str> {
    if sender.ends_with("@lid") {
        settings
            .lids
            .iter()
            .find(|(_, lid)| jid_eq(lid, sender))
            .map(|(phone, _)| phone.as_str())
    } else {
        let user = sender.split('@').next().unwrap_or(sender);
        Some(user.trim_start_matches('+'))
    }
}

/// Compare jids on their user part, ignoring case and the domain.
fn jid_eq(a: &str, b: &str) -> bool {
    fn user(jid: &str) -> &str {
        jid.split('@').next().unwrap_or(jid)
    }
    user(a).eq_ignore_ascii_case(user(b))
}

/// One auto-reply per chat per [`AUTO_REPLY_DEBOUNCE_SECS`], so a reply loop
/// between the bridge and an agent cannot form. The routing agent calls
/// [`AutoReplyGate::may_send`] before sending an auto-reply.
#[derive(Debug, Default)]
pub struct AutoReplyGate {
    last: HashMap<String, Instant>,
}

impl AutoReplyGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether an auto-reply may be sent to `chat` at `now`; on `true` the
    /// debounce window restarts.
    pub fn may_send(&mut self, chat: &str, now: Instant) -> bool {
        let allowed = self.last.get(chat).is_none_or(|last| {
            now.duration_since(*last) >= Duration::from_secs(AUTO_REPLY_DEBOUNCE_SECS)
        });
        if allowed {
            self.last.insert(chat.to_string(), now);
        }
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_argument_and_status_are_status() {
        assert_eq!(parse("/agent"), Some(Command::Status));
        assert_eq!(parse("/agent status"), Some(Command::Status));
        assert_eq!(parse("/agent STATUS"), Some(Command::Status));
    }

    #[test]
    fn cancel_is_case_insensitive() {
        assert_eq!(parse("/agent cancel"), Some(Command::Cancel));
        assert_eq!(parse("/AGENT Cancel"), Some(Command::Cancel));
        assert_eq!(
            parse("  \t/Agent   CANCEL  "),
            Some(Command::Cancel),
            "leading whitespace and mixed case are tolerated"
        );
    }

    #[test]
    fn arguments_parse_as_answers_pending_resolves_them() {
        let answer = parse("/agent 1").unwrap();
        assert_eq!(answer, Command::Answer { text: "1".into() });
        assert_eq!(
            resolve(answer.clone(), true),
            Command::Answer { text: "1".into() },
            "with a question pending, 1 is an answer"
        );
        assert_eq!(
            resolve(answer, false),
            Command::Prompt { text: "1".into() },
            "without a pending question, 1 is a prompt"
        );
        assert_eq!(
            resolve(parse("/agent fix the build").unwrap(), true),
            Command::Answer {
                text: "fix the build".into()
            }
        );
    }

    #[test]
    fn a_mention_mid_sentence_does_not_match() {
        assert_eq!(parse("can you /agent do this"), None);
        assert_eq!(parse("hey /agent"), None);
        assert_eq!(
            parse("/agents hello"),
            None,
            "a longer word is not the prefix"
        );
        assert_eq!(parse("/agency"), None);
    }

    #[test]
    fn answer_options_are_1_based_integers_only() {
        assert_eq!(answer_option("1"), Some(1));
        assert_eq!(answer_option("12"), Some(12));
        assert_eq!(answer_option("one"), None);
        assert_eq!(answer_option("0"), None);
        assert_eq!(answer_option("-1"), None);
        assert_eq!(answer_option("1.5"), None);
    }

    fn settings_with_allow(phone: &str) -> Settings {
        Settings {
            version: 1,
            chats: Default::default(),
            allow: vec![phone.to_string()],
            lids: Default::default(),
        }
    }

    #[test]
    fn allowlist_is_default_deny_for_phone_addressed_senders() {
        let settings = settings_with_allow("447700900123");
        assert!(sender_allowed(&settings, "447700900123@s.whatsapp.net"));
        assert!(sender_allowed(&settings, "+447700900123@s.whatsapp.net"));
        assert!(!sender_allowed(&settings, "5511987654321@s.whatsapp.net"));
        // An empty allowlist denies everything.
        let settings = settings_with_allow("447700900123");
        let mut empty = settings.clone();
        empty.allow.clear();
        assert!(!sender_allowed(&empty, "447700900123@s.whatsapp.net"));
    }

    #[test]
    fn lid_addressed_senders_resolve_through_recorded_counterparts() {
        let mut settings = settings_with_allow("447700900123");
        assert!(
            !sender_allowed(&settings, "12345678901234@lid"),
            "an unrecorded LID resolves to nobody"
        );
        settings
            .lids
            .insert("447700900123".to_string(), "12345678901234@lid".to_string());
        assert!(sender_allowed(&settings, "12345678901234@lid"));
        assert!(
            sender_allowed(&settings, "12345678901234@lid"),
            "the counterpart resolves to the same person"
        );
        assert!(!sender_allowed(&settings, "99999999999999@lid"));
    }

    #[test]
    fn auto_reply_debounce_is_one_per_chat_per_window() {
        let mut gate = AutoReplyGate::new();
        let t0 = Instant::now();
        assert!(gate.may_send("chat-a", t0));
        assert!(
            !gate.may_send("chat-a", t0 + Duration::from_secs(59)),
            "within the window the reply is suppressed"
        );
        assert!(gate.may_send("chat-a", t0 + Duration::from_secs(60)));
        // The debounce is per chat, not global.
        assert!(gate.may_send("chat-b", t0 + Duration::from_secs(60)));
        assert!(!gate.may_send("chat-b", t0 + Duration::from_secs(61)));
    }
}
