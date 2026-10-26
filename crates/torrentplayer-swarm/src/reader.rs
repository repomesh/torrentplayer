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
use tokio_util::sync::WaitForCancellationFutureOwned;

use crate::metainfo::Metainfo;
use crate::sched::{Interest, InterestId, InterestKind};
use crate::torrent::Shared;

/// Largest single read from the piece store.
const MAX_READ: usize = 256 * 1024;

enum State {
    Idle,
    /// Until a piece is verified, or the torrent stops.
    Waiting(
        Pin<Box<OwnedNotified>>,
        Pin<Box<WaitForCancellationFutureOwned>>,
    ),
    Reading(JoinHandle<io::Result<Vec<u8>>>),
}

/// An `AsyncRead + AsyncSeek` view of one file in a torrent.
///
/// While it exists, the data just after its position is the most urgent work in the torrent.
/// Reads wait until the needed piece is downloaded and verified; seeking re-prioritizes
/// immediately and cancels requests that are no longer needed. Once the torrent stops, a read
/// that would wait fails with [`io::ErrorKind::ConnectionAborted`].
///
/// A *stored-only* reader ([`Torrent::stored_reader`](crate::Torrent::stored_reader)) changes
/// nothing about what is downloaded: it reads pieces already stored and fails at once with
/// [`io::ErrorKind::WouldBlock`] where one is missing.
pub struct FileReader {
    shared: Arc<Shared>,
    id: InterestId,
    file_offset: u64,
    len: u64,
    pos: u64,
    state: State,
    stored_only: bool,
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
    pub(crate) fn new(
        shared: Arc<Shared>,
        id: InterestId,
        file_offset: u64,
        len: u64,
        stored_only: bool,
    ) -> Self {
        let reader = Self {
            shared,
            id,
            file_offset,
            len,
            pos: 0,
            state: State::Idle,
            stored_only,
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

    /// Records the reader's position and asks the driver to re-plan now.
    fn register(&self, seek: bool) {
        if self.stored_only {
            return;
        }
        self.set_interest();
        self.shared.request_replan(seek);
    }

    /// Records the reader's position for the driver's next plan.
    fn set_interest(&self) {
        if self.stored_only {
            return;
        }
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
    }

    /// Moves the position past `n` bytes just read. Only entering a new piece re-plans at once:
    /// within a piece the urgent window barely moves, and the driver's tick re-plans anyway.
    fn advance(&mut self, n: u64) {
        let before = self.file_offset + self.pos;
        self.pos += n;
        if enters_new_piece(&self.shared.meta, before, self.file_offset + self.pos) {
            self.register(false);
        } else {
            self.set_interest();
        }
    }
}

/// Whether a read from absolute offset `before` up to `after` ends in a different piece.
fn enters_new_piece(meta: &Metainfo, before: u64, after: u64) -> bool {
    meta.piece_at(before) != meta.piece_at(after)
}

impl Drop for FileReader {
    fn drop(&mut self) {
        if self.stored_only {
            return;
        }
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
                            shared.read_piece(piece, in_piece, &mut out)?;
                            Ok(out)
                        }));
                    } else if self.stored_only {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "piece not downloaded",
                        )));
                    } else {
                        // Register for the wake-up before re-checking, so a piece finishing in
                        // between is not missed.
                        let mut notified =
                            Box::pin(Arc::clone(&self.shared.piece_done).notified_owned());
                        notified.as_mut().enable();
                        if !self.shared.sched().have(piece) {
                            let stopped = Box::pin(self.shared.cancel.clone().cancelled_owned());
                            self.state = State::Waiting(notified, stopped);
                        }
                    }
                }
                State::Waiting(notified, stopped) => {
                    // A stopped torrent downloads nothing more: fail instead of waiting forever
                    // (and keeping the torrent's state alive).
                    if stopped.as_mut().poll(cx).is_ready() {
                        self.state = State::Idle;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "the torrent was stopped",
                        )));
                    }
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
                    self.advance(bytes.len() as u64);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A torrent of one 64 KiB file in four 16 KiB pieces.
    fn four_pieces() -> Metainfo {
        let mut bytes =
            b"d4:infod6:lengthi65536e4:name5:a.mp412:piece lengthi16384e6:pieces80:".to_vec();
        bytes.extend_from_slice(&[0; 80]);
        bytes.extend_from_slice(b"ee");
        Metainfo::from_bytes(&bytes).unwrap()
    }

    #[test]
    fn only_reads_entering_a_new_piece_re_plan() {
        let meta = four_pieces();
        // Continuous reads within the first piece.
        assert!(!enters_new_piece(&meta, 0, 4096));
        assert!(!enters_new_piece(&meta, 4096, 16_383));
        // A read ending at a piece boundary leaves the reader at the start of the next piece.
        assert!(enters_new_piece(&meta, 12_288, 16_384));
        assert!(!enters_new_piece(&meta, 16_384, 20_480));
        // Reading to the end of the file stays in the last piece.
        assert!(!enters_new_piece(&meta, 61_440, 65_536));
    }

    #[test]
    fn sequential_small_reads_re_plan_once_per_piece() {
        let meta = four_pieces();
        // 4 KiB reads, each stopping at a piece boundary as `poll_read` does.
        let mut pos = 0u64;
        let mut replans = 0;
        while pos < meta.total_len {
            let piece = meta.piece_at(pos);
            let piece_end = meta.piece_offset(piece) + u64::from(meta.piece_size(piece));
            let after = (pos + 4096).min(piece_end);
            if enters_new_piece(&meta, pos, after) {
                replans += 1;
            }
            pos = after;
        }
        // Sixteen reads; one re-plan entering each of pieces 1 to 3.
        assert_eq!(replans, 3);
    }
}
