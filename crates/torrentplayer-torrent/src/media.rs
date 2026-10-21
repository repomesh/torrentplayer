// SPDX-License-Identifier: MIT OR Apache-2.0

//! File classification by extension. Phase 3 adds header probing on top of this.

use std::path::Path;

/// What kind of file a torrent entry is, judged by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// A video container TorrentPlayer can try to play.
    Video,
    /// Music, podcasts, audiobooks: audio TorrentPlayer can try to play.
    Audio,
    /// A subtitle file.
    Subtitle,
    /// Lecture notes, slides, and similar documents.
    Document,
    /// Anything else.
    Other,
}

const VIDEO: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "avi", "mov", "flv", "ts", "m2ts", "mts", "ogv", "wmv", "mpg",
    "mpeg",
];
const AUDIO: &[&str] = &[
    "mp3", "flac", "m4a", "m4b", "aac", "ogg", "oga", "opus", "wav", "wma", "aif", "aiff", "ape",
    "alac", "mka",
];
const SUBTITLE: &[&str] = &["srt", "vtt", "ass", "ssa", "sub"];
const DOCUMENT: &[&str] = &[
    "pdf", "txt", "md", "html", "htm", "epub", "pptx", "ppt", "docx",
];

impl FileKind {
    /// Classifies a file name.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()) else {
            return Self::Other;
        };
        let ext = ext.to_ascii_lowercase();
        if VIDEO.contains(&ext.as_str()) {
            Self::Video
        } else if AUDIO.contains(&ext.as_str()) {
            Self::Audio
        } else if SUBTITLE.contains(&ext.as_str()) {
            Self::Subtitle
        } else if DOCUMENT.contains(&ext.as_str()) {
            Self::Document
        } else {
            Self::Other
        }
    }

    /// Whether the player can try to play it: video or audio.
    #[must_use]
    pub fn is_playable(self) -> bool {
        matches!(self, Self::Video | Self::Audio)
    }
}

/// MIME type to send for a video or audio file, so the webview can pick a decoder.
#[must_use]
pub fn mime_type(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("ogv") => "video/ogg",
        Some("mov") => "video/quicktime",
        Some("avi") => "video/x-msvideo",
        Some("ts" | "m2ts" | "mts") => "video/mp2t",
        Some("mpg" | "mpeg") => "video/mpeg",
        Some("wmv") => "video/x-ms-wmv",
        Some("flv") => "video/x-flv",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("m4a" | "m4b" | "alac") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("ogg" | "oga") => "audio/ogg",
        Some("opus") => "audio/ogg; codecs=opus",
        Some("wav") => "audio/wav",
        Some("wma") => "audio/x-ms-wma",
        Some("aif" | "aiff") => "audio/aiff",
        Some("mka") => "audio/x-matroska",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension_case_insensitively() {
        assert_eq!(FileKind::from_name("Lecture 01.MP4"), FileKind::Video);
        assert_eq!(FileKind::from_name("dir/lec.mkv"), FileKind::Video);
        assert_eq!(FileKind::from_name("lec01.en.srt"), FileKind::Subtitle);
        assert_eq!(FileKind::from_name("notes.PDF"), FileKind::Document);
        assert_eq!(FileKind::from_name("README"), FileKind::Other);
        assert_eq!(FileKind::from_name("archive.zip"), FileKind::Other);
        assert_eq!(FileKind::from_name("01 - Intro.FLAC"), FileKind::Audio);
        assert_eq!(FileKind::from_name("book.m4b"), FileKind::Audio);
        assert!(FileKind::Audio.is_playable() && FileKind::Video.is_playable());
        assert!(!FileKind::Subtitle.is_playable());
    }

    #[test]
    fn mime_types() {
        assert_eq!(mime_type("a.mp4"), "video/mp4");
        assert_eq!(mime_type("a.WEBM"), "video/webm");
        assert_eq!(mime_type("a.mkv"), "video/x-matroska");
        assert_eq!(mime_type("song.mp3"), "audio/mpeg");
        assert_eq!(mime_type("song.FLAC"), "audio/flac");
        assert_eq!(mime_type("song.m4a"), "audio/mp4");
        assert_eq!(mime_type("a"), "application/octet-stream");
    }
}
