// SPDX-License-Identifier: MIT OR Apache-2.0

//! The BitTorrent peer wire protocol (BEP 3): handshake and message framing.
//!
//! Everything a peer sends is untrusted, so parsing is strict: every length is checked before
//! it is used, frames have a hard size limit, and unknown messages are skipped rather than
//! guessed at.

use bytes::Bytes;

/// Length of the handshake.
pub(crate) const HANDSHAKE_LEN: usize = 68;
const PROTOCOL: &[u8; 19] = b"BitTorrent protocol";
/// Reserved byte and bit advertising the extension protocol (BEP 10), which carries peer
/// exchange (BEP 11).
const EXTENSION_BYTE: usize = 25;
const EXTENSION_BIT: u8 = 0x10;
/// The extended message id TorrentPlayer assigns to `ut_pex` in its extended handshake: peers send
/// peer-exchange messages to TorrentPlayer with this id.
pub(crate) const UT_PEX_ID: u8 = 1;
/// TorrentPlayer's extended handshake: it accepts `ut_pex` (and sends no extended messages itself).
const EXTENDED_HANDSHAKE: &[u8] = b"d1:md6:ut_pexi1ee1:v13:TorrentPlayere";

/// Largest frame accepted besides a bitfield: a 16 KiB block plus headers, with headroom for
/// peers that send larger extension messages.
pub(crate) const MAX_FRAME: usize = 256 * 1024;

/// Builds our handshake.
pub(crate) fn handshake(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> [u8; HANDSHAKE_LEN] {
    let mut h = [0u8; HANDSHAKE_LEN];
    h[0] = 19;
    h[1..20].copy_from_slice(PROTOCOL);
    // Reserved bytes 20..28: only the extension protocol bit.
    h[EXTENSION_BYTE] = EXTENSION_BIT;
    h[28..48].copy_from_slice(info_hash);
    h[48..68].copy_from_slice(peer_id);
    h
}

/// Whether a handshake advertises the extension protocol.
pub(crate) fn supports_extensions(h: &[u8; HANDSHAKE_LEN]) -> bool {
    h[EXTENSION_BYTE] & EXTENSION_BIT != 0
}

/// Checks a peer's handshake and returns its peer id.
pub(crate) fn check_handshake(
    h: &[u8; HANDSHAKE_LEN],
    info_hash: &[u8; 20],
) -> Result<[u8; 20], WireError> {
    if h[0] != 19 || &h[1..20] != PROTOCOL {
        return Err(WireError::NotBitTorrent);
    }
    if &h[28..48] != info_hash {
        return Err(WireError::WrongTorrent);
    }
    let mut id = [0u8; 20];
    id.copy_from_slice(&h[48..68]);
    Ok(id)
}

/// A message from a peer. Piece data borrows from the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Message<'a> {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(&'a [u8]),
    Request {
        piece: u32,
        begin: u32,
        len: u32,
    },
    Piece {
        piece: u32,
        begin: u32,
        data: &'a [u8],
    },
    Cancel {
        piece: u32,
        begin: u32,
        len: u32,
    },
    /// An extension protocol message (BEP 10): `id` 0 is the extended handshake, others are the
    /// ids TorrentPlayer assigned in its own handshake. The payload is untrusted bencode.
    Extended {
        id: u8,
        payload: &'a [u8],
    },
    /// Any message TorrentPlayer does not use (DHT port, fast extension).
    Ignored(u8),
}

/// Protocol violations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum WireError {
    #[error("not a BitTorrent handshake")]
    NotBitTorrent,
    #[error("the peer is serving a different torrent")]
    WrongTorrent,
    #[error("frame of {0} bytes exceeds the limit")]
    FrameTooLarge(usize),
    #[error("malformed message {0}")]
    Malformed(u8),
}

/// Splits one complete frame off the front of `buf`.
///
/// Returns `Ok(None)` if more data is needed, otherwise the frame (without its length prefix)
/// and the number of bytes consumed. `max` bounds the frame length.
pub(crate) fn split_frame(buf: &[u8], max: usize) -> Result<Option<(&[u8], usize)>, WireError> {
    let Some(prefix) = buf.get(..4) else {
        return Ok(None);
    };
    let len = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    if len > max {
        return Err(WireError::FrameTooLarge(len));
    }
    Ok(buf.get(4..4 + len).map(|frame| (frame, 4 + len)))
}

/// Parses a frame (without its length prefix).
pub(crate) fn parse(frame: &[u8]) -> Result<Message<'_>, WireError> {
    let Some((&id, body)) = frame.split_first() else {
        return Ok(Message::KeepAlive);
    };
    let fixed = |want: usize| {
        if body.len() == want {
            Ok(())
        } else {
            Err(WireError::Malformed(id))
        }
    };
    Ok(match id {
        0 => {
            fixed(0)?;
            Message::Choke
        }
        1 => {
            fixed(0)?;
            Message::Unchoke
        }
        2 => {
            fixed(0)?;
            Message::Interested
        }
        3 => {
            fixed(0)?;
            Message::NotInterested
        }
        4 => {
            fixed(4)?;
            Message::Have(be32(body, 0))
        }
        5 => Message::Bitfield(body),
        6 | 8 => {
            fixed(12)?;
            let (piece, begin, len) = (be32(body, 0), be32(body, 4), be32(body, 8));
            if id == 6 {
                Message::Request { piece, begin, len }
            } else {
                Message::Cancel { piece, begin, len }
            }
        }
        7 => {
            if body.len() < 8 {
                return Err(WireError::Malformed(id));
            }
            Message::Piece {
                piece: be32(body, 0),
                begin: be32(body, 4),
                data: &body[8..],
            }
        }
        20 => {
            let Some((&ext, payload)) = body.split_first() else {
                return Err(WireError::Malformed(id));
            };
            Message::Extended { id: ext, payload }
        }
        other => Message::Ignored(other),
    })
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Messages TorrentPlayer sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outgoing {
    KeepAlive,
    Choke,
    Unchoke,
    /// TorrentPlayer's extended handshake (BEP 10), sent only to peers that support extensions.
    ExtendedHandshake,
    Interested,
    Bitfield(Bytes),
    Have(u32),
    Request {
        piece: u32,
        begin: u32,
        len: u32,
    },
    Cancel {
        piece: u32,
        begin: u32,
        len: u32,
    },
    Piece {
        piece: u32,
        begin: u32,
        data: Bytes,
    },
}

impl Outgoing {
    /// Appends the encoded message to `out`.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        let mut put = |id: Option<u8>, ints: &[u32]| {
            let len = u32::from(id.is_some()) + 4 * u32::try_from(ints.len()).unwrap_or(0);
            out.extend_from_slice(&len.to_be_bytes());
            if let Some(id) = id {
                out.push(id);
            }
            for i in ints {
                out.extend_from_slice(&i.to_be_bytes());
            }
        };
        match self {
            Self::ExtendedHandshake => {
                let len = u32::try_from(EXTENDED_HANDSHAKE.len() + 2).unwrap_or(0);
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(&[20, 0]);
                out.extend_from_slice(EXTENDED_HANDSHAKE);
            }
            Self::KeepAlive => put(None, &[]),
            Self::Choke => put(Some(0), &[]),
            Self::Unchoke => put(Some(1), &[]),
            Self::Interested => put(Some(2), &[]),
            Self::Bitfield(bits) => {
                let len = u32::try_from(bits.len() + 1).unwrap_or(0);
                out.extend_from_slice(&len.to_be_bytes());
                out.push(5);
                out.extend_from_slice(bits);
            }
            Self::Have(piece) => put(Some(4), &[*piece]),
            Self::Request { piece, begin, len } => put(Some(6), &[*piece, *begin, *len]),
            Self::Cancel { piece, begin, len } => put(Some(8), &[*piece, *begin, *len]),
            Self::Piece { piece, begin, data } => {
                let len = u32::try_from(data.len() + 9).unwrap_or(0);
                out.extend_from_slice(&len.to_be_bytes());
                out.push(7);
                out.extend_from_slice(&piece.to_be_bytes());
                out.extend_from_slice(&begin.to_be_bytes());
                out.extend_from_slice(data);
            }
        }
    }

    /// Payload bytes uploaded by this message.
    pub(crate) fn uploaded_len(&self) -> usize {
        match self {
            Self::Piece { data, .. } => data.len(),
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_round_trip() {
        let h = handshake(&[7; 20], &[9; 20]);
        assert_eq!(check_handshake(&h, &[7; 20]), Ok([9; 20]));
        assert!(supports_extensions(&h));
        assert!(!supports_extensions(&[0; HANDSHAKE_LEN]));
        assert_eq!(check_handshake(&h, &[8; 20]), Err(WireError::WrongTorrent));
        let mut bad = h;
        bad[5] = b'x';
        assert_eq!(
            check_handshake(&bad, &[7; 20]),
            Err(WireError::NotBitTorrent)
        );
    }

    #[test]
    fn frames_and_messages() {
        let mut out = Vec::new();
        Outgoing::Request {
            piece: 3,
            begin: 16_384,
            len: 16_384,
        }
        .encode(&mut out);
        Outgoing::KeepAlive.encode(&mut out);
        Outgoing::Have(9).encode(&mut out);

        let (frame, used) = split_frame(&out, MAX_FRAME).unwrap().unwrap();
        assert_eq!(
            parse(frame),
            Ok(Message::Request {
                piece: 3,
                begin: 16_384,
                len: 16_384
            })
        );
        let rest = &out[used..];
        let (frame, used2) = split_frame(rest, MAX_FRAME).unwrap().unwrap();
        assert_eq!(parse(frame), Ok(Message::KeepAlive));
        let (frame, _) = split_frame(&rest[used2..], MAX_FRAME).unwrap().unwrap();
        assert_eq!(parse(frame), Ok(Message::Have(9)));
    }

    #[test]
    fn upload_messages_round_trip() {
        let mut out = Vec::new();
        Outgoing::Choke.encode(&mut out);
        Outgoing::Unchoke.encode(&mut out);
        Outgoing::Bitfield(Bytes::from_static(&[0xa0])).encode(&mut out);
        Outgoing::Piece {
            piece: 4,
            begin: 16_384,
            data: Bytes::from_static(b"piece data"),
        }
        .encode(&mut out);

        let mut rest = out.as_slice();
        for expected in [Message::Choke, Message::Unchoke, Message::Bitfield(&[0xa0])] {
            let (frame, used) = split_frame(rest, MAX_FRAME).unwrap().unwrap();
            assert_eq!(parse(frame), Ok(expected));
            rest = &rest[used..];
        }
        let (frame, used) = split_frame(rest, MAX_FRAME).unwrap().unwrap();
        assert_eq!(used, rest.len());
        assert_eq!(
            parse(frame),
            Ok(Message::Piece {
                piece: 4,
                begin: 16_384,
                data: b"piece data",
            })
        );
    }

    #[test]
    fn extended_messages() {
        let mut out = Vec::new();
        Outgoing::ExtendedHandshake.encode(&mut out);
        let (frame, used) = split_frame(&out, MAX_FRAME).unwrap().unwrap();
        assert_eq!(used, out.len());
        let Ok(Message::Extended { id: 0, payload }) = parse(frame) else {
            panic!("not an extended handshake");
        };
        let dict = crate::bencode::decode(payload).unwrap();
        let m = dict.get(b"m").unwrap();
        assert_eq!(
            m.get(b"ut_pex").and_then(crate::bencode::Value::as_int),
            Some(i64::from(UT_PEX_ID))
        );
        assert_eq!(
            parse(&[20, 1, b'd', b'e']),
            Ok(Message::Extended {
                id: 1,
                payload: b"de"
            })
        );
        assert_eq!(parse(&[20]), Err(WireError::Malformed(20)));
    }

    #[test]
    fn piece_borrows_its_data() {
        let frame = [&[7u8][..], &1u32.to_be_bytes(), &0u32.to_be_bytes(), b"abc"].concat();
        assert_eq!(
            parse(&frame),
            Ok(Message::Piece {
                piece: 1,
                begin: 0,
                data: b"abc"
            })
        );
    }

    #[test]
    fn incomplete_oversized_and_malformed_input() {
        assert_eq!(split_frame(&[0, 0], MAX_FRAME), Ok(None));
        assert_eq!(split_frame(&[0, 0, 0, 5, 1], MAX_FRAME), Ok(None));
        assert_eq!(
            split_frame(&[0x7f, 0, 0, 0], MAX_FRAME),
            Err(WireError::FrameTooLarge(0x7f00_0000))
        );
        assert_eq!(parse(&[4, 0, 0]), Err(WireError::Malformed(4)));
        assert_eq!(parse(&[0, 1]), Err(WireError::Malformed(0)));
        assert_eq!(parse(&[7, 0, 0, 0]), Err(WireError::Malformed(7)));
        assert_eq!(parse(&[6; 12]), Err(WireError::Malformed(6)));
        assert_eq!(parse(&[21, 1, 2, 3]), Ok(Message::Ignored(21)));
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // A cheap deterministic stand-in for fuzzing (the cargo-fuzz target covers more).
        let mut x: u32 = 1;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let len = (x % 40) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (x >> (i % 24)).to_le_bytes()[0]).collect();
            let _ = split_frame(&bytes, 64);
            let _ = parse(&bytes);
        }
    }
}
