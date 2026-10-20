// SPDX-License-Identifier: MIT OR Apache-2.0

//! Seekable readers over torrent files.

use std::future::Future as _;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tokio::sync::futures::OwnedNotified;
use tokio::task::JoinHandle;

use crate::sched::{Interest, InterestId, InterestKind};
use crate::torrent::Shared;

/// Largest single read from the piece store.
const MAX_READ: usize = 256 * 1024;

enum State {
    Idle,
    Waiting(Pin<Box<OwnedNotified>>),
    Reading(JoinHandle<io::Result<Vec<u8>>>),
}

/// An `AsyncRead + AsyncSeek` view of one file in a torrent.
///
/// While it exists, the data just after its position is the most urgent work in the torrent.
/// Reads wait until the needed piece is downloaded and verified; seeking re-prioritizes
/// immediately and cancels requests that are no longer needed.
pub struct FileReader {
    shared: Arc<Shared>,
    id: InterestId,
    file_offset: u64,
    len: u64,
    pos: u64,
    state: State,
}

impl std::fmt::Debug for FileReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileReader")
            .field("pos", &self.pos)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl FileReader {
    pub(crate) fn new(shared: Arc<Shared>, id: InterestId, file_offset: u64, len: u64) -> Self {
        let reader = Self {
            shared,
            id,
            file_offset,
            len,
            pos: 0,
            state: State::Idle,
        };
        reader.register(true);
        reader
    }

    /// File length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn register(&self, seek: bool) {
        self.shared.sched().set_interest(
            self.id,
            Interest {
                kind: InterestKind::Reader,
                file_offset: self.file_offset,
                file_len: self.len,
                position: self.pos,
                byte_rate: None,
            },
        );
        self.shared.request_replan(seek);
    }
}

impl Drop for FileReader {
    fn drop(&mut self) {
        self.shared.sched().remove_interest(self.id);
        self.shared.request_replan(false);
    }
}

impl AsyncRead for FileReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            match &mut self.state {
                State::Idle => {
                    if self.pos >= self.len || buf.remaining() == 0 {
                        return Poll::Ready(Ok(()));
                    }
                    let meta = &self.shared.meta;
                    let abs = self.file_offset + self.pos;
                    let piece = meta.piece_at(abs);
                    if self.shared.sched().have(piece) {
                        let in_piece = abs - meta.piece_offset(piece);
                        let piece_left = u64::from(meta.piece_size(piece)) - in_piece;
                        let n = piece_left
                            .min(self.len - self.pos)
                            .min(buf.remaining() as u64)
                            .min(MAX_READ as u64);
                        let n = usize::try_from(n).unwrap_or(MAX_READ);
                        let in_piece = u32::try_from(in_piece).unwrap_or(u32::MAX);
                        let shared = Arc::clone(&self.shared);
                        self.state = State::Reading(tokio::task::spawn_blocking(move || {
                            let mut out = vec![0u8; n];
                            shared.store.read(piece, in_piece, &mut out)?;
                            Ok(out)
                        }));
                    } else {
                        // Register for the wake-up before re-checking, so a piece finishing in
                        // between is not missed.
                        let mut notified =
                            Box::pin(Arc::clone(&self.shared.piece_done).notified_owned());
                        notified.as_mut().enable();
                        if !self.shared.sched().have(piece) {
                            self.state = State::Waiting(notified);
                        }
                    }
                }
                State::Waiting(notified) => {
                    if notified.as_mut().poll(cx).is_pending() {
                        return Poll::Pending;
                    }
                    self.state = State::Idle;
                }
                State::Reading(task) => {
                    let result = match Pin::new(task).poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(r) => r,
                    };
                    self.state = State::Idle;
                    let bytes = result.map_err(io::Error::other)??;
                    buf.put_slice(&bytes);
                    self.pos += bytes.len() as u64;
                    self.register(false);
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

impl AsyncSeek for FileReader {
    fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let target = match position {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        }
        .filter(|&p| p <= self.len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek out of range"))?;
        if target != self.pos {
            self.pos = target;
            // Abandon any wait or read for the old position.
            self.state = State::Idle;
            self.register(true);
        }
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.pos))
    }
}
