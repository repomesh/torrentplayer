// SPDX-License-Identifier: MIT OR Apache-2.0

//! One task per peer connection: handshake, then read messages and write requests.

use std::net::SocketAddr;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use bytes::{Buf as _, BytesMut};
use lumen_net::{Lease, NetError};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tracing::{debug, trace};

use crate::sched::PeerKey;
use crate::torrent::{PeerEnd, Shared, Verdict};
use crate::wire::{self, HANDSHAKE_LEN, MAX_FRAME, Message, Outgoing, UT_PEX_ID, WireError};

/// Messages queued for one peer before it is considered stuck.
pub(crate) const SEND_QUEUE: usize = 4096;
/// Keep-alive interval (peers drop connections idle for about two minutes).
const KEEPALIVE: Duration = Duration::from_secs(60);
/// Peer-exchange messages closer together than this are ignored (BEP 11 asks for one a
/// minute).
const PEX_INTERVAL: Duration = Duration::from_secs(30);

/// Per-connection state the message handler keeps.
struct Conn {
    addr: SocketAddr,
    /// Piece data received, in bytes.
    delivered: u64,
    last_pex: Option<Instant>,
}

#[derive(Debug, thiserror::Error)]
enum PeerError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("network: {0}")]
    Network(#[from] NetError),
    #[error("protocol: {0}")]
    Wire(#[from] WireError),
    #[error("timed out")]
    Timeout,
    #[error("connected to ourselves")]
    SelfConnection,
    #[error("invalid {0}")]
    Invalid(&'static str),
}

/// Connects to `addr` (giving up after `timeout`) and serves the connection until it ends or
/// the torrent stops.
pub(crate) async fn run(shared: Arc<Shared>, addr: SocketAddr, key: PeerKey, timeout: Duration) {
    let opened = open(&shared, addr, timeout).await;
    shared.tcp_finished(opened.is_ok());
    let connected = match opened {
        Ok((tcp, lease)) => {
            let shaken = handshake(&shared, tcp).await;
            shared.handshake_finished();
            shaken.map(|(tcp, peer_id, extensions)| (tcp, peer_id, lease, extensions))
        }
        Err(error) => Err(error),
    };
    let (tcp, peer_id, mut lease, extensions) = match connected {
        Ok(ok) => ok,
        Err(error) => {
            trace!(%addr, %error, "could not connect");
            let end = match error {
                PeerError::SelfConnection
                | PeerError::Wire(WireError::NotBitTorrent | WireError::WrongTorrent) => {
                    PeerEnd::Banned
                }
                _ => PeerEnd::Unreachable,
            };
            shared.peer_finished(addr, end);
            return;
        }
    };
    let started = Instant::now();
    debug!(%addr, peer = key, client = %String::from_utf8_lossy(&peer_id[..8]), "peer connected");

    let (tx, rx) = mpsc::channel(SEND_QUEUE);
    shared.sched().add_peer(key);
    shared
        .peers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, tx);
    if extensions {
        shared.send(key, [Outgoing::ExtendedHandshake, Outgoing::Interested]);
    } else {
        shared.send(key, [Outgoing::Interested]);
    }

    let mut conn = Conn {
        addr,
        delivered: 0,
        last_pex: None,
    };
    let (read, write) = tcp.into_split();
    let result = tokio::select! {
        () = shared.cancel.cancelled() => Ok(()),
        // The network mode changed or the kill switch engaged: this connection must close.
        () = async {
            match lease.as_mut() {
                Some(lease) => lease.revoked().await,
                None => std::future::pending().await,
            }
        } => Ok(()),
        r = read_loop(&shared, key, read, &mut conn) => r,
        r = write_loop(write, rx) => r,
    };
    if let Err(error) = result {
        debug!(%addr, peer = key, %error, "peer disconnected");
    }
    shared
        .peers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&key);
    shared.sched().remove_peer(key);
    let end = match shared.take_verdict(key) {
        Some(Verdict::Banned) => PeerEnd::Banned,
        Some(Verdict::Unresponsive) => PeerEnd::Closed { worked: false },
        None => PeerEnd::closed(started.elapsed(), conn.delivered),
    };
    shared.peer_finished(addr, end);
    // Its released requests should go to other peers promptly.
    shared.request_replan(false);
}

/// Opens the TCP connection, giving up after `timeout`. Returns it and the network lease.
async fn open(
    shared: &Shared,
    addr: SocketAddr,
    timeout: Duration,
) -> Result<(TcpStream, Option<Lease>), PeerError> {
    let (tcp, lease) = if let Some(network) = &shared.cfg.network {
        // A proxy makes its own connection to the peer after ours, so allow it twice as long.
        let limit = if network.policy().protected {
            timeout * 2
        } else {
            timeout
        };
        let (tcp, lease) = tokio::time::timeout(limit, network.connect(addr))
            .await
            .map_err(|_| PeerError::Timeout)??;
        (tcp, Some(lease))
    } else {
        let tcp = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| PeerError::Timeout)??;
        (tcp, None)
    };
    tcp.set_nodelay(true)?;
    Ok((tcp, lease))
}

/// Exchanges handshakes. Returns the connection, the peer's id, and whether the peer supports
/// the extension protocol.
async fn handshake(
    shared: &Shared,
    mut tcp: TcpStream,
) -> Result<(TcpStream, [u8; 20], bool), PeerError> {
    let cfg = &shared.cfg;
    let ours = wire::handshake(&shared.meta.info_hash, &shared.peer_id);
    let theirs = tokio::time::timeout(cfg.handshake_timeout, async {
        tcp.write_all(&ours).await?;
        let mut h = [0u8; HANDSHAKE_LEN];
        tcp.read_exact(&mut h).await?;
        Ok::<_, std::io::Error>(h)
    })
    .await
    .map_err(|_| PeerError::Timeout)??;
    let peer_id = wire::check_handshake(&theirs, &shared.meta.info_hash)?;
    if peer_id == shared.peer_id {
        return Err(PeerError::SelfConnection);
    }
    Ok((tcp, peer_id, wire::supports_extensions(&theirs)))
}

async fn write_loop(
    mut write: OwnedWriteHalf,
    mut rx: mpsc::Receiver<Outgoing>,
) -> Result<(), PeerError> {
    let mut out = Vec::with_capacity(4096);
    loop {
        let first = match tokio::time::timeout(KEEPALIVE, rx.recv()).await {
            Ok(Some(m)) => m,
            // Every sender is gone: the peer was dropped by the torrent.
            Ok(None) => return Ok(()),
            Err(_) => Outgoing::KeepAlive,
        };
        out.clear();
        first.encode(&mut out);
        // Batch whatever else is queued into one write.
        while out.len() < 64 * 1024 {
            match rx.try_recv() {
                Ok(m) => m.encode(&mut out),
                Err(_) => break,
            }
        }
        write.write_all(&out).await?;
    }
}

async fn read_loop(
    shared: &Arc<Shared>,
    key: PeerKey,
    mut read: OwnedReadHalf,
    conn: &mut Conn,
) -> Result<(), PeerError> {
    // A bitfield can exceed the normal frame limit for torrents with many pieces.
    let max_frame = MAX_FRAME.max(shared.meta.piece_count() as usize / 8 + 16);
    let mut buf = BytesMut::with_capacity(64 * 1024);
    // A bitfield is allowed once, before any `have`. It need not be the very first message:
    // clients such as librqbit send an extended handshake first.
    let mut bitfield_allowed = true;
    loop {
        while let Some((frame, used)) = wire::split_frame(&buf, max_frame)? {
            let message = wire::parse(frame)?;
            let availability = matches!(message, Message::Bitfield(_) | Message::Have(_));
            handle(shared, key, message, bitfield_allowed, conn)?;
            if availability {
                bitfield_allowed = false;
            }
            buf.advance(used);
        }
        let n = tokio::time::timeout(shared.cfg.idle_timeout, read.read_buf(&mut buf))
            .await
            .map_err(|_| PeerError::Timeout)??;
        if n == 0 {
            return Ok(());
        }
        shared.throttle(n).await;
    }
}

fn handle(
    shared: &Arc<Shared>,
    key: PeerKey,
    message: Message<'_>,
    bitfield_allowed: bool,
    conn: &mut Conn,
) -> Result<(), PeerError> {
    let now = Instant::now();
    let mut refill = false;
    match message {
        Message::Bitfield(bits) => {
            if !bitfield_allowed || !shared.sched().on_bitfield(key, bits) {
                return Err(PeerError::Invalid("bitfield"));
            }
            refill = true;
        }
        Message::Have(piece) => {
            if !shared.sched().on_have(key, piece) {
                return Err(PeerError::Invalid("have"));
            }
            refill = true;
        }
        Message::Unchoke => {
            shared.sched().on_unchoke(key);
            refill = true;
        }
        Message::Choke => {
            shared.sched().on_choke(key);
            shared.request_replan(false);
        }
        Message::Piece { piece, begin, data } => {
            conn.delivered += data.len() as u64;
            receive_block(shared, key, piece, begin, data);
            // `receive_block` refilled the queue.
        }
        Message::Extended { id, payload } if id == UT_PEX_ID => {
            if conn
                .last_pex
                .is_none_or(|t| now.saturating_duration_since(t) >= PEX_INTERVAL)
            {
                conn.last_pex = Some(now);
                match crate::pex::parse(payload, conn.addr) {
                    Ok(peers) => shared.add_exchanged(&peers),
                    Err(error) => trace!(peer = key, %error, "ignoring a malformed PEX message"),
                }
            }
        }
        // TorrentStream does not upload yet (peers stay choked), so requests are ignored. The extended
        // handshake needs no answer: TorrentStream only receives peer exchange.
        Message::Request { .. }
        | Message::Cancel { .. }
        | Message::Interested
        | Message::NotInterested
        | Message::KeepAlive
        | Message::Extended { .. }
        | Message::Ignored(_) => {}
    }
    if refill {
        let requests = shared.sched().fill(key, now);
        if !requests.is_empty() {
            shared.send_requests(key, &requests);
        }
    }
    Ok(())
}

/// Hands a received block to the scheduler, cancels its duplicates, verifies a completed piece,
/// and tops up the sender's queue. Shared by peers and web seeds.
pub(crate) fn receive_block(
    shared: &Arc<Shared>,
    key: PeerKey,
    piece: u32,
    begin: u32,
    data: &[u8],
) {
    let outcome = shared.sched().on_block(key, piece, begin, data);
    shared.send_cancels(&outcome.cancels);
    if let Some((piece, data)) = outcome.completed {
        shared.complete_piece(piece, data);
    }
    let requests = shared.sched().fill(key, Instant::now());
    if !requests.is_empty() {
        shared.send_requests(key, &requests);
    }
}
