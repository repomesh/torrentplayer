// SPDX-License-Identifier: MIT OR Apache-2.0

//! One task per peer connection: handshake, then read messages and write requests.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::{Buf as _, Bytes, BytesMut};
use lumen_net::{Lease, NetError};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::metainfo::BLOCK_LEN;
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
/// Most disk reads a peer may queue for upload. Extra requests are ignored until a slot frees.
const MAX_UPLOAD_REQUESTS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UploadKey {
    piece: u32,
    begin: u32,
    len: u32,
}

/// Cancellable upload reads for one connection.
struct Uploads {
    slots: Arc<Semaphore>,
    pending: Mutex<HashMap<UploadKey, Arc<CancellationToken>>>,
}

impl Uploads {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(MAX_UPLOAD_REQUESTS)),
            pending: Mutex::new(HashMap::new()),
        })
    }

    fn request(self: &Arc<Self>, shared: Arc<Shared>, peer: PeerKey, key: UploadKey) {
        let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() else {
            return;
        };
        let Ok(global_permit) = Arc::clone(&shared.upload_slots).try_acquire_owned() else {
            return;
        };
        let cancel = Arc::new(CancellationToken::new());
        if let Some(old) = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, Arc::clone(&cancel))
        {
            old.cancel();
        }
        let uploads = Arc::clone(self);
        tokio::spawn(async move {
            let read_from = Arc::clone(&shared);
            let data = tokio::task::spawn_blocking(move || {
                let mut data = vec![0_u8; key.len as usize];
                read_from
                    .store
                    .read(key.piece, key.begin, &mut data)
                    .map(|()| Bytes::from(data))
            })
            .await;
            match data {
                Ok(Ok(data)) if shared.throttle_upload(data.len(), cancel.as_ref()).await => {
                    shared.send_upload(
                        peer,
                        Outgoing::Piece {
                            piece: key.piece,
                            begin: key.begin,
                            data,
                        },
                    );
                }
                Ok(Err(error)) => trace!(peer, %error, "could not read an upload block"),
                Err(error) => debug!(peer, %error, "upload read task failed"),
                _ => {}
            }
            let mut pending = uploads
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if pending
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &cancel))
            {
                pending.remove(&key);
            }
            drop(permit);
            drop(global_permit);
        });
    }

    fn cancel(&self, key: UploadKey) {
        if let Some(cancel) = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key)
        {
            cancel.cancel();
        }
    }

    fn cancel_all(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        for (_, cancel) in pending.drain() {
            cancel.cancel();
        }
    }
}

/// Per-connection state the message handler keeps.
struct Conn {
    addr: SocketAddr,
    /// Piece data received, in bytes.
    delivered: u64,
    last_pex: Option<Instant>,
    interested: bool,
    uploads: Arc<Uploads>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.uploads.cancel_all();
    }
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
    // Publish the sender only after its initial messages are queued. Holding the peer map while
    // taking the bitfield snapshot makes a concurrent piece completion either appear in the
    // bitfield or send a later `have`, never slip between them or precede the bitfield.
    let published = {
        let mut peers = shared.peers.lock().unwrap_or_else(PoisonError::into_inner);
        let mut hello = vec![Outgoing::Bitfield(shared.bitfield())];
        if extensions {
            hello.push(Outgoing::ExtendedHandshake);
        }
        hello.push(Outgoing::Interested);
        hello.push(if shared.upload_enabled.load(Ordering::Relaxed) {
            Outgoing::Unchoke
        } else {
            Outgoing::Choke
        });
        if hello
            .into_iter()
            .any(|message| tx.try_send(message).is_err())
        {
            false
        } else {
            peers.insert(key, tx);
            true
        }
    };
    if !published {
        shared.sched().remove_peer(key);
        shared.peer_finished(addr, PeerEnd::Closed { worked: false });
        return;
    }

    let mut conn = Conn {
        addr,
        delivered: 0,
        last_pex: None,
        interested: false,
        uploads: Uploads::new(),
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
        r = write_loop(&shared, write, rx) => r,
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
    shared: &Shared,
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
        let mut uploaded = first.uploaded_len();
        first.encode(&mut out);
        // Batch whatever else is queued into one write.
        while out.len() < 64 * 1024 {
            match rx.try_recv() {
                Ok(m) => {
                    uploaded += m.uploaded_len();
                    m.encode(&mut out);
                }
                Err(_) => break,
            }
        }
        write.write_all(&out).await?;
        shared
            .uploaded
            .fetch_add(uploaded as u64, Ordering::Relaxed);
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
        Message::Interested => conn.interested = true,
        Message::NotInterested => {
            conn.interested = false;
            conn.uploads.cancel_all();
        }
        Message::Request { piece, begin, len } => {
            let request = UploadKey { piece, begin, len };
            let valid = len > 0
                && len <= BLOCK_LEN
                && begin
                    .checked_add(len)
                    .is_some_and(|end| end <= shared.meta.piece_size(piece));
            if !valid {
                return Err(PeerError::Invalid("request"));
            }
            let have = shared.sched().have(piece);
            if conn.interested && have && shared.upload_enabled.load(Ordering::Relaxed) {
                conn.uploads.request(Arc::clone(shared), key, request);
            }
        }
        Message::Cancel { piece, begin, len } => {
            conn.uploads.cancel(UploadKey { piece, begin, len });
        }
        // The extended handshake needs no answer: TorrentStream only receives peer exchange.
        Message::KeepAlive | Message::Extended { .. } | Message::Ignored(_) => {}
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
