//! Outbound routing: the per-conversation forwarder, entry diffing, and
//! question rendering.
//!
//! One forwarder task per bound conversation subscribes to the tenant's
//! `updates` broadcast, re-reads the conversation, and forwards assistant
//! entries after `last_forwarded_entry`, which persists in `settings.json` so
//! a restart does not replay the transcript. A `Lagged` broadcast is safe: the
//! forwarder always diffs by entry id.
//!
//! Question rendering (`whatsapp_text` for the body):
//!
//! ```text
//! *Question from claude*
//! Where should we run this?
//!
//! 1. Local
//! 2. Remote
//!
//! Reply /agent 1, or /agent <your answer>, or /agent cancel
//! ```
