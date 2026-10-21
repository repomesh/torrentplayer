// SPDX-License-Identifier: MIT OR Apache-2.0

//! The error type every IPC command returns.

use serde::Serialize;
use torrentplayer_torrent::{EngineError, SourceError};
use ts_rs::TS;

/// Stable, machine-readable error categories. See `docs/ipc.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ErrorKind {
    /// An argument failed validation.
    InvalidArgument,
    /// The torrent, file, or stream does not exist.
    NotFound,
    /// The torrent link or file was refused.
    SourceRefused,
    /// The torrent contains file paths that could escape the download folder.
    UnsafeTorrent,
    /// The torrent's metadata could not be found in time (likely no peers).
    MetadataTimeout,
    /// The torrent engine failed; details are in the log.
    Engine,
    /// The search service is rate limiting, down, timed out, or sent an unreadable response.
    /// Retrying later may help.
    SearchUnavailable,
    /// The selected proxy or tunnel is not working, or traffic is held, so the kill switch blocked
    /// the request. Nothing was sent.
    KillSwitch,
    /// A network request (such as the connection check) failed.
    NetworkUnavailable,
    /// The operation was cancelled: a search replaced by a newer one, or stopped with
    /// `cancel_search`. Nothing to show the user.
    Cancelled,
    /// The audio could not be converted for the built-in player; details are in the log.
    AudioConversion,
    /// Something unexpected went wrong; details are in the log.
    Internal,
}

/// An error returned to the webview.
///
/// `message` is safe to show to users: it never contains secrets or local file paths. Engine
/// errors are logged in full and replaced with a generic message here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS, thiserror::Error)]
#[error("{message}")]
#[ts(export)]
pub struct IpcError {
    /// Error category.
    pub kind: ErrorKind,
    /// Human-readable description.
    pub message: String,
}

impl IpcError {
    /// Creates an error.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// An argument failed validation.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidArgument, message)
    }

    /// Logs `error` in full and returns a generic internal error.
    pub fn internal(error: &dyn std::fmt::Display) -> Self {
        tracing::error!(%error, "internal error");
        Self::new(
            ErrorKind::Internal,
            "Something went wrong. See the log for details.",
        )
    }
}

impl From<SourceError> for IpcError {
    fn from(error: SourceError) -> Self {
        Self::new(ErrorKind::SourceRefused, error.to_string())
    }
}

impl From<EngineError> for IpcError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Source(e) => e.into(),
            EngineError::UnsafePath(e) => Self::new(
                ErrorKind::UnsafeTorrent,
                format!("This torrent was refused because {e}."),
            ),
            EngineError::TooManyFiles => {
                Self::new(ErrorKind::UnsafeTorrent, "This torrent has too many files.")
            }
            EngineError::MetadataTimeout => Self::new(
                ErrorKind::MetadataTimeout,
                "Could not find this torrent's details. There may be no peers sharing it.",
            ),
            EngineError::UnknownTorrent => Self::new(ErrorKind::NotFound, "Unknown torrent."),
            EngineError::UnknownFile => Self::new(ErrorKind::NotFound, "Unknown file."),
            EngineError::InvalidTorrent(detail) => Self::new(
                ErrorKind::SourceRefused,
                format!("This is not a valid .torrent file ({detail})."),
            ),
            EngineError::Unsupported(message) => Self::new(ErrorKind::SourceRefused, message),
            EngineError::Engine(detail) => {
                // May contain local paths: log only.
                tracing::warn!(%detail, "torrent engine error");
                Self::new(ErrorKind::Engine, "The torrent engine reported an error.")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_details_are_not_exposed() {
        let error: IpcError =
            EngineError::Engine("failed to open C:\\Users\\someone\\secret.mp4".into()).into();
        assert_eq!(error.kind, ErrorKind::Engine);
        assert!(!error.message.contains("someone"));
    }

    #[test]
    fn serializes_with_kebab_case_kind() {
        let json = serde_json::to_value(IpcError::invalid("bad")).unwrap();
        assert_eq!(json["kind"], "invalid-argument");
        assert_eq!(json["message"], "bad");
    }
}
