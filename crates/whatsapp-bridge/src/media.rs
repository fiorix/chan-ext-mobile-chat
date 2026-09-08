//! Type and size gating, destination paths, streaming download.
//!
//! A media attachment is stored only when its kind is in the configured
//! `media` list and its size does not exceed `media_max_bytes`; the routing
//! agent then streams the bytes through `WaSink::download` so memory stays
//! flat regardless of file size. Stored paths are
//! `media/<kind>/<unix>-<msgid>.<ext>`, relative to the chat directory so
//! logs stay portable.
//!
//! This module is pure path and gating logic: [`plan`] decides and builds the
//! destination, the caller creates the parent directory and downloads.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::BridgeConfig;

/// The media kinds the bridge recognizes, matching the `media` config list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Document,
    Sticker,
}

impl MediaKind {
    /// Config-table spelling (`"image"`, `"video"`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Sticker => "sticker",
        }
    }

    /// Parse the config-table spelling.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "image" => Some(Self::Image),
            "video" => Some(Self::Video),
            "audio" => Some(Self::Audio),
            "document" => Some(Self::Document),
            "sticker" => Some(Self::Sticker),
            _ => None,
        }
    }

    /// Destination subdirectory under `media/`, matching the on-disk layout.
    pub fn subdir(self) -> &'static str {
        match self {
            Self::Image => "images",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "documents",
            Self::Sticker => "stickers",
        }
    }
}

/// A decided media destination, what the routing agent needs to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDest {
    /// Path relative to the chat directory, as recorded in the log.
    pub rel: String,
    /// Absolute path the sink streams into.
    pub abs: PathBuf,
}

impl MediaDest {
    /// Create the destination's parent directory.
    pub fn create_parent(&self) -> Result<()> {
        let parent = self
            .abs
            .parent()
            .context("media destination has no parent directory")?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))
    }
}

/// Whether an attachment of `kind` may be stored under `config`: the kind
/// must be listed and a known size must not exceed `media_max_bytes`. An
/// unknown size (`None`) passes; the caller can compare the byte count the
/// sink returns against the cap afterwards.
pub fn allowed(config: &BridgeConfig, kind: MediaKind, bytes: Option<u64>) -> bool {
    config.media.iter().any(|listed| listed == kind.name())
        && bytes.is_none_or(|size| size <= config.media_max_bytes)
}

/// What is known about an attachment before downloading: declared mime,
/// WhatsApp file name and advertised size. Any of them may be unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaInfo<'a> {
    pub mime: Option<&'a str>,
    pub file_name: Option<&'a str>,
    pub bytes: Option<u64>,
}

/// Decide and build the media destination for one attachment. Returns `None`
/// when gating rejects it; the mime and file name feed the extension guess.
pub fn plan(
    config: &BridgeConfig,
    chat_dir: &Path,
    kind: MediaKind,
    unix: u64,
    msg_id: &str,
    info: &MediaInfo<'_>,
) -> Option<MediaDest> {
    if !allowed(config, kind, info.bytes) {
        return None;
    }
    let rel = format!(
        "media/{}/{}-{}.{}",
        kind.subdir(),
        unix,
        sanitize_id(msg_id),
        extension(info.mime, info.file_name)
    );
    Some(MediaDest {
        abs: chat_dir.join(&rel),
        rel,
    })
}

/// File extension guessed from the mime type, or the WhatsApp file name when
/// no mime is available, falling back to `bin`.
fn extension(mime: Option<&str>, file_name: Option<&str>) -> String {
    if let Some(mime) = mime
        && let Some(ext) = mime_ext(mime)
    {
        return ext;
    }
    if let Some(ext) = file_name
        .and_then(|name| name.rsplit('.').next())
        .filter(|ext| {
            !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric())
        })
    {
        return ext.to_lowercase();
    }
    "bin".to_string()
}

fn mime_ext(mime: &str) -> Option<String> {
    let ext = match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/ogg" => "ogg",
        "audio/opus" => "opus",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "application/zip" => "zip",
        _ => {
            // Generic `type/subtype`: use a safe alphanumeric subtype.
            let (_, subtype) = mime.split_once('/')?;
            if subtype.chars().all(|c| c.is_ascii_alphanumeric()) && !subtype.is_empty() {
                subtype
            } else {
                return None;
            }
        }
    };
    Some(ext.to_string())
}

/// Message ids go into file names; keep them filesystem-safe.
fn sanitize_id(msg_id: &str) -> String {
    let mut out = String::with_capacity(msg_id.len());
    let mut dashed = false;
    for ch in msg_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
            dashed = false;
        } else if !dashed {
            out.push('-');
            dashed = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "msg".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BridgeConfig;

    fn config(media: &[&str], max: u64) -> BridgeConfig {
        BridgeConfig {
            log_max_bytes: 1024 * 1024,
            log_keep: 10,
            media_max_bytes: max,
            media: media.iter().map(|s| s.to_string()).collect(),
            reply_progress: true,
        }
    }

    #[test]
    fn kinds_parse_and_map_to_layout_subdirectories() {
        assert_eq!(MediaKind::from_name("image"), Some(MediaKind::Image));
        assert_eq!(MediaKind::from_name("voice"), None);
        assert_eq!(MediaKind::Video.subdir(), "video");
        assert_eq!(MediaKind::Audio.subdir(), "audio");
        assert_eq!(MediaKind::Document.subdir(), "documents");
        assert_eq!(MediaKind::Sticker.subdir(), "stickers");
        assert_eq!(MediaKind::Image.subdir(), "images");
    }

    #[test]
    fn gating_by_kind_and_size() {
        let cfg = config(&["image", "document"], 1000);
        assert!(allowed(&cfg, MediaKind::Image, Some(1000)));
        assert!(!allowed(&cfg, MediaKind::Image, Some(1001)), "over the cap");
        assert!(allowed(&cfg, MediaKind::Image, None), "unknown size passes");
        assert!(
            !allowed(&cfg, MediaKind::Video, Some(10)),
            "kind not listed"
        );
    }

    #[test]
    fn plan_builds_the_layout_path_and_guesses_extensions() {
        let cfg = config(&["image", "video", "audio", "document", "sticker"], 1024);
        let dir = Path::new("/chats/alex-0123abcd");

        let dest = plan(
            &cfg,
            dir,
            MediaKind::Image,
            1_757_248_496,
            "3EB0FA7B...",
            &MediaInfo {
                mime: Some("image/jpeg"),
                file_name: None,
                bytes: Some(500),
            },
        )
        .unwrap();
        assert_eq!(dest.rel, "media/images/1757248496-3EB0FA7B.jpg");
        assert_eq!(dest.abs, dir.join(&dest.rel));

        // Extension from the file name when no mime is available.
        let dest = plan(
            &cfg,
            dir,
            MediaKind::Document,
            1_757_248_496,
            "3EB0",
            &MediaInfo {
                mime: None,
                file_name: Some("Quarterly Report.PDF"),
                bytes: None,
            },
        )
        .unwrap();
        assert_eq!(dest.rel, "media/documents/1757248496-3EB0.pdf");

        // Unknown mime falls back to the file name, then to `bin`.
        let dest = plan(
            &cfg,
            dir,
            MediaKind::Video,
            1,
            "abc",
            &MediaInfo {
                mime: Some("application/octet-stream"),
                file_name: Some("clip.mp4"),
                bytes: None,
            },
        )
        .unwrap();
        assert_eq!(dest.rel, "media/video/1-abc.mp4");
        let dest = plan(&cfg, dir, MediaKind::Audio, 1, "abc", &MediaInfo::default()).unwrap();
        assert_eq!(dest.rel, "media/audio/1-abc.bin");

        // A gated-out attachment produces no destination: kind not listed,
        // then advertised size over the cap.
        let cfg_images_only = config(&["image"], 1024);
        assert!(
            plan(
                &cfg_images_only,
                dir,
                MediaKind::Sticker,
                1,
                "abc",
                &MediaInfo::default()
            )
            .is_none()
        );
        assert!(
            plan(
                &cfg,
                dir,
                MediaKind::Image,
                1,
                "abc",
                &MediaInfo {
                    bytes: Some(1025),
                    ..MediaInfo::default()
                }
            )
            .is_none()
        );
    }

    #[test]
    fn message_ids_are_sanitized_for_file_names() {
        let cfg = config(&["image"], 1024);
        let dir = Path::new("/chats/alex-0123abcd");
        let dest = plan(
            &cfg,
            dir,
            MediaKind::Image,
            1,
            "BAE5:/fwd\\x",
            &MediaInfo {
                mime: Some("image/png"),
                ..MediaInfo::default()
            },
        )
        .unwrap();
        assert_eq!(dest.rel, "media/images/1-BAE5-fwd-x.png");
    }
}
