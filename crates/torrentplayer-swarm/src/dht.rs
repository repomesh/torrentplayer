// SPDX-License-Identifier: MIT OR Apache-2.0

//! Peer discovery through the BitTorrent DHT (BEP 5), using librqbit's DHT node.
//!
//! Many older course torrents list trackers that no longer answer; the DHT still knows their
//! peers. With a network manager its UDP sockets and bootstrap names use the selected network
//! lease. The node stops on every epoch change, when DHT is disabled, or when the kill switch
//! engages, and starts again once allowed. It never falls back to unbound sockets.
//! Only public addresses from the DHT are used: anyone can put an address there.
//!
//! Each torrent looks up its peers at once when added, then as often as it needs them (see
//! [`next_lookup`]): every 20 seconds or so while it downloads with few peers, every few
//! minutes once it has enough, and not at all while paused or with nothing to download.
//! TorrentPlayer announces itself in its lookups, and answers as a read-write node (BEP 43), only
//! while peers can reach it: the engine accepts incoming peers, and the router confirms a port
//! mapping for them or a peer connected in recently (see [`announce_port`] and
//! [`follow_reachability`]); never in SOCKS5 mode.
//!
//! In direct mode the routing tables (and with them the node id) are kept in a file between
//! runs, so the next start finds peers without bootstrapping from scratch. A protected mode
//! neither loads nor saves them: the same node id seen directly and through a VPN or proxy
//! would link the two.

use std::collections::HashSet;
use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use librqbit_dht::{Dht, DhtBuilder, DhtConfig, Id20};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use torrentplayer_net::Policy;
use tracing::{debug, info, warn};

use crate::torrent::Shared;

/// Wait before trying again when the node cannot start (for example, no UDP socket).
const RESTART_AFTER: Duration = Duration::from_secs(30);

/// How often a running node's routing tables are saved. Contacts change slowly, and the node
/// also saves when it stops.
const SAVE_EVERY: Duration = Duration::from_secs(10 * 60);

/// The engine's DHT node, started on first use.
pub(crate) struct DhtNode {
    running: Mutex<Option<(Dht, CancellationToken, u64)>>,
    network: Option<torrentplayer_net::Network>,
    /// The engine's token: the node stops with the engine.
    cancel: CancellationToken,
    /// Where direct-mode routing tables are kept between runs; `None` keeps nothing.
    state: Option<Arc<StateFile>>,
    /// Whether peers can reach the engine, as [`follow_reachability`] last saw: a node started
    /// while they cannot is read-only (BEP 43).
    reachable: AtomicBool,
}

// librqbit's DHT state is not `Debug`.
impl std::fmt::Debug for DhtNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DhtNode").finish_non_exhaustive()
    }
}

impl DhtNode {
    pub(crate) fn new(
        cancel: CancellationToken,
        network: Option<torrentplayer_net::Network>,
        state_file: Option<PathBuf>,
    ) -> Self {
        Self {
            running: Mutex::new(None),
            network,
            cancel,
            state: state_file.map(|path| {
                Arc::new(StateFile {
                    path,
                    written: std::sync::Mutex::new(None),
                })
            }),
            reachable: AtomicBool::new(false),
        }
    }

    /// The running node, starting it if needed. `None` if it cannot start, or if `policy` does
    /// not allow it: checked under the same lock as [`DhtNode::stop`], so a mode change that
    /// stops the node cannot be overtaken by a start that checked the policy just before it.
    async fn get(&self, policy: Option<&watch::Receiver<Policy>>) -> Option<Dht> {
        let mut running = self.running.lock().await;
        if !allowed(policy) || self.cancel.is_cancelled() {
            return None;
        }
        if self.network.is_none() && policy.is_some_and(|p| p.borrow().protected) {
            return None;
        }
        let epoch = policy.map_or(0, |p| p.borrow().epoch);
        if let Some((dht, token, started)) = running.as_ref()
            && !token.is_cancelled()
            && *started == epoch
        {
            return Some(Arc::clone(dht));
        }
        if let Some((_, token, _)) = running.take() {
            token.cancel();
        }
        let transport = if let Some(network) = &self.network {
            match crate::dht_transport::ScopedDht::open(network.clone()).await {
                Ok(transport) => Some(transport),
                Err(error) => {
                    debug!(%error, "could not open scoped DHT sockets");
                    return None;
                }
            }
        } else {
            None
        };
        if !allowed(policy) || policy.is_some_and(|p| p.borrow().epoch != epoch) {
            return None;
        }
        let token = self.cancel.child_token();
        let state = self
            .state
            .as_ref()
            .filter(|_| !policy.is_some_and(|p| p.borrow().protected));
        let saved = match state {
            Some(state) => state.load().await,
            None => None,
        };
        let (routing_table, routing_table_v6) = saved.map(|s| (s.table, s.table_v6)).unzip();
        let mut config = DhtConfig {
            transport,
            routing_table,
            routing_table_v6,
            cancellation_token: Some(token.clone()),
            // Read-only (BEP 43) while peers cannot reach the engine: see `follow_reachability`.
            read_only: !self.reachable.load(Ordering::Relaxed),
            ..DhtConfig::default()
        };
        // The node keeps the id its saved tables were built around.
        if let Some(table) = &config.routing_table {
            config.peer_id = Some(table.id());
        }
        match DhtBuilder::with_config(config).await {
            Ok(dht) => {
                debug!("DHT started");
                if let Some(state) = state {
                    tokio::spawn(keep_saved(Arc::clone(state), Arc::clone(&dht)));
                }
                *running = Some((Arc::clone(&dht), token, epoch));
                Some(dht)
            }
            Err(error) => {
                debug!(%error, "could not start the DHT");
                None
            }
        }
    }

    /// Shuts the node down (it starts again on next use).
    pub(crate) async fn stop(&self) {
        if let Some((_, token, _)) = self.running.lock().await.take() {
            token.cancel();
            debug!("DHT stopped");
        }
    }
}

/// The file holding a direct-mode node's routing tables between runs.
struct StateFile {
    path: PathBuf,
    /// The SHA-1 of what the file holds, when known, so an unchanged table is not written
    /// again. Locked for the whole save: a stopping node's last save can overlap its
    /// successor's first.
    written: std::sync::Mutex<Option<[u8; 20]>>,
}

/// The saved routing tables. librqbit's table type is not exported, so it is only ever
/// inferred from [`DhtConfig`] and `DhtState::with_routing_tables`; each table carries the
/// node id.
#[derive(Serialize, Deserialize)]
struct Saved<T> {
    table: T,
    table_v6: T,
}

impl StateFile {
    /// The saved tables, if any. A damaged file is ignored (the next save replaces it).
    async fn load<T: for<'de> Deserialize<'de>>(&self) -> Option<Saved<T>> {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => {
                warn!(%error, "could not read the saved DHT state");
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(saved) => {
                debug!("loaded the saved DHT routing tables");
                *self.lock_written() = Some(digest(&bytes));
                Some(saved)
            }
            Err(error) => {
                warn!(%error, "ignoring damaged DHT state");
                None
            }
        }
    }

    /// Saves `dht`'s tables, unless it knows no nodes yet: a node stopped right after starting
    /// must not replace a useful file with an empty one. Tables the file already holds are
    /// not written again.
    fn save(&self, dht: &Dht) -> io::Result<()> {
        let stats = dht.stats();
        if stats.routing_table_size + stats.routing_table_size_v6 == 0 {
            return Ok(());
        }
        let mut value = dht
            .with_routing_tables(|table, table_v6| serde_json::to_value(Saved { table, table_v6 }))
            .map_err(io::Error::other)?;
        strip_volatile(&mut value);
        let json = serde_json::to_vec(&value).map_err(io::Error::other)?;
        let digest = digest(&json);
        let mut written = self.lock_written();
        if *written == Some(digest) {
            return Ok(());
        }
        // Unknown until the write succeeds: a failed one may have changed nothing.
        *written = None;
        write_replacing(&self.path, &json)?;
        *written = Some(digest);
        Ok(())
    }

    fn lock_written(&self) -> std::sync::MutexGuard<'_, Option<[u8; 20]>> {
        self.written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Fields librqbit writes but never reads back: times since the last contact or refresh, a
/// node's status, and its error count. They change on every save, so they are dropped to
/// leave only the contacts (whose ids and addresses are what a restart uses).
const VOLATILE: [&str; 6] = [
    "last_refreshed",
    "last_request_ago",
    "last_response_ago",
    "last_query_ago",
    "status",
    "errors_in_a_row",
];

fn strip_volatile(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|key, _| !VOLATILE.contains(&key.as_str()));
            map.values_mut().for_each(strip_volatile);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_volatile),
        _ => {}
    }
}

fn digest(bytes: &[u8]) -> [u8; 20] {
    use sha1::Digest as _;
    sha1::Sha1::digest(bytes).into()
}

/// Writes `bytes` to `path` through a temporary file, so a crash never leaves half a file.
fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

/// Saves `dht`'s tables every [`SAVE_EVERY`], and once more when it stops.
async fn keep_saved(state: Arc<StateFile>, dht: Dht) {
    let stopped = dht.cancellation_token().clone();
    loop {
        let last = tokio::select! {
            () = stopped.cancelled() => true,
            () = tokio::time::sleep(SAVE_EVERY) => false,
        };
        let (state, dht) = (Arc::clone(&state), Arc::clone(&dht));
        match tokio::task::spawn_blocking(move || state.save(&dht)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!(%error, "could not save the DHT state"),
            Err(error) => warn!(%error, "the DHT state save did not finish"),
        }
        if last {
            return;
        }
    }
}

/// Whether the policy lets the DHT run. Without a network manager, it may.
pub(crate) fn allowed(policy: Option<&watch::Receiver<Policy>>) -> bool {
    policy.is_none_or(|rx| {
        let p = *rx.borrow();
        p.traffic_allowed && p.bound_udp_allowed && p.dht_allowed
    })
}

/// Waits for the next policy change; `false` if the network manager is gone. Never returns
/// without a network manager.
async fn changed(policy: &mut Option<watch::Receiver<Policy>>) -> bool {
    match policy.as_mut() {
        Some(rx) => rx.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Stops the node whenever the policy stops allowing it. Runs until the engine stops.
pub(crate) async fn follow_policy(node: Arc<DhtNode>, mut policy: watch::Receiver<Policy>) {
    let mut epoch = policy.borrow().epoch;
    loop {
        tokio::select! {
            () = node.cancel.cancelled() => return,
            changed = policy.changed() => {
                if changed.is_err() {
                    node.stop().await;
                    return;
                }
            }
        }
        let next = policy.borrow().epoch;
        if !allowed(Some(&policy)) || epoch != next {
            node.stop().await;
        }
        epoch = next;
    }
}

/// Makes the node read-write (BEP 43) as soon as peers can reach the engine (it accepts
/// incoming peers on a port with a confirmed mapping or a recent inbound peer, see
/// [`announce_port`]), and read-only again as soon as they cannot (the mapping lapsed, no peer
/// connected in for a while, or the listener closed), rather
/// than at a torrent's next lookup, which may be many minutes away. Runs until the engine
/// stops.
pub(crate) async fn follow_reachability(
    node: Arc<DhtNode>,
    network: torrentplayer_net::Network,
    accept_incoming: bool,
) {
    let mut status = network.status_changes();
    loop {
        let port = announce_port(
            network.incoming_port().filter(|_| accept_incoming),
            network.status().mode,
            network.reachable_port(),
        );
        let reachable = port.is_some();
        if node.reachable.swap(reachable, Ordering::Relaxed) != reachable {
            if let Some(port) = port {
                info!(port, "DHT read-write: peers can reach the listening port");
            } else {
                info!("DHT read-only: peers can no longer reach the listening port");
            }
            if let Some((dht, ..)) = node.running.lock().await.as_ref() {
                dht.set_read_only(!reachable);
            }
        }
        tokio::select! {
            () = node.cancel.cancelled() => return,
            changed = status.changed() => if changed.is_err() { return },
        }
    }
}

/// What a torrent needs from the DHT right now, for [`next_lookup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Demand {
    /// Paused: no lookups, and so no announces.
    pub(crate) paused: bool,
    /// Nothing to download now: every piece is verified, or nothing is playing or being read
    /// and every selected piece is verified.
    pub(crate) idle: bool,
    pub(crate) live_peers: usize,
    pub(crate) peer_limit: usize,
    /// The port incoming peers are accepted on, while they are (`network.incomingPeers`).
    pub(crate) listen_port: Option<u16>,
}

impl Demand {
    /// Downloading with fewer peers than it could use.
    fn wanting(self) -> bool {
        !self.paused && !self.idle && self.live_peers < enough_peers(self.peer_limit)
    }
}

/// Live peers past which more are not worth frequent lookups: half the peer limit.
fn enough_peers(peer_limit: usize) -> usize {
    (peer_limit / 2).max(1)
}

/// The shortest wait between lookups for a torrent that wants peers. It doubles after each
/// lookup that finds fewer than [`NEW_PEERS`] new addresses, up to [`SATISFIED_EVERY`].
const WANTING_EVERY: Duration = Duration::from_secs(20);
/// New addresses a lookup must find to keep [`WANTING_EVERY`] from growing.
const NEW_PEERS: usize = 10;
/// The wait between lookups once a torrent downloads with enough peers.
const SATISFIED_EVERY: Duration = Duration::from_secs(5 * 60);
/// The wait between lookups of a torrent with nothing to download that announces itself:
/// DHT nodes keep an announced peer for about 30 minutes.
const REANNOUNCE_EVERY: Duration = Duration::from_secs(15 * 60);
/// The wait after a lookup that found no address, while the torrent has something to download.
const EMPTY_RETRY: Duration = Duration::from_secs(2);
/// How often a waiting torrent's demand is checked, so a resumed or newly played torrent
/// looks up within this time rather than after the wait it had before.
const DEMAND_POLL: Duration = Duration::from_secs(5);
/// DHT addresses remembered per torrent, to count new ones; all forgotten past this.
const SEEN_LIMIT: usize = 8192;

/// How long after a lookup ended the next one is due, or `None` until the demand changes:
/// never while paused, nor while there is nothing to download unless the torrent announces
/// itself; every [`WANTING_EVERY`] while it wants peers, doubled for each of `misses` (the
/// lookups since one last found new peers); every [`SATISFIED_EVERY`] otherwise. A torrent with
/// something to download whose last `empty` lookups found no address at all (usually a node
/// still bootstrapping, whose first contacts did not answer) tries again after
/// [`EMPTY_RETRY`], doubled for each.
fn next_lookup(demand: Demand, announcing: bool, misses: u32, empty: u32) -> Option<Duration> {
    if demand.paused {
        return None;
    }
    if demand.idle {
        return announcing.then_some(REANNOUNCE_EVERY);
    }
    if empty > 0 {
        return Some(
            EMPTY_RETRY
                .saturating_mul(1 << (empty - 1).min(8))
                .min(SATISFIED_EVERY),
        );
    }
    if !demand.wanting() {
        return Some(SATISFIED_EVERY);
    }
    Some(
        WANTING_EVERY
            .saturating_mul(1 << misses.min(8))
            .min(SATISFIED_EVERY),
    )
}

/// The port to announce in DHT lookups (BEP 5 `announce_peer`, with the port given rather
/// than implied: the DHT's UDP port is not the peer port), and while there is one the node is
/// read-write. Only while the engine accepts incoming peers on `listen_port` and peers can
/// reach that port (`reachable`, from [`Network::reachable_port`]: the network listens on it,
/// and the router confirms a UPnP mapping to a public address or a peer from the internet
/// connected in recently, which also covers a port forwarded by hand or by a VPN provider once
/// trackers have sent a peer). Listening alone is not enough: behind a router that forwards
/// nothing, an announcement only sends peers to a closed port, and a read-write node only gets
/// queries it never receives. Never in SOCKS5 mode, which cannot accept connections.
///
/// [`Network::reachable_port`]: torrentplayer_net::Network::reachable_port
fn announce_port(
    listen_port: Option<u16>,
    mode: torrentplayer_net::ModeKind,
    reachable: Option<u16>,
) -> Option<u16> {
    let port = listen_port?;
    (mode != torrentplayer_net::ModeKind::Socks5 && reachable == Some(port)).then_some(port)
}

/// Finds peers for a torrent through the DHT while the policy allows it, until `cancel` fires
/// or the torrent is gone. The first lookup starts at once (unless the torrent is paused),
/// later ones as [`next_lookup`] says. Each lookup ends by itself, and every address it finds
/// goes to the torrent as soon as it arrives.
pub(crate) async fn discover(
    node: Arc<DhtNode>,
    shared: Weak<Shared>,
    info_hash: [u8; 20],
    mut policy: Option<watch::Receiver<Policy>>,
    cancel: CancellationToken,
) {
    let network = shared.upgrade().and_then(|s| s.cfg.network.clone());
    let mut pace = Pace::default();
    let mut seen = HashSet::new();
    loop {
        while !allowed(policy.as_ref()) {
            tokio::select! {
                () = cancel.cancelled() => return,
                ok = changed(&mut policy) => if !ok { return },
            }
        }
        let demand = match pace.wait(&shared, &mut policy, &cancel).await {
            Due::Now(demand) => demand,
            // Check the policy again.
            Due::PolicyChanged => continue,
            Due::Stop => return,
        };
        let Some(dht) = node.get(policy.as_ref()).await else {
            if !allowed(policy.as_ref()) {
                // The policy changed since the check above: wait for it again.
                continue;
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(RESTART_AFTER) => continue,
            }
        };
        let announce = network.as_ref().and_then(|network| {
            announce_port(
                demand.listen_port,
                network.status().mode,
                network.reachable_port(),
            )
        });
        // Not announcing means nobody can connect to us: say so (BEP 43), so other nodes keep
        // us out of their routing tables and do not query us.
        dht.set_read_only(announce.is_none());
        let started = Instant::now();
        let lookup = Lookup {
            dht: &dht,
            shared: &shared,
            seen: &mut seen,
            found: 0,
            new: 0,
        };
        let Some((finished, found, new)) =
            lookup.run(info_hash, announce, &mut policy, &cancel).await
        else {
            return;
        };
        debug!(
            found,
            new,
            announced = announce.is_some(),
            elapsed_ms = started.elapsed().as_millis(),
            finished,
            "DHT lookup"
        );
        pace.lookup_ended(finished, announce.is_some(), found, new);
    }
}

/// When one torrent's next lookup is due.
struct Pace {
    /// When the last lookup ended, and whether it announced.
    last: Option<(Instant, bool)>,
    /// Lookups since one found [`NEW_PEERS`] new addresses.
    misses: u32,
    /// Lookups in a row that found no address.
    empty: u32,
    was_wanting: bool,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            last: None,
            misses: 0,
            empty: 0,
            was_wanting: true,
        }
    }
}

enum Due {
    Now(Demand),
    PolicyChanged,
    /// The torrent is gone or discovery was cancelled.
    Stop,
}

impl Pace {
    /// Waits until a lookup is due, checking the torrent's demand every [`DEMAND_POLL`].
    async fn wait(
        &mut self,
        shared: &Weak<Shared>,
        policy: &mut Option<watch::Receiver<Policy>>,
        cancel: &CancellationToken,
    ) -> Due {
        loop {
            let Some(demand) = shared.upgrade().map(|s| s.dht_demand()) else {
                return Due::Stop;
            };
            let wanting = demand.wanting();
            if wanting && !self.was_wanting {
                self.misses = 0;
            }
            self.was_wanting = wanting;
            let due = match self.last {
                None => !demand.paused,
                Some((ended, announced)) => next_lookup(demand, announced, self.misses, self.empty)
                    .is_some_and(|wait| ended.elapsed() >= wait),
            };
            if due {
                return Due::Now(demand);
            }
            tokio::select! {
                () = cancel.cancelled() => return Due::Stop,
                () = tokio::time::sleep(DEMAND_POLL) => {},
                ok = changed(policy) => {
                    return if ok { Due::PolicyChanged } else { Due::Stop };
                }
            }
        }
    }

    /// Records a lookup that ended by itself (`finished`), or was stopped by a mode change or
    /// a stopped node, after which the next node looks up at once.
    fn lookup_ended(&mut self, finished: bool, announced: bool, found: usize, new: usize) {
        if finished {
            self.last = Some((Instant::now(), announced));
            self.misses = if new >= NEW_PEERS { 0 } else { self.misses + 1 };
            self.empty = if found == 0 { self.empty + 1 } else { 0 };
        } else {
            self.last = None;
        }
    }
}

/// One lookup of a torrent's peers.
struct Lookup<'a> {
    dht: &'a Dht,
    shared: &'a Weak<Shared>,
    /// Addresses earlier lookups found, to count new ones.
    seen: &'a mut HashSet<SocketAddr>,
    found: usize,
    new: usize,
}

impl Lookup<'_> {
    /// Hands every usable address to the torrent as it arrives. Returns whether the lookup
    /// finished (rather than being stopped by a policy change or a stopped node), with the
    /// addresses found and how many of them were new; `None` if the torrent is gone or
    /// discovery was cancelled.
    async fn run(
        mut self,
        info_hash: [u8; 20],
        announce: Option<u16>,
        policy: &mut Option<watch::Receiver<Policy>>,
        cancel: &CancellationToken,
    ) -> Option<(bool, usize, usize)> {
        let mut peers = self.dht.get_peers_once(Id20::new(info_hash), announce);
        let epoch = policy.as_ref().map_or(0, |p| p.borrow().epoch);
        let finished = loop {
            tokio::select! {
                () = cancel.cancelled() => return None,
                () = self.dht.cancellation_token().cancelled() => {
                    tokio::select! {
                        () = cancel.cancelled() => return None,
                        () = tokio::time::sleep(RESTART_AFTER) => {},
                    }
                    break false;
                },
                addr = peers.next() => {
                    let Some(addr) = addr else {
                        break true;
                    };
                    self.add(addr)?;
                }
                ok = changed(policy) => {
                    if !ok {
                        return None;
                    }
                    if !allowed(policy.as_ref()) || policy.as_ref().is_some_and(|p| p.borrow().epoch != epoch) {
                        break false;
                    }
                }
            }
        };
        Some((finished, self.found, self.new))
    }

    /// `None` if the torrent is gone.
    fn add(&mut self, addr: SocketAddr) -> Option<()> {
        let shared = self.shared.upgrade()?;
        if usable(addr) {
            self.found += 1;
            if self.seen.len() >= SEEN_LIMIT {
                self.seen.clear();
            }
            if self.seen.insert(addr) {
                self.new += 1;
            }
            shared.add_candidates([addr]);
        }
        Some(())
    }
}

/// Only public addresses from the DHT: anyone can store an address there.
fn usable(addr: SocketAddr) -> bool {
    addr.port() != 0 && torrentplayer_net::is_public_ip(addr.ip())
}

/// The most peers [`bootstrap_peers`] returns.
const BOOTSTRAP_PEERS: usize = 200;
/// How long [`bootstrap_peers`] keeps collecting after the first peer: a lookup finds most of
/// a swarm's DHT peers within a second or two of the first.
const BOOTSTRAP_SETTLE: Duration = Duration::from_secs(2);

/// Finds a bounded set of DHT peers through the selected network for magnet metadata: up to
/// 200, collected for 2 seconds after the first one, until the lookup ends, or until
/// `deadline`. They are all the peers the metadata fetch gets, and many addresses in the DHT
/// are stale or unreachable, so a handful is not enough.
/// The temporary node is cancelled on return or when the caller is cancelled.
pub async fn bootstrap_peers(
    network: torrentplayer_net::Network,
    info_hash: [u8; 20],
    deadline: Duration,
) -> Vec<SocketAddr> {
    let cancel = CancellationToken::new();
    let _stop = cancel.clone().drop_guard();
    let policy = network.subscribe();
    let node = DhtNode::new(cancel, Some(network), None);
    let mut peers = Vec::new();
    let _ = tokio::time::timeout(deadline, async {
        let Some(dht) = node.get(Some(&policy)).await else {
            return;
        };
        let mut stream = dht.get_peers_once(Id20::new(info_hash), None);
        let settled = tokio::time::sleep(deadline);
        tokio::pin!(settled);
        loop {
            tokio::select! {
                () = dht.cancellation_token().cancelled() => break,
                () = &mut settled => break,
                peer = stream.next() => match peer {
                    Some(peer) if usable(peer) && !peers.contains(&peer) => {
                        if peers.is_empty() {
                            settled
                                .as_mut()
                                .reset(tokio::time::Instant::now() + BOOTSTRAP_SETTLE);
                        }
                        peers.push(peer);
                        if peers.len() >= BOOTSTRAP_PEERS {
                            break;
                        }
                    },
                    Some(_) => {},
                    None => break,
                },
            }
        }
    })
    .await;
    peers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(traffic: bool, udp: bool) -> Policy {
        Policy {
            epoch: 1,
            traffic_allowed: traffic,
            protected: !udp,
            udp_allowed: udp,
            bound_udp_allowed: udp,
            dht_allowed: true,
            trackers_allowed: true,
        }
    }

    #[test]
    fn runs_only_while_udp_is_allowed() {
        assert!(allowed(None), "no network manager: direct");
        let (tx, rx) = watch::channel(policy(true, true));
        assert!(allowed(Some(&rx)));
        tx.send_replace(policy(true, false));
        assert!(!allowed(Some(&rx)), "a protected mode");
        tx.send_replace(policy(false, true));
        assert!(!allowed(Some(&rx)), "the kill switch");
        let mut disabled = policy(true, true);
        disabled.dht_allowed = false;
        tx.send_replace(disabled);
        assert!(!allowed(Some(&rx)), "the proxy DHT switch");
    }

    #[tokio::test]
    async fn never_starts_while_the_policy_disallows_it() {
        // A mode change that lands between `discover`'s policy check and the start must not
        // leave a node running: the start checks the policy again under the lock.
        let node = DhtNode::new(CancellationToken::new(), None, None);
        let (tx, rx) = watch::channel(policy(true, false));
        assert!(node.get(Some(&rx)).await.is_none(), "a protected mode");
        tx.send_replace(policy(false, true));
        assert!(node.get(Some(&rx)).await.is_none(), "the kill switch");
        let mut disabled = policy(true, true);
        disabled.dht_allowed = false;
        tx.send_replace(disabled);
        assert!(node.get(Some(&rx)).await.is_none(), "DHT disabled");
        let mut protected = policy(true, true);
        protected.protected = true;
        tx.send_replace(protected);
        assert!(
            node.get(Some(&rx)).await.is_none(),
            "no scoped network transport"
        );
        assert!(node.running.lock().await.is_none(), "nothing was started");
    }

    fn demand(live_peers: usize) -> Demand {
        Demand {
            paused: false,
            idle: false,
            live_peers,
            peer_limit: 40,
            listen_port: None,
        }
    }

    #[test]
    fn looks_up_often_only_while_peers_are_wanted() {
        let few = demand(3);
        assert_eq!(next_lookup(few, false, 0, 0), Some(WANTING_EVERY));
        assert_eq!(next_lookup(few, false, 1, 0), Some(WANTING_EVERY * 2));
        assert_eq!(next_lookup(few, false, 2, 0), Some(WANTING_EVERY * 4));
        assert_eq!(
            next_lookup(few, false, 30, 0),
            Some(SATISFIED_EVERY),
            "lookups that find nothing new back off to the satisfied pace"
        );
        assert_eq!(
            next_lookup(demand(20), false, 0, 0),
            Some(SATISFIED_EVERY),
            "half the peer limit is enough"
        );
        assert_eq!(next_lookup(demand(19), false, 0, 0), Some(WANTING_EVERY));

        let paused = Demand {
            paused: true,
            ..few
        };
        assert_eq!(next_lookup(paused, false, 0, 0), None, "never while paused");
        assert_eq!(
            next_lookup(paused, true, 0, 0),
            None,
            "not even to announce"
        );

        let idle = Demand { idle: true, ..few };
        assert_eq!(
            next_lookup(idle, false, 0, 0),
            None,
            "nothing to download: no lookups"
        );
        assert_eq!(
            next_lookup(idle, true, 0, 0),
            Some(REANNOUNCE_EVERY),
            "an announcing torrent stays announced"
        );
        assert!(REANNOUNCE_EVERY < Duration::from_secs(30 * 60));

        // Lookups that found nothing at all are retried soon, then less and less often.
        assert_eq!(next_lookup(few, false, 1, 1), Some(EMPTY_RETRY));
        assert_eq!(next_lookup(few, false, 2, 2), Some(EMPTY_RETRY * 2));
        assert_eq!(next_lookup(demand(30), false, 3, 3), Some(EMPTY_RETRY * 4));
        assert_eq!(next_lookup(few, false, 40, 40), Some(SATISFIED_EVERY));
        assert_eq!(next_lookup(paused, false, 1, 1), None);
        assert_eq!(next_lookup(idle, false, 1, 1), None);
    }

    #[test]
    fn announces_only_while_peers_can_reach_the_accepted_port() {
        use torrentplayer_net::ModeKind;
        assert_eq!(
            announce_port(Some(6881), ModeKind::Direct, Some(6881)),
            Some(6881),
            "the router confirms a mapping"
        );
        assert_eq!(
            announce_port(Some(6881), ModeKind::Direct, None),
            None,
            "listening without a mapping"
        );
        assert_eq!(
            announce_port(Some(6881), ModeKind::WireGuard, Some(6881)),
            Some(6881),
            "a peer connected in through the tunnel"
        );
        assert_eq!(
            announce_port(Some(6881), ModeKind::WireGuard, None),
            None,
            "nobody connected in through the tunnel"
        );
        assert_eq!(
            announce_port(None, ModeKind::Direct, Some(6881)),
            None,
            "incoming peers are off for this engine"
        );
        assert_eq!(
            announce_port(Some(6881), ModeKind::Socks5, Some(6881)),
            None,
            "never through a proxy"
        );
        assert_eq!(
            announce_port(Some(6881), ModeKind::Direct, Some(7000)),
            None,
            "another port"
        );
    }

    #[tokio::test]
    async fn a_single_lookup_ends_by_itself() {
        // The responder names only itself, so the IPv4 lookup runs out of nodes to ask after
        // one query, and the IPv6 one (no IPv6 contacts) gives up after a second.
        let (contact, responder) = fake_contact().await;
        let node = local_node(vec![contact.to_string()]).await;
        knows_a_node(&node).await;
        let mut lookup = node.get_peers_once(Id20::new([7; 20]), None);
        tokio::time::timeout(Duration::from_secs(5), async {
            while lookup.next().await.is_some() {}
        })
        .await
        .expect("the lookup ends once no query is left");
        node.cancellation_token().cancel();
        responder.abort();
    }

    /// The id of [`recording_responder`], and the info hash its tests look up.
    const ID: &[u8; 20] = b"ABCDEFGHIJKLMNOPQRST";

    /// A query seen by [`recording_responder`]: method, read-only flag (BEP 43), and for
    /// announcements the port and `implied_port`.
    type Query = (Vec<u8>, Option<i64>, Option<i64>, Option<i64>);

    /// A responder whose id is the info hash itself (as close as a node can be), giving a token
    /// with every reply and recording the queries it gets.
    async fn recording_responder() -> (
        SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<Query>,
        tokio::task::JoinHandle<()>,
    ) {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let contact = socket.local_addr().unwrap();
        let (tx, queries) = tokio::sync::mpsc::unbounded_channel();
        let responder = tokio::spawn(async move {
            let mut buf = [0; 1500];
            while let Ok((len, from)) = socket.recv_from(&mut buf).await {
                let Ok(request) = crate::bencode::decode(&buf[..len]) else {
                    continue;
                };
                let field = |key: &[u8]| {
                    request
                        .get(b"a")
                        .and_then(|a| a.get(key))
                        .and_then(crate::bencode::Value::as_int)
                };
                let method = request
                    .get(b"q")
                    .and_then(crate::bencode::Value::as_bytes)
                    .unwrap_or_default()
                    .to_vec();
                let read_only = request.get(b"ro").and_then(crate::bencode::Value::as_int);
                let _ = tx.send((method, read_only, field(b"port"), field(b"implied_port")));
                let Some(transaction) = request.get(b"t").and_then(crate::bencode::Value::as_bytes)
                else {
                    continue;
                };
                let mut reply = b"d1:rd2:id20:".to_vec();
                reply.extend(ID);
                reply.extend(b"5:nodes26:");
                reply.extend(ID);
                reply.extend([127, 0, 0, 1]);
                reply.extend(contact.port().to_be_bytes());
                reply.extend(b"5:token4:test");
                reply.extend(format!("e1:t{}:", transaction.len()).bytes());
                reply.extend(transaction);
                reply.extend(b"1:y1:re");
                let _ = socket.send_to(&reply, from).await;
            }
        });
        (contact, queries, responder)
    }

    /// Runs one lookup of [`ID`], announcing `announce`, and returns the queries the
    /// responder got.
    async fn lookup_queries(
        node: &Dht,
        announce: Option<u16>,
        queries: &mut tokio::sync::mpsc::UnboundedReceiver<Query>,
    ) -> Vec<Query> {
        while queries.try_recv().is_ok() {}
        let mut lookup = node.get_peers_once(Id20::new(*ID), announce);
        while lookup.next().await.is_some() {}
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut seen = Vec::new();
        while let Ok(query) = queries.try_recv() {
            seen.push(query);
        }
        seen
    }

    /// No announcement, and every query says the node is read-only.
    fn unreachable(seen: &[Query]) -> bool {
        seen.iter().any(|(method, ..)| method == b"get_peers")
            && seen
                .iter()
                .all(|(method, read_only, ..)| method != b"announce_peer" && *read_only == Some(1))
    }

    /// Announced with `port` (given, not implied: the DHT's own port is not the peer port),
    /// and no query says the node is read-only.
    fn announced(seen: &[Query], port: u16) -> bool {
        seen.iter().all(|(_, read_only, ..)| read_only.is_none())
            && seen.iter().any(|(method, _, announced, implied)| {
                method == b"announce_peer"
                    && *announced == Some(i64::from(port))
                    && *implied == Some(0)
            })
    }

    #[tokio::test]
    async fn a_lookup_announces_the_listening_port_to_close_nodes_unless_read_only() {
        let (contact, mut queries, responder) = recording_responder().await;
        let node = local_node(vec![contact.to_string()]).await;
        knows_a_node(&node).await;

        node.set_read_only(true);
        let seen = lookup_queries(&node, None, &mut queries).await;
        assert!(unreachable(&seen), "{seen:?}");

        node.set_read_only(false);
        let seen = lookup_queries(&node, Some(51413), &mut queries).await;
        assert!(announced(&seen, 51413), "{seen:?}");
        node.cancellation_token().cancel();
        responder.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_node_is_read_write_and_announces_only_while_the_port_is_reachable() {
        use torrentplayer_net::{IncomingConfig, Network, NetworkConfig, test_util::TestGateway};
        let public = std::net::IpAddr::from([93, 184, 216, 34]);
        let gateway = TestGateway::start(public).await.unwrap();
        let net = Network::start(
            NetworkConfig::default(),
            torrentplayer_net::NetworkOptions {
                upnp_lease: Duration::from_secs(1),
                upnp_control_url: Some(gateway.control_url()),
                reachable_after_inbound: Duration::from_secs(2),
                ..loopback_options()
            },
        )
        .await
        .unwrap();
        let _peers = net.incoming_peers();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let listen = |upnp| IncomingConfig {
            enabled: true,
            port,
            upnp,
        };
        net.set_incoming(listen(false));

        // The engine's node, started as `DhtNode::get` starts it, but on loopback.
        let (contact, mut queries, responder) = recording_responder().await;
        let node = Arc::new(DhtNode::new(
            CancellationToken::new(),
            Some(net.clone()),
            None,
        ));
        let dht = local_node(vec![contact.to_string()]).await;
        dht.set_read_only(!node.reachable.load(Ordering::Relaxed));
        *node.running.lock().await = Some((Arc::clone(&dht), CancellationToken::new(), 0));
        knows_a_node(&dht).await;
        tokio::spawn(follow_reachability(Arc::clone(&node), net.clone(), true));
        let wait_reachable = |want: bool| {
            let node = Arc::clone(&node);
            async move {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while node.reachable.load(Ordering::Relaxed) != want {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("reachable never became {want}"));
            }
        };
        let announce =
            || announce_port(net.incoming_port(), net.status().mode, net.reachable_port());

        // Listening, but no mapping: read-only, and no announcement.
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(
                net.status().incoming,
                torrentplayer_net::IncomingStatus::Listening { .. }
            ) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!node.reachable.load(Ordering::Relaxed));
        assert_eq!(announce(), None);
        let seen = lookup_queries(&dht, announce(), &mut queries).await;
        assert!(unreachable(&seen), "{seen:?}");

        // The router confirms a mapping: read-write, and the port is announced.
        net.set_incoming(listen(true));
        wait_reachable(true).await;
        assert_eq!(announce(), Some(port));
        let seen = lookup_queries(&dht, announce(), &mut queries).await;
        assert!(announced(&seen, port), "{seen:?}");

        // The mapping lapses: read-only again, without waiting for a lookup.
        gateway.set_external(None);
        wait_reachable(false).await;
        assert_eq!(announce(), None);
        let seen = lookup_queries(&dht, announce(), &mut queries).await;
        assert!(unreachable(&seen), "{seen:?}");

        // A peer connects in (a port forwarded by hand or by a VPN provider): read-write and
        // announced, until no peer has connected in for a while.
        let _client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        wait_reachable(true).await;
        assert_eq!(announce(), Some(port));
        let seen = lookup_queries(&dht, announce(), &mut queries).await;
        assert!(announced(&seen, port), "{seen:?}");
        wait_reachable(false).await;
        assert_eq!(announce(), None);

        node.cancel.cancel();
        dht.cancellation_token().cancel();
        responder.abort();
        net.shutdown();
    }

    #[test]
    fn uses_only_public_addresses() {
        assert!(usable("8.8.8.8:6881".parse().unwrap()));
        assert!(!usable("8.8.8.8:0".parse().unwrap()));
        assert!(!usable("192.168.0.2:6881".parse().unwrap()));
        assert!(!usable("127.0.0.1:6881".parse().unwrap()));
        assert!(!usable("[fe80::1]:6881".parse().unwrap()));
    }

    /// A node on loopback that bootstraps only from `bootstrap`.
    async fn local_node(bootstrap: Vec<String>) -> Dht {
        DhtBuilder::with_config(DhtConfig {
            listen_addr: Some("127.0.0.1:0".parse().unwrap()),
            bootstrap_addrs: Some(bootstrap),
            ..DhtConfig::default()
        })
        .await
        .unwrap()
    }

    /// Waits until `dht` has a node in its routing table.
    async fn knows_a_node(dht: &Dht) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while dht.stats().routing_table_size == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the node learns a contact");
    }

    fn state_file(path: PathBuf) -> StateFile {
        StateFile {
            path,
            written: std::sync::Mutex::new(None),
        }
    }

    /// A loopback DHT responder that names itself in every reply, so a node bootstrapping from
    /// it adds it to its routing table (nodes are learned only from replies).
    async fn fake_contact() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buf = [0; 1500];
            while let Ok((len, from)) = socket.recv_from(&mut buf).await {
                let Ok(request) = crate::bencode::decode(&buf[..len]) else {
                    continue;
                };
                let Some(transaction) = request.get(b"t").and_then(crate::bencode::Value::as_bytes)
                else {
                    continue;
                };
                let mut reply =
                    b"d1:rd2:id20:ABCDEFGHIJKLMNOPQRST5:nodes26:ABCDEFGHIJKLMNOPQRST".to_vec();
                reply.extend([127, 0, 0, 1]);
                reply.extend(addr.port().to_be_bytes());
                reply.extend(format!("e1:t{}:", transaction.len()).bytes());
                reply.extend(transaction);
                reply.extend(b"1:y1:re");
                let _ = socket.send_to(&reply, from).await;
            }
        });
        (addr, task)
    }

    #[tokio::test]
    async fn saved_tables_restore_the_node_id_and_its_contacts() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_file(dir.path().join("dht").join("swarm.json"));
        let (contact, responder) = fake_contact().await;
        let node = local_node(vec![contact.to_string()]).await;
        knows_a_node(&node).await;
        state.save(&node).unwrap();
        // Unchanged tables are not written again (times since contact are not saved).
        let modified = std::fs::metadata(&state.path).unwrap().modified().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        state.save(&node).unwrap();
        let after = std::fs::metadata(&state.path).unwrap().modified().unwrap();
        assert_eq!(modified, after, "an unchanged table is not rewritten");
        node.cancellation_token().cancel();

        let saved = state.load().await.expect("the tables were saved");
        let mut config = DhtConfig {
            listen_addr: Some("127.0.0.1:0".parse().unwrap()),
            bootstrap_addrs: Some(Vec::new()),
            routing_table: Some(saved.table),
            routing_table_v6: Some(saved.table_v6),
            ..DhtConfig::default()
        };
        if let Some(table) = &config.routing_table {
            config.peer_id = Some(table.id());
        }
        let restored = DhtBuilder::with_config(config).await.unwrap();
        assert_eq!(restored.stats().id, node.stats().id);
        assert!(restored.stats().routing_table_size > 0, "contacts restored");
        restored.cancellation_token().cancel();
        responder.abort();
    }

    #[tokio::test]
    async fn an_empty_node_or_a_damaged_file_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_file(dir.path().join("swarm.json"));
        let node = local_node(Vec::new()).await;
        state.save(&node).unwrap();
        assert!(!state.path.exists(), "nothing worth keeping");
        node.cancellation_token().cancel();

        std::fs::write(&state.path, b"{\"table\":").unwrap();
        let saved: Option<Saved<serde_json::Value>> = state.load().await;
        assert!(saved.is_none());
    }

    /// Network options whose sockets stay on loopback, where the test proxy is.
    fn loopback_options() -> torrentplayer_net::NetworkOptions {
        torrentplayer_net::NetworkOptions {
            loopback_only: true,
            ..torrentplayer_net::NetworkOptions::default()
        }
    }

    #[tokio::test]
    async fn protected_dht_bootstraps_and_finds_peers_through_a_proxy_without_local_dns() {
        use torrentplayer_net::{
            Network, NetworkConfig, NetworkMode, Socks5Config, socks5::Target,
            test_util::TestUdpProxy,
        };
        let proxy = TestUdpProxy::start(|target, bytes| {
            let request = crate::bencode::decode(bytes).ok()?;
            let transaction = request.get(b"t")?.as_bytes()?;
            let query = request.get(b"q")?.as_bytes()?;
            let source = match target {
                Target::Addr(addr) => *addr,
                Target::Domain(_, port) => SocketAddr::from(([8, 8, 8, 8], *port)),
            };
            let mut reply = b"d1:rd2:id20:ABCDEFGHIJKLMNOPQRST".to_vec();
            if query == b"get_peers" {
                reply.extend(b"5:token4:test6:valuesl6:");
                reply.extend([9, 9, 9, 9, 0x1A, 0xE1]);
                reply.extend(b"e");
            } else if query == b"find_node" {
                reply.extend(b"5:nodes26:ABCDEFGHIJKLMNOPQRST");
                reply.extend([8, 8, 8, 8]);
                reply.extend(source.port().to_be_bytes());
            }
            reply.extend(format!("e1:t{}:", transaction.len()).bytes());
            reply.extend(transaction);
            reply.extend(b"1:y1:re");
            Some((source, reply))
        })
        .await
        .unwrap();
        let net = Network::start(
            NetworkConfig {
                mode: NetworkMode::Socks5(Socks5Config {
                    host: proxy.addr().ip().to_string(),
                    port: proxy.addr().port(),
                    username: None,
                    password: None,
                    remote_dns: true,
                    proxy_trackers: true,
                    proxy_dht: true,
                }),
                allow_ipv6: false,
                hold: None,
            },
            loopback_options(),
        )
        .await
        .unwrap();
        assert!(net.settled(Duration::from_secs(3)).await.traffic_allowed);
        let peers = bootstrap_peers(net.clone(), [5; 20], Duration::from_secs(3)).await;
        assert!(
            peers.contains(&"9.9.9.9:6881".parse().unwrap()),
            "{peers:?}"
        );
        assert!(
            proxy
                .targets()
                .iter()
                .any(|target| matches!(target, Target::Domain(_, _))),
            "bootstrap names went to the proxy"
        );
        let transport = crate::dht_transport::ScopedDht::open(net.clone())
            .await
            .unwrap();
        assert!(
            transport
                .send_to(b"blocked", "127.0.0.1:1".parse().unwrap())
                .await
                .is_err()
        );
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("swarm.json");
        let node = DhtNode::new(
            CancellationToken::new(),
            Some(net.clone()),
            Some(state_path.clone()),
        );
        let policy = net.subscribe();
        let dht = node.get(Some(&policy)).await.unwrap();
        knows_a_node(&dht).await;
        proxy.stop();
        tokio::time::timeout(Duration::from_secs(3), dht.cancellation_token().cancelled())
            .await
            .unwrap();
        assert!(
            node.running.lock().await.as_ref().unwrap().1.is_cancelled(),
            "worker exit marks the cached node dead so the next lookup can restart it"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !state_path.exists(),
            "a protected node's tables are never saved"
        );
        net.shutdown();
        assert!(
            transport
                .send_to(b"blocked", "8.8.8.8:6881".parse().unwrap())
                .await
                .is_err()
        );
    }
}
