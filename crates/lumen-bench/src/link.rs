// SPDX-License-Identifier: MIT OR Apache-2.0

//! A loopback link emulator: bandwidth and latency between the engine under test and the seeder.
//!
//! The seeder runs unthrottled. Every connection to it goes through this relay, which
//!
//! - shapes seeder-to-leecher traffic to a fixed rate shared by all connections (the user's
//!   downlink), with a small burst (20 ms of the rate, at least 64 KiB), and
//! - delays every chunk in both directions by half the round-trip time, and
//! - buffers at most 256 KiB per connection (a router queue), so senders feel backpressure.
//!
//! Throttling the seeder directly is unrealistic: its rate limiter allows a one-second burst
//! that refills whenever the link goes idle, and loopback has almost no latency, so request
//! queue sizing (which matters most on real networks) would be invisible.
//!
//! A [`PeerProfile`] makes one seeder behave like a weaker peer of a thin swarm: a slow uplink of
//! its own, a longer round trip, only some of the pieces (its bitfield is rewritten), periodic
//! choking (choke and unchoke messages are injected and piece data is dropped while choked), or
//! times when it is offline. Times are measured from the last [`Clock::reset`].

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// Smallest burst the shaper allows, in bytes.
const MIN_BURST: f64 = 64.0 * 1024.0;
/// The burst is also at least this long at the link rate. Windows timers fire with coarse
/// (often about 15 ms) resolution, so a shaper that must sleep for every few KiB cannot
/// reach high rates; 20 ms of slack is still a small burst.
const BURST_TIME: f64 = 0.020;
/// Bytes the relay holds per connection before it stops reading from the sender, like a
/// router's queue. Beyond this, TCP flow control pushes back on the sender.
const QUEUE_LIMIT: usize = 256 * 1024;
/// A partial seed's pieces come in runs of this many (see [`PeerProfile::have`]).
pub const STRIPE_PIECES: u32 = 8;
/// Length of the BitTorrent handshake, which the relay passes through before parsing frames.
const HANDSHAKE_LEN: usize = 68;

/// The time base of every profile: offline windows and choke cycles count from the last reset,
/// so each benchmark run sees the same schedule.
#[derive(Debug, Clone)]
pub struct Clock(Arc<Mutex<Instant>>);

impl Default for Clock {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }
}

impl Clock {
    /// Restarts the schedule now.
    pub fn reset(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    /// Time since the last reset.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }

    fn at(&self, offset: Duration) -> Instant {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) + offset
    }
}

/// Unchoked for `on`, then choked for `off`, repeating; `offset` shifts the cycle so several
/// peers need not choke at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChokeCycle {
    /// Time unchoked per cycle.
    pub on: Duration,
    /// Time choked per cycle.
    pub off: Duration,
    /// How far into the cycle the clock starts.
    pub offset: Duration,
}

impl ChokeCycle {
    /// Whether the peer is choking at `t`, and when that next changes.
    fn state(&self, t: Duration) -> (bool, Duration) {
        let period = self.on + self.off;
        if period.is_zero() || self.off.is_zero() {
            return (false, Duration::MAX);
        }
        let shifted = t + self.offset;
        let into = Duration::from_nanos(
            u64::try_from(shifted.as_nanos() % period.as_nanos()).unwrap_or(0),
        );
        if into < self.on {
            (false, t + self.on.saturating_sub(into))
        } else {
            (true, t + period.saturating_sub(into))
        }
    }
}

/// How one seeder behaves, beyond the shared downlink.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerProfile {
    /// The seeder's own upload rate in bytes per second, shared by its connections. `None`: only
    /// the downlink limits it.
    pub up_bps: Option<u64>,
    /// Round-trip time to this seeder; `None` uses the link's.
    pub rtt: Option<Duration>,
    /// `(k, n)`: advertise only pieces `p` with `(p / STRIPE_PIECES) % n == k`, like a partial
    /// seed. Such a peer then holds `1/n` of the torrent.
    pub have: Option<(u32, u32)>,
    /// Periodic choking.
    pub choke: Option<ChokeCycle>,
    /// `(from, until)` windows (since the clock's reset) in which the seeder is offline: open
    /// connections are closed at `from` and new ones are refused until `until`.
    pub offline: Vec<(Duration, Duration)>,
    /// With a local tracker: announce this seeder only after this long (a peer that joins the
    /// swarm late).
    pub announce_after: Duration,
}

impl PeerProfile {
    /// Whether the profile needs the relay to parse the peer protocol.
    fn rewrites(&self) -> bool {
        self.have.is_some() || self.choke.is_some()
    }

    /// Whether the seeder is offline at `t`.
    fn offline_at(&self, t: Duration) -> bool {
        self.offline
            .iter()
            .any(|&(from, until)| from <= t && t < until)
    }

    /// The next time at or after `t` when the seeder goes offline.
    fn next_offline(&self, t: Duration) -> Option<Duration> {
        self.offline
            .iter()
            .map(|&(from, _)| from)
            .filter(|&from| from >= t)
            .min()
    }

    fn advertises(&self, piece: u32) -> bool {
        self.have
            .is_none_or(|(k, n)| n == 0 || (piece / STRIPE_PIECES) % n == k)
    }
}

/// A running link emulator. Stops when dropped.
#[derive(Debug)]
pub struct Link {
    /// Address leechers should connect to instead of the seeder.
    pub addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Link {
    /// Starts relaying to `upstream` with `down_bps` of downlink and `rtt` round-trip time.
    ///
    /// # Errors
    ///
    /// Returns an error if the loopback listener cannot be bound.
    pub async fn start(
        upstream: SocketAddr,
        down_bps: u64,
        rtt: Duration,
    ) -> std::io::Result<Self> {
        Self::start_shared(
            upstream,
            Arc::new(Bucket::new(down_bps)),
            rtt,
            Arc::new(PeerProfile::default()),
            Clock::default(),
        )
        .await
    }

    /// Starts one relay per upstream, all sharing one downlink of `down_bps`, as several peers
    /// share a viewer's connection.
    ///
    /// # Errors
    ///
    /// Returns an error if a loopback listener cannot be bound.
    pub async fn start_many(
        upstreams: &[SocketAddr],
        down_bps: u64,
        rtt: Duration,
    ) -> std::io::Result<Vec<Self>> {
        let peers: Vec<(SocketAddr, PeerProfile)> = upstreams
            .iter()
            .map(|&a| (a, PeerProfile::default()))
            .collect();
        Self::start_profiles(&peers, down_bps, rtt, &Clock::default()).await
    }

    /// Like [`Link::start_many`], with a profile per seeder.
    ///
    /// # Errors
    ///
    /// Returns an error if a loopback listener cannot be bound.
    pub async fn start_profiles(
        peers: &[(SocketAddr, PeerProfile)],
        down_bps: u64,
        rtt: Duration,
        clock: &Clock,
    ) -> std::io::Result<Vec<Self>> {
        let bucket = Arc::new(Bucket::new(down_bps));
        let mut links = Vec::with_capacity(peers.len());
        for (upstream, profile) in peers {
            links.push(
                Self::start_shared(
                    *upstream,
                    Arc::clone(&bucket),
                    profile.rtt.unwrap_or(rtt),
                    Arc::new(profile.clone()),
                    clock.clone(),
                )
                .await?,
            );
        }
        Ok(links)
    }

    async fn start_shared(
        upstream: SocketAddr,
        bucket: Arc<Bucket>,
        rtt: Duration,
        profile: Arc<PeerProfile>,
        clock: Clock,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let one_way = rtt / 2;
        // The seeder's own uplink, shared by all its connections.
        let uplink = profile.up_bps.map(|bps| Arc::new(Bucket::new(bps)));
        let task = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                if profile.offline_at(clock.elapsed()) {
                    // Refused, as by a peer that is not online.
                    drop(client);
                    continue;
                }
                let buckets: Vec<Arc<Bucket>> = uplink
                    .iter()
                    .cloned()
                    .chain([Arc::clone(&bucket)])
                    .collect();
                let profile = Arc::clone(&profile);
                let clock = clock.clone();
                tokio::spawn(async move {
                    let Ok(server) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    let _ = client.set_nodelay(true);
                    let _ = server.set_nodelay(true);
                    let (client_read, client_write) = client.into_split();
                    let (server_read, server_write) = server.into_split();
                    let goes_offline = async {
                        match profile.next_offline(clock.elapsed()) {
                            Some(at) => tokio::time::sleep_until(clock.at(at).into()).await,
                            None => std::future::pending().await,
                        }
                    };
                    let filter = profile
                        .rewrites()
                        .then(|| Filter::new(Arc::clone(&profile), clock.clone()));
                    tokio::select! {
                        () = pump(client_read, server_write, one_way, Vec::new(), None) => {}
                        () = pump(server_read, client_write, one_way, buckets, filter) => {}
                        () = goes_offline => {}
                    }
                });
            }
        });
        Ok(Self { addr, task })
    }
}

/// Copies `from` to `to`, delaying each chunk by `delay` and shaping it to every bucket in turn;
/// `filter` rewrites seeder-to-leecher protocol messages.
async fn pump(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    delay: Duration,
    buckets: Vec<Arc<Bucket>>,
    mut filter: Option<Filter>,
) {
    let queue = Arc::new(Relay::default());
    let reader_queue = Arc::clone(&queue);
    let reader = async move {
        let mut buf = vec![0u8; 64 * 1024];
        let mut out = Vec::new();
        loop {
            // Like a full router queue: stop reading until the writer drains it.
            while reader_queue.queued() >= QUEUE_LIMIT {
                reader_queue.drained.notified().await;
            }
            let toggle = filter.as_ref().map_or(Duration::MAX, Filter::next_toggle);
            let read = tokio::select! {
                read = from.read(&mut buf) => Some(read),
                () = sleep_until_offset(filter.as_ref(), toggle) => None,
            };
            out.clear();
            match read {
                None => {
                    if let Some(f) = filter.as_mut() {
                        f.update_choke(&mut out);
                    }
                }
                Some(Ok(0) | Err(_)) => break,
                Some(Ok(n)) => match filter.as_mut() {
                    Some(f) => {
                        f.update_choke(&mut out);
                        f.feed(&buf[..n], &mut out);
                    }
                    None => out.extend_from_slice(&buf[..n]),
                },
            }
            if !out.is_empty() {
                reader_queue.push(Instant::now() + delay, std::mem::take(&mut out));
            }
        }
    };
    let writer = async move {
        loop {
            let Some((due, chunk)) = queue.pop() else {
                queue.filled.notified().await;
                continue;
            };
            tokio::time::sleep_until(due.into()).await;
            for bucket in &buckets {
                bucket.take(chunk.len()).await;
            }
            if to.write_all(&chunk).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        () = reader => {}
        () = writer => {}
    }
}

async fn sleep_until_offset(filter: Option<&Filter>, offset: Duration) {
    match filter {
        Some(f) if offset != Duration::MAX => {
            tokio::time::sleep_until(f.clock.at(offset).into()).await;
        }
        _ => std::future::pending().await,
    }
}

/// Rewrites the seeder's side of the peer protocol for a [`PeerProfile`]: hides pieces a
/// partial seed does not have, and chokes on the profile's schedule.
struct Filter {
    profile: Arc<PeerProfile>,
    clock: Clock,
    /// Handshake bytes still to pass through unparsed.
    handshake_left: usize,
    /// Bytes of an incomplete frame.
    partial: Vec<u8>,
    /// Whether the seeder itself has unchoked the leecher.
    seeder_unchoked: bool,
    /// Whether the profile is choking the leecher now.
    forced_choke: bool,
}

impl Filter {
    fn new(profile: Arc<PeerProfile>, clock: Clock) -> Self {
        Self {
            profile,
            clock,
            handshake_left: HANDSHAKE_LEN,
            partial: Vec::new(),
            seeder_unchoked: false,
            forced_choke: false,
        }
    }

    /// When the choke state next changes (as an offset on the clock).
    fn next_toggle(&self) -> Duration {
        self.profile
            .choke
            .map_or(Duration::MAX, |c| c.state(self.clock.elapsed()).1)
    }

    /// Applies the choke schedule, injecting a choke or unchoke into `out` when it changes.
    fn update_choke(&mut self, out: &mut Vec<u8>) {
        let Some(cycle) = self.profile.choke else {
            return;
        };
        let (choked, _) = cycle.state(self.clock.elapsed());
        if choked == self.forced_choke || self.handshake_left > 0 {
            return;
        }
        self.forced_choke = choked;
        // The leecher only sees a change if the seeder had unchoked it.
        if self.seeder_unchoked {
            out.extend_from_slice(&[0, 0, 0, 1, u8::from(!choked)]);
        }
    }

    /// Passes seeder bytes through, rewriting whole frames.
    fn feed(&mut self, mut data: &[u8], out: &mut Vec<u8>) {
        if self.handshake_left > 0 {
            let n = self.handshake_left.min(data.len());
            out.extend_from_slice(&data[..n]);
            self.handshake_left -= n;
            data = &data[n..];
        }
        self.partial.extend_from_slice(data);
        let mut at = 0;
        while let Some(prefix) = self.partial.get(at..at + 4) {
            let len = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
            let Some(frame) = self.partial.get(at + 4..at + 4 + len) else {
                break;
            };
            let frame = frame.to_vec();
            self.frame(&frame, out);
            at += 4 + len;
        }
        self.partial.drain(..at);
    }

    fn frame(&mut self, frame: &[u8], out: &mut Vec<u8>) {
        let forward = |out: &mut Vec<u8>, frame: &[u8]| {
            out.extend_from_slice(&u32::try_from(frame.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(frame);
        };
        match frame.first() {
            // Choke and unchoke: remember the seeder's choice; hide it while choking ourselves.
            Some(&id @ (0 | 1)) => {
                self.seeder_unchoked = id == 1;
                if !self.forced_choke {
                    forward(out, frame);
                }
            }
            // Have: only for pieces the profile holds.
            Some(4) if frame.len() == 5 => {
                let piece = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
                if self.profile.advertises(piece) {
                    forward(out, frame);
                }
            }
            // Bitfield: clear the pieces the profile does not hold.
            Some(5) => {
                let mut bits = frame.to_vec();
                for (byte_index, byte) in bits[1..].iter_mut().enumerate() {
                    for bit in 0..8u32 {
                        let piece = u32::try_from(byte_index).unwrap_or(u32::MAX) * 8 + bit;
                        if !self.profile.advertises(piece) {
                            *byte &= !(0x80u8 >> bit);
                        }
                    }
                }
                forward(out, &bits);
            }
            // Piece data is discarded while choked, as a choking peer discards requests.
            Some(7) if self.forced_choke => {}
            _ => forward(out, frame),
        }
    }
}

/// Chunks waiting to be forwarded, each with the time it becomes due, plus their total size.
type Queue = (VecDeque<(Instant, Vec<u8>)>, usize);

/// One direction's queue of delayed chunks.
#[derive(Debug, Default)]
struct Relay {
    chunks: Mutex<Queue>,
    filled: Notify,
    drained: Notify,
}

impl Relay {
    fn queued(&self) -> usize {
        self.chunks.lock().unwrap_or_else(PoisonError::into_inner).1
    }

    fn push(&self, due: Instant, chunk: Vec<u8>) {
        let mut q = self.chunks.lock().unwrap_or_else(PoisonError::into_inner);
        q.1 += chunk.len();
        q.0.push_back((due, chunk));
        drop(q);
        self.filled.notify_one();
    }

    fn pop(&self) -> Option<(Instant, Vec<u8>)> {
        let mut q = self.chunks.lock().unwrap_or_else(PoisonError::into_inner);
        let next = q.0.pop_front();
        if let Some((_, chunk)) = &next {
            q.1 -= chunk.len();
            drop(q);
            self.drained.notify_one();
        }
        next
    }
}

/// A link's bandwidth, shared by all connections through it.
///
/// Implemented as GCRA (virtual scheduling): the link is busy until a theoretical time, each
/// send reserves the next slot, and a sender waits only for its own slot. (A token bucket in
/// which every sender sleeps off the shared deficit over-throttles when several connections
/// send at once.)
#[derive(Debug)]
struct Bucket {
    bytes_per_sec: f64,
    burst: Duration,
    /// When the link becomes idle if every reserved send goes out back to back.
    busy_until: Mutex<Instant>,
}

impl Bucket {
    #[allow(clippy::cast_precision_loss)]
    fn new(bytes_per_sec: u64) -> Self {
        let bytes_per_sec = bytes_per_sec.max(1) as f64;
        let burst = Duration::from_secs_f64((MIN_BURST / bytes_per_sec).max(BURST_TIME));
        Self {
            bytes_per_sec,
            burst,
            busy_until: Mutex::new(Instant::now()),
        }
    }

    async fn take(&self, bytes: usize) {
        let wait = {
            let mut busy = self
                .busy_until
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            // An idle link only banks up to one burst of credit.
            let start = (*busy).max(now.checked_sub(self.burst).unwrap_or(now));
            #[allow(clippy::cast_precision_loss)]
            let cost = Duration::from_secs_f64(bytes as f64 / self.bytes_per_sec);
            *busy = start + cost;
            // Allowed to send once the slot is within one burst of now.
            (*busy)
                .checked_sub(self.burst)
                .and_then(|ready| ready.checked_duration_since(now))
        };
        if let Some(wait) = wait.filter(|w| *w > Duration::from_millis(1)) {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choke_cycle_alternates() {
        let c = ChokeCycle {
            on: Duration::from_secs(10),
            off: Duration::from_secs(5),
            offset: Duration::ZERO,
        };
        assert_eq!(c.state(Duration::ZERO), (false, Duration::from_secs(10)));
        assert_eq!(
            c.state(Duration::from_secs(12)),
            (true, Duration::from_secs(15))
        );
        assert_eq!(
            c.state(Duration::from_secs(16)),
            (false, Duration::from_secs(25))
        );
        let shifted = ChokeCycle {
            offset: Duration::from_secs(10),
            ..c
        };
        assert!(shifted.state(Duration::ZERO).0);
    }

    #[test]
    fn filter_hides_pieces_and_chokes() {
        let profile = Arc::new(PeerProfile {
            have: Some((1, 2)),
            choke: Some(ChokeCycle {
                on: Duration::from_secs(3600),
                off: Duration::from_secs(1),
                offset: Duration::ZERO,
            }),
            ..PeerProfile::default()
        });
        let mut f = Filter::new(profile, Clock::default());
        let mut out = Vec::new();
        // Handshake, then a bitfield for 32 pieces, an unchoke, and two haves.
        let mut input = vec![19u8; HANDSHAKE_LEN];
        input.extend_from_slice(&[0, 0, 0, 5, 5, 0xff, 0xff, 0xff, 0xff]);
        input.extend_from_slice(&[0, 0, 0, 1, 1]);
        input.extend_from_slice(&[0, 0, 0, 5, 4, 0, 0, 0, 3]);
        input.extend_from_slice(&[0, 0, 0, 5, 4, 0, 0, 0, 9]);
        // Split mid-frame to exercise reassembly.
        f.feed(&input[..70], &mut out);
        f.feed(&input[70..], &mut out);
        assert_eq!(&out[..HANDSHAKE_LEN], &input[..HANDSHAKE_LEN]);
        let rest = &out[HANDSHAKE_LEN..];
        // Pieces 8..16 and 24..32 remain: stripes 1 and 3 of 8 pieces.
        assert_eq!(&rest[..9], &[0, 0, 0, 5, 5, 0x00, 0xff, 0x00, 0xff]);
        assert_eq!(&rest[9..14], &[0, 0, 0, 1, 1]);
        // Only `have 9` survives.
        assert_eq!(&rest[14..], &[0, 0, 0, 5, 4, 0, 0, 0, 9]);
        assert!(f.seeder_unchoked);

        // While forcibly choked, piece data is dropped.
        out.clear();
        f.forced_choke = true;
        f.feed(&[0, 0, 0, 10, 7, 0, 0, 0, 1, 0, 0, 0, 0, 42], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn offline_windows() {
        let p = PeerProfile {
            offline: vec![(Duration::from_secs(5), Duration::from_secs(8))],
            ..PeerProfile::default()
        };
        assert!(!p.offline_at(Duration::from_secs(4)));
        assert!(p.offline_at(Duration::from_secs(5)));
        assert!(!p.offline_at(Duration::from_secs(8)));
        assert_eq!(
            p.next_offline(Duration::from_secs(1)),
            Some(Duration::from_secs(5))
        );
        assert_eq!(p.next_offline(Duration::from_secs(6)), None);
    }
}
