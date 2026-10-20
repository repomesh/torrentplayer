// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading one small file of an added torrent (a subtitle file, a picture) without disturbing
//! the video that plays.

use std::time::Duration;

use tokio::io::AsyncReadExt as _;

use super::{Handle, StreamEngine, file_entry, swarm_err};
use crate::engine::{EngineError, TorrentId};

/// How often download progress is checked while waiting for the file.
const POLL: Duration = Duration::from_millis(250);

impl StreamEngine {
    /// Reads a whole file of an added torrent, after downloading it in the background.
    ///
    /// The file is selected at background priority (lumen-swarm `select_range`, librqbit
    /// `select_file`), so the playhead and open readers keep priority, and is read only once
    /// all its pieces are downloaded and verified; no urgent reader waits on it meanwhile.
    /// Waits as long as that takes, so callers apply a timeout; dropping the future stops
    /// waiting (the file stays selected).
    ///
    /// # Errors
    ///
    /// Returns an error if the torrent or file is unknown, the file is larger than `max_bytes`
    /// ([`EngineError::Unsupported`]), or the engine fails.
    pub async fn read_small_file(
        &self,
        id: TorrentId,
        file: usize,
        max_bytes: u64,
    ) -> Result<Vec<u8>, EngineError> {
        let entry = file_entry(&self.info(id)?, file)?;
        if entry.len > max_bytes {
            return Err(EngineError::Unsupported("The file is too large."));
        }
        match self.handle(id)? {
            Handle::Swarm(torrent) => torrent
                .select_range(file, 0..entry.len)
                .map_err(swarm_err)?,
            Handle::Rqbit(inner) => self.rqbit().await?.select_file(inner, file).await?,
        }
        let (geometry, span) = self.file_geometry(id, file)?;
        let pieces =
            geometry.pieces_overlapping(span.offset_in_torrent..span.offset_in_torrent + span.len);
        loop {
            // librqbit has no piece list until the torrent is initialized.
            let have = self.have_pieces(id).await.unwrap_or_default();
            let complete = pieces.clone().all(|p| {
                usize::try_from(p)
                    .ok()
                    .and_then(|p| have.get(p))
                    .copied()
                    .unwrap_or(false)
            });
            if complete {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        let media = self.media_source(id, file).await?;
        let reader = media
            .open()
            .await
            .map_err(|e| EngineError::Engine(format!("opening a small file: {e}")))?;
        let mut bytes = Vec::new();
        reader
            .take(max_bytes)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| EngineError::Engine(format!("reading a small file: {e}")))?;
        Ok(bytes)
    }
}
