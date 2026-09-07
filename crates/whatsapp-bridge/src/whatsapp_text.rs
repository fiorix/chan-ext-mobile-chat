//! Markdown to WhatsApp formatting and chunking.
//!
//! Agent bodies are Markdown; WhatsApp has its own flavour (`*bold*`,
//! `_italic_`, `~strike~`, triple-backtick code). Rendering maps between the
//! two, then chunks at 3500 characters, splitting on paragraph, then line,
//! then a hard boundary, with `(1/3)` markers. A fenced block is never split
//! mid-fence: close it, mark the chunk, and reopen in the next.

/// Maximum characters per outbound message.
pub const CHUNK_MAX_CHARS: usize = 3500;
