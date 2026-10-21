// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading a container header through a seekable reader.

use std::io::{self, SeekFrom};

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncSeek, AsyncSeekExt as _};

use crate::sparse::{Sparse, Stop};
use crate::tracks::TrackReport;
use crate::{mkv, mp4};

/// Bytes read at a time. Headers are small and usually sit together, so one read covers many.
const BLOCK: u64 = 64 * 1024;
/// Most reads per probe, so at most 48 × 64 KiB = 3 MiB is read. A Matroska file needs one or
/// two; an MP4 file about one per track plus one or two for a `moov` box at the end.
pub const MAX_READS: usize = 48;

/// Why probing failed.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The file is not MP4, MOV, Matroska, or `WebM`.
    #[error("not an MP4, MOV, Matroska, or WebM file")]
    UnsupportedContainer,
    /// The container is malformed or exceeds a limit.
    #[error("the container is malformed: {0}")]
    Malformed(&'static str),
    /// The header was spread over more than [`MAX_READS`] reads.
    #[error("the container header needs too many reads")]
    TooManyReads,
    /// Reading failed.
    #[error("reading the file failed: {0}")]
    Io(#[from] io::Error),
}

/// Lists the audio and subtitle tracks of the `len`-byte file behind `reader`, reading only the
/// container headers. Waits as long as the reader does (a torrent reader waits for its pieces),
/// so callers should apply a timeout.
///
/// # Errors
///
/// Returns an error if the container is not supported or is malformed, or reading fails.
pub async fn probe_tracks<R>(reader: &mut R, len: u64) -> Result<TrackReport, ProbeError>
where
    R: AsyncRead + AsyncSeek + Unpin + ?Sized,
{
    let mut data = Sparse::new(len);
    let mut reads = 0;
    loop {
        let range = match parse(&data) {
            Ok(report) => return Ok(report),
            Err(Stop::Need(range)) => range,
            Err(Stop::Invalid(why)) => return Err(ProbeError::Malformed(why)),
            Err(Stop::Unsupported) => return Err(ProbeError::UnsupportedContainer),
        };
        if reads == MAX_READS {
            return Err(ProbeError::TooManyReads);
        }
        reads += 1;
        let want = (range.end - range.start).max(BLOCK).min(len - range.start);
        reader.seek(SeekFrom::Start(range.start)).await?;
        let bytes = read_up_to(reader, want).await?;
        if (bytes.len() as u64) < range.end - range.start {
            return Err(ProbeError::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        data.insert(range.start, bytes);
    }
}

/// Picks the parser by the first bytes of the file.
fn parse(data: &Sparse) -> Result<TrackReport, Stop> {
    let head = data.get(0, 8).map_err(|stop| match stop {
        Stop::Invalid(_) => Stop::Unsupported,
        other => other,
    })?;
    if mkv::sniff(head) {
        mkv::parse(data)
    } else if mp4::sniff(head) {
        mp4::parse(data)
    } else {
        Err(Stop::Unsupported)
    }
}

/// Reads up to `len` bytes, stopping early only at the end of the file.
async fn read_up_to<R>(reader: &mut R, len: u64) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut out = Vec::new();
    reader.take(len).read_to_end(&mut out).await?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use proptest::prelude::*;
    use tokio::io::ReadBuf;

    use super::*;
    use crate::audio::AudioCodec;
    use crate::mkv::tests as mk;
    use crate::mp4::tests as m4;
    use crate::tracks::Container;

    /// Counts reads and the bytes they return.
    struct Counting {
        inner: Cursor<Vec<u8>>,
        bytes: usize,
        seeks: usize,
    }

    impl Counting {
        fn new(file: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(file),
                bytes: 0,
                seeks: 0,
            }
        }
    }

    impl AsyncRead for Counting {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let before = buf.filled().len();
            let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
            self.bytes += buf.filled().len() - before;
            poll
        }
    }

    impl AsyncSeek for Counting {
        fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
            self.seeks += 1;
            Pin::new(&mut self.inner).start_seek(position)
        }

        fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
            Pin::new(&mut self.inner).poll_complete(cx)
        }
    }

    async fn probe(file: Vec<u8>) -> (Result<TrackReport, ProbeError>, Counting) {
        let len = file.len() as u64;
        let mut reader = Counting::new(file);
        (probe_tracks(&mut reader, len).await, reader)
    }

    #[tokio::test]
    async fn probes_an_mp4_with_moov_at_the_end_without_reading_the_media() {
        let mdat_len = 40 * 1024 * 1024;
        let video = m4::trak(b"vide", true, &m4::bx(b"avc1", &[0; 78]), 2 * 1024 * 1024);
        let audio = m4::trak(b"soun", true, &m4::sound_entry(b"ec-3", &[]), 1024);
        let file = [
            m4::ftyp(),
            m4::bx(b"mdat", &vec![0; mdat_len]),
            m4::moov(&[video, audio]),
        ]
        .concat();
        let (report, reader) = probe(file).await;
        let report = report.unwrap();
        assert_eq!(report.container, Container::Mp4);
        assert_eq!(report.primary().unwrap().codec, AudioCodec::Eac3);
        // The head, the moov header, and the audio track after the video's 2 MiB table.
        assert!(reader.seeks <= 4, "{} reads", reader.seeks);
        assert!(reader.bytes <= 4 * 64 * 1024, "{} bytes read", reader.bytes);
    }

    #[tokio::test]
    async fn probes_matroska_in_one_read() {
        let file = [
            mk::file(&[mk::el(
                0x1654_AE6B,
                &[
                    mk::track(2, "A_DTS", Some(true)),
                    mk::track(2, "A_AAC", Some(false)),
                ]
                .concat(),
            )]),
            vec![0; 1024 * 1024],
        ]
        .concat();
        let (report, reader) = probe(file).await;
        let report = report.unwrap();
        assert_eq!(report.container, Container::Matroska);
        assert_eq!(report.primary().unwrap().codec, AudioCodec::Dts);
        assert_eq!(
            report.playable_alternative().unwrap().codec,
            AudioCodec::Aac
        );
        assert_eq!(reader.seeks, 1);
    }

    #[tokio::test]
    async fn reports_unsupported_and_malformed_files() {
        let avi = [
            b"RIFF".to_vec(),
            vec![0; 4],
            b"AVI LIST".to_vec(),
            vec![0; 100],
        ]
        .concat();
        assert!(matches!(
            probe(avi).await.0,
            Err(ProbeError::UnsupportedContainer)
        ));
        assert!(matches!(
            probe(vec![0x47; 4]).await.0,
            Err(ProbeError::UnsupportedContainer)
        ));
        assert!(matches!(
            probe(Vec::new()).await.0,
            Err(ProbeError::UnsupportedContainer)
        ));
        let truncated = [m4::ftyp(), m4::bx(b"mdat", &[0; 10])[..12].to_vec()].concat();
        assert!(matches!(
            probe(truncated).await.0,
            Err(ProbeError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn gives_up_after_too_many_reads() {
        // Many tracks, each behind a large sample table, need one read each.
        let traks: Vec<Vec<u8>> = (0..MAX_READS + 4)
            .map(|_| m4::trak(b"vide", true, &m4::bx(b"avc1", &[0; 8]), 70 * 1024))
            .collect();
        let file = [m4::ftyp(), m4::moov(&traks)].concat();
        let (report, reader) = probe(file).await;
        assert!(
            matches!(report, Err(ProbeError::TooManyReads)),
            "{report:?}"
        );
        assert_eq!(reader.seeks, MAX_READS);
    }

    #[tokio::test]
    async fn a_short_reader_is_an_error() {
        // The reader ends before the length it was said to have.
        let file = [m4::ftyp(), m4::bx(b"mdat", &[0; 100])].concat();
        let mut reader = Cursor::new(file[..20].to_vec());
        let result = probe_tracks(&mut reader, file.len() as u64).await;
        assert!(matches!(result, Err(ProbeError::Io(_))), "{result:?}");
    }

    fn valid_files() -> Vec<Vec<u8>> {
        vec![
            [
                m4::ftyp(),
                m4::moov(&[m4::trak(
                    b"soun",
                    true,
                    &m4::sound_entry(b"mp4a", &[m4::esds(0x40)]),
                    64,
                )]),
            ]
            .concat(),
            mk::file(&[mk::el(
                0x1654_AE6B,
                &[
                    mk::track(1, "V_VP9", None),
                    mk::track(2, "A_OPUS", Some(true)),
                ]
                .concat(),
            )]),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Any bytes after a valid magic number: an answer or an error, never a panic.
        #[test]
        fn arbitrary_bytes_never_panic(magic in 0usize..3, tail in proptest::collection::vec(any::<u8>(), 0..600)) {
            let head: &[u8] = match magic {
                0 => &[0x1A, 0x45, 0xDF, 0xA3],
                1 => b"\0\0\0\x18ftyp",
                _ => b"\0\0\0\x08moov",
            };
            let data = Sparse::whole([head, &tail].concat());
            let _ = parse(&data);
        }

        /// Valid files with bytes changed, or cut short: an answer or an error, never a panic.
        #[test]
        fn mutated_files_never_panic(
            which in 0usize..2,
            edits in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..8),
            cut in any::<usize>(),
        ) {
            let mut file = valid_files().swap_remove(which);
            for (at, value) in edits {
                let at = at % file.len();
                file[at] = value;
            }
            let _ = parse(&Sparse::whole(file.clone()));
            file.truncate(cut % (file.len() + 1));
            let _ = parse(&Sparse::whole(file));
        }
    }

    #[test]
    fn valid_fixtures_parse() {
        for file in valid_files() {
            assert_eq!(parse(&Sparse::whole(file)).unwrap().audio.len(), 1);
        }
    }
}
