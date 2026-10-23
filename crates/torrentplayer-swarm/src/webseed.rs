// SPDX-License-Identifier: MIT OR Apache-2.0

//! Web seeds (BEP 19): HTTP servers that hold the torrent's files, used as one more peer.
//!
//! Each web seed is a virtual peer in the scheduler that has every piece and never chokes, so
//! it gets requests by deadline and measured rate like any other peer. Its task turns runs of
//! adjacent requested blocks into HTTP range requests (a few in flight at once) and hands the
//! response back block by block. Every piece is still hash-checked, so a server with wrong data
//! is caught (and dropped) like a bad peer.
//!
//! Web seed URLs come from the `.torrent` file and are untrusted:
//!
//! - Requests go through TorrentPlayer's network manager when one is configured (proxy, bound
//!   interface, kill switch), as a public-only bridge client that refuses addresses on the
//!   user's network. Without one, a resolver drops non-public addresses instead.
//! - Literal hosts must be public, and redirects are followed (at most [`MAX_REDIRECTS`]) only
//!   to http and https URLs that pass the same check.
//! - A response must be `206 Partial Content` for the range asked (a `200` only for a range at
//!   the start of the file) and the expected file length; its body is read no further than the
//!   range, and every read times out.
//! - URLs are never logged: a web seed is identified by its index in the `.torrent` file.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::header::{CONTENT_ENCODING, CONTENT_RANGE, RANGE};
use reqwest::{StatusCode, Url};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};
use tracing::{debug, trace};

use crate::metainfo::{MetaFile, Metainfo};
use crate::sched::{PeerKey, Request};
use crate::torrent::Shared;
use crate::wire::Outgoing;

/// Most blocks fetched by one HTTP request (4 MiB).
const MAX_RUN_BLOCKS: usize = 256;
/// HTTP requests in flight per web seed.
const MAX_FETCHES: usize = 3;
/// While a request is in flight, the next starts only once this many blocks are waiting, so a
/// queue topped up one block at a time does not turn into one request per block.
const MIN_BATCH_BLOCKS: usize = 16;
/// Wait at most this long for response headers.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
/// Wait at most this long for each piece of a response body.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Back off after a failure, doubling per failure in a row, up to [`BACKOFF_MAX`].
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// Pause while the kill switch blocks traffic, without counting a failure.
const BLOCKED_PAUSE: Duration = Duration::from_secs(2);
/// Give up on a web seed after this many failures in a row.
const MAX_FAILURES: u32 = 10;
/// Redirects followed per request.
pub(crate) const MAX_REDIRECTS: usize = 5;

/// Why a fetch failed.
#[derive(Debug, thiserror::Error)]
enum FetchError {
    /// Worth retrying later: a timeout, a connection error, or a server error.
    #[error("{0}")]
    Transient(String),
    /// The server cannot serve this torrent: missing file, no range support, wrong length.
    #[error("{0}")]
    Permanent(String),
}

impl FetchError {
    /// A `reqwest` error, described by its kind only: its message would include the URL, which
    /// is never logged.
    fn http(error: &reqwest::Error) -> Self {
        let kind = if error.is_timeout() {
            "timed out"
        } else if error.is_connect() {
            "could not connect"
        } else if error.is_redirect() {
            "redirect refused"
        } else if error.is_body() || error.is_decode() {
            "the response body failed"
        } else {
            "the request failed"
        };
        if error.is_redirect() || error.is_builder() {
            Self::Permanent(kind.into())
        } else {
            Self::Transient(kind.into())
        }
    }
}

/// Whether a web seed (or HTTP tracker) URL's host may be contacted: http or https, and a
/// literal address only if it is public. Names are checked when they are resolved (by the bridge or
/// [`PublicResolver`]); `localhost` names are refused here.
pub(crate) fn url_allowed(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => torrentplayer_net::is_public_ip(ip.into()),
        Some(url::Host::Ipv6(ip)) => torrentplayer_net::is_public_ip(ip.into()),
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name != "localhost" && !name.ends_with(".localhost")
        }
        None => false,
    }
}

/// Resolves names to their public addresses only, for web seeds when no network manager is
/// configured (with one, its bridge does the same).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| torrentplayer_net::is_public_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("the web seed has no public address".into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The redirect policy for web seeds: at most [`MAX_REDIRECTS`], each to an allowed URL.
pub(crate) fn redirect_policy(allow_local: bool) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if allow_local || url_allowed(attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("redirect refused")
        }
    })
}

/// The URL of `file` on a web seed whose `url-list` entry is `base` (BEP 19): the entry itself
/// for a single-file torrent, unless it ends in `/`, in which case the torrent name is added;
/// for a multi-file torrent, the entry plus the torrent name and the file's path. `None` if a
/// path component is empty, `.`, or `..` (they would change which file is meant).
pub(crate) fn file_url(base: &Url, meta: &Metainfo, file: &MetaFile) -> Option<Url> {
    if !meta.multi_file && !base.path().ends_with('/') {
        return Some(base.clone());
    }
    let safe = |c: &str| !c.is_empty() && c != "." && c != "..";
    let mut url = base.clone();
    {
        let mut segments = url.path_segments_mut().ok()?;
        segments.pop_if_empty();
        if !safe(&meta.name) {
            return None;
        }
        segments.push(&meta.name);
        if meta.multi_file {
            for component in &file.path {
                if !safe(component) {
                    return None;
                }
                segments.push(component);
            }
        }
    }
    Some(url)
}

/// Parses `Content-Range: bytes FIRST-LAST/TOTAL` (TOTAL may be `*`).
fn content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    (first <= last && total.is_none_or(|t| last < t)).then_some((first, last, total))
}

/// One block of a run, at an absolute offset in the torrent.
#[derive(Debug, Clone, Copy)]
struct Block {
    abs: u64,
    request: Request,
}

/// Collects a run's bytes and hands each block to the scheduler as soon as it is complete.
struct Assembler {
    shared: Arc<Shared>,
    key: PeerKey,
    blocks: VecDeque<Block>,
    buf: Vec<u8>,
    /// Blocks of this run not yet delivered and still wanted.
    wanted: Arc<Mutex<BTreeSet<u64>>>,
}

impl Assembler {
    /// Appends bytes; returns whether anything of the run is still wanted.
    fn push(&mut self, mut data: &[u8]) -> bool {
        while let Some(block) = self.blocks.front().copied() {
            let need = block.request.len as usize - self.buf.len();
            let take = need.min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() < block.request.len as usize {
                break;
            }
            self.blocks.pop_front();
            // A cancelled block is still delivered: another peer may be fetching it, and the
            // first copy to arrive wins.
            self.wanted
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&block.abs);
            crate::peer::receive_block(
                &self.shared,
                self.key,
                block.request.piece,
                block.request.begin,
                &self.buf,
            );
            self.buf.clear();
        }
        !self
            .wanted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }
}

/// A fetch in flight.
struct Fetch {
    /// Absolute byte range.
    start: u64,
    end: u64,
    wanted: Arc<Mutex<BTreeSet<u64>>>,
    blocks: Vec<Block>,
    abort: AbortHandle,
}

/// How a fetch ended without an error.
#[derive(Debug)]
enum Fetched {
    /// Everything arrived.
    Complete,
    /// Nothing more was wanted, or the server sent less than asked.
    Partial,
}

/// Runs the web seed at `base` (index `index` in the `.torrent`) until the torrent stops, the
/// torrent drops it for bad data, or it fails for good.
pub(crate) async fn run(
    shared: Arc<Shared>,
    client: reqwest::Client,
    base: Url,
    index: usize,
    key: PeerKey,
) {
    let (tx, mut rx) = mpsc::channel(crate::peer::SEND_QUEUE);
    {
        let mut sched = shared.sched();
        sched.add_peer(key);
        let all = vec![0xffu8; shared.meta.piece_count().div_ceil(8) as usize];
        sched.on_bitfield(key, &all);
        sched.on_unchoke(key);
    }
    shared
        .peers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, tx);
    shared.web_seed_started();
    debug!(web_seed = index, peer = key, "web seed added");
    shared.request_replan(false);

    let mut seed = WebSeed {
        shared: Arc::clone(&shared),
        client,
        base,
        index,
        key,
        pending: BTreeMap::new(),
        order: VecDeque::new(),
        fetches: Vec::new(),
        tasks: JoinSet::new(),
        next_fetch: 0,
        failures: 0,
        paused_until: None,
    };
    seed.serve(&mut rx).await;
    seed.tasks.abort_all();
    shared
        .peers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&key);
    shared.sched().remove_peer(key);
    shared.web_seed_finished();
    shared.request_replan(false);
}

struct WebSeed {
    shared: Arc<Shared>,
    client: reqwest::Client,
    base: Url,
    index: usize,
    key: PeerKey,
    /// Requested blocks not yet sent to the server, by absolute offset...
    pending: BTreeMap<u64, Request>,
    /// ...and in the order the scheduler asked for them (may hold stale offsets).
    order: VecDeque<u64>,
    fetches: Vec<(u64, Fetch)>,
    tasks: JoinSet<(u64, Result<Fetched, FetchError>)>,
    next_fetch: u64,
    failures: u32,
    /// Choked (by TorrentPlayer itself) until then, after a failure.
    paused_until: Option<Instant>,
}

impl WebSeed {
    async fn serve(&mut self, rx: &mut mpsc::Receiver<Outgoing>) {
        loop {
            let pause = self.paused_until;
            tokio::select! {
                () = self.shared.cancel.cancelled() => return,
                message = rx.recv() => {
                    let Some(message) = message else {
                        // The torrent dropped this web seed (it sent bad data).
                        debug!(web_seed = self.index, "web seed dropped");
                        return;
                    };
                    self.on_message(&message);
                    while let Ok(message) = rx.try_recv() {
                        self.on_message(&message);
                    }
                }
                Some(done) = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    // An error here is a fetch aborted after a cancel or a pause, which was
                    // already handled.
                    if let Ok((id, result)) = done
                        && !self.finished(id, result)
                    {
                        return;
                    }
                }
                () = sleep_until(pause), if pause.is_some() => {
                    self.paused_until = None;
                    self.shared.sched().on_unchoke(self.key);
                    let requests = self.shared.sched().fill(self.key, Instant::now());
                    self.shared.send_requests(self.key, &requests);
                }
            }
            self.start_fetches();
        }
    }

    fn on_message(&mut self, message: &Outgoing) {
        match message {
            Outgoing::Request { piece, begin, len } if self.paused_until.is_none() => {
                let abs = self.shared.meta.piece_offset(*piece) + u64::from(*begin);
                if self
                    .pending
                    .insert(
                        abs,
                        Request {
                            piece: *piece,
                            begin: *begin,
                            len: *len,
                        },
                    )
                    .is_none()
                {
                    self.order.push_back(abs);
                }
            }
            Outgoing::Cancel { piece, begin, .. } => {
                let abs = self.shared.meta.piece_offset(*piece) + u64::from(*begin);
                if self.pending.remove(&abs).is_some() {
                    return;
                }
                // Abort a fetch once nothing it would still deliver is wanted.
                let Some(i) = self
                    .fetches
                    .iter()
                    .position(|(_, f)| f.start <= abs && abs < f.end)
                else {
                    return;
                };
                let done = {
                    let mut wanted = self.fetches[i]
                        .1
                        .wanted
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    wanted.remove(&abs);
                    wanted.is_empty()
                };
                if done {
                    let (_, fetch) = self.fetches.swap_remove(i);
                    fetch.abort.abort();
                }
            }
            // Requests while paused were released by the pause; everything else (`have`,
            // interest, keep-alives) means nothing to a server.
            _ => {}
        }
    }

    /// Handles a finished fetch. Returns `false` if the web seed should stop.
    fn finished(&mut self, id: u64, result: Result<Fetched, FetchError>) -> bool {
        let Some(at) = self.fetches.iter().position(|(i, _)| *i == id) else {
            return true;
        };
        let (_, fetch) = self.fetches.swap_remove(at);
        let leftover: Vec<Block> = {
            let wanted = fetch.wanted.lock().unwrap_or_else(PoisonError::into_inner);
            fetch
                .blocks
                .iter()
                .filter(|b| wanted.contains(&b.abs))
                .copied()
                .collect()
        };
        match result {
            Ok(Fetched::Complete | Fetched::Partial) => {
                self.failures = 0;
                // Blocks the server did not send are asked for again, first.
                for block in leftover.into_iter().rev() {
                    if self.pending.insert(block.abs, block.request).is_none() {
                        self.order.push_front(block.abs);
                    }
                }
                true
            }
            Err(FetchError::Permanent(reason)) => {
                debug!(web_seed = self.index, %reason, "giving up on the web seed");
                false
            }
            Err(FetchError::Transient(reason)) => {
                let blocked = self
                    .shared
                    .cfg
                    .network
                    .as_ref()
                    .is_some_and(|n| !n.policy().traffic_allowed);
                let pause = if blocked {
                    BLOCKED_PAUSE
                } else {
                    self.failures += 1;
                    if self.failures >= MAX_FAILURES {
                        debug!(web_seed = self.index, %reason, "giving up on the web seed");
                        return false;
                    }
                    BACKOFF_BASE
                        .saturating_mul(1 << (self.failures - 1).min(16))
                        .min(BACKOFF_MAX)
                };
                debug!(web_seed = self.index, %reason, ?pause, "web seed request failed");
                self.pause(pause);
                true
            }
        }
    }

    /// Stops using the web seed for `pause`: its requests go back to other peers.
    fn pause(&mut self, pause: Duration) {
        self.tasks.abort_all();
        self.fetches.clear();
        self.pending.clear();
        self.order.clear();
        self.shared.sched().on_choke(self.key);
        self.shared.request_replan(false);
        self.paused_until = Some(Instant::now() + pause);
    }

    fn start_fetches(&mut self) {
        while self.paused_until.is_none()
            && self.fetches.len() < MAX_FETCHES
            && (self.fetches.is_empty() || self.pending.len() >= MIN_BATCH_BLOCKS)
        {
            let Some(run) = self.take_run() else {
                return;
            };
            let id = self.next_fetch;
            self.next_fetch += 1;
            let wanted: BTreeSet<u64> = run.iter().map(|b| b.abs).collect();
            let wanted = Arc::new(Mutex::new(wanted));
            let start = run[0].abs;
            let last = run[run.len() - 1];
            let end = last.abs + u64::from(last.request.len);
            let assembler = Assembler {
                shared: Arc::clone(&self.shared),
                key: self.key,
                blocks: run.iter().copied().collect(),
                buf: Vec::new(),
                wanted: Arc::clone(&wanted),
            };
            let job = fetch(
                Arc::clone(&self.shared),
                self.client.clone(),
                self.base.clone(),
                start,
                end,
                assembler,
            );
            let abort = self.tasks.spawn(async move { (id, job.await) });
            self.fetches.push((
                id,
                Fetch {
                    start,
                    end,
                    wanted,
                    blocks: run,
                    abort,
                },
            ));
        }
    }

    /// Takes the oldest pending block and the pending blocks adjacent to it, in file order.
    fn take_run(&mut self) -> Option<Vec<Block>> {
        let first = loop {
            let abs = self.order.pop_front()?;
            if let Some(request) = self.pending.remove(&abs) {
                break Block { abs, request };
            }
        };
        let mut run = VecDeque::from([first]);
        let mut end = first.abs + u64::from(first.request.len);
        while run.len() < MAX_RUN_BLOCKS
            && let Some(request) = self.pending.remove(&end)
        {
            run.push_back(Block { abs: end, request });
            end += u64::from(request.len);
        }
        while run.len() < MAX_RUN_BLOCKS
            && let Some((&abs, &request)) = self.pending.range(..run[0].abs).next_back()
            && abs + u64::from(request.len) == run[0].abs
        {
            self.pending.remove(&abs);
            run.push_front(Block { abs, request });
        }
        Some(run.into())
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// Fetches the absolute range `start..end`, file by file, feeding the assembler.
async fn fetch(
    shared: Arc<Shared>,
    client: reqwest::Client,
    base: Url,
    start: u64,
    end: u64,
    mut assembler: Assembler,
) -> Result<Fetched, FetchError> {
    let meta = Arc::clone(&shared.meta);
    for file in &meta.files {
        let file_end = file.offset + file.len;
        let (from, to) = (start.max(file.offset), end.min(file_end));
        if from >= to {
            continue;
        }
        if file.padding {
            // Padding files (BEP 47) are zeros and are not on the server.
            let zeros = vec![0u8; usize::try_from(to - from).unwrap_or(0)];
            if !assembler.push(&zeros) {
                return Ok(Fetched::Partial);
            }
            continue;
        }
        let url = file_url(&base, &meta, file)
            .ok_or_else(|| FetchError::Permanent("unsafe file path".into()))?;
        if !shared.cfg.allow_local_web_seeds && !url_allowed(&url) {
            return Err(FetchError::Permanent("the web seed is not public".into()));
        }
        let segment = (from - file.offset, to - file.offset, file.len);
        match fetch_segment(&shared, &client, url, segment, &mut assembler).await? {
            Fetched::Complete => {}
            partial @ Fetched::Partial => return Ok(partial),
        }
    }
    Ok(Fetched::Complete)
}

/// Fetches bytes `first..last_excl` of a file of length `file_len`.
async fn fetch_segment(
    shared: &Shared,
    client: &reqwest::Client,
    url: Url,
    (first, last_excl, file_len): (u64, u64, u64),
    assembler: &mut Assembler,
) -> Result<Fetched, FetchError> {
    let want = last_excl - first;
    let range = format!("bytes={first}-{}", last_excl - 1);
    let mut response = tokio::time::timeout(
        RESPONSE_TIMEOUT,
        client.get(url).header(RANGE, range).send(),
    )
    .await
    .map_err(|_| FetchError::Transient("no response".into()))?
    .map_err(|e| FetchError::http(&e))?;
    let status = response.status();
    match status {
        StatusCode::PARTIAL_CONTENT => {
            let header = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(content_range);
            match header {
                Some((f, _, total)) if f == first && total.is_none_or(|t| t == file_len) => {}
                Some((_, _, Some(total))) if total != file_len => {
                    return Err(FetchError::Permanent(
                        "the file has the wrong length".into(),
                    ));
                }
                _ => return Err(FetchError::Transient("unexpected content range".into())),
            }
        }
        // The server ignores ranges: usable only from the start of the file.
        StatusCode::OK if first == 0 => {
            if response.content_length().is_some_and(|l| l != file_len) {
                return Err(FetchError::Permanent(
                    "the file has the wrong length".into(),
                ));
            }
        }
        StatusCode::OK => {
            return Err(FetchError::Permanent(
                "the server does not support ranges".into(),
            ));
        }
        StatusCode::NOT_FOUND
        | StatusCode::GONE
        | StatusCode::UNAUTHORIZED
        | StatusCode::FORBIDDEN
        | StatusCode::RANGE_NOT_SATISFIABLE
        | StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS => {
            return Err(FetchError::Permanent(format!("status {}", status.as_u16())));
        }
        _ => return Err(FetchError::Transient(format!("status {}", status.as_u16()))),
    }
    if response
        .headers()
        .get(CONTENT_ENCODING)
        .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(FetchError::Permanent("the response is encoded".into()));
    }
    let mut received: u64 = 0;
    while received < want {
        let chunk = tokio::time::timeout(READ_TIMEOUT, response.chunk())
            .await
            .map_err(|_| FetchError::Transient("the response stalled".into()))?
            .map_err(|e| FetchError::http(&e))?;
        let Some(chunk) = chunk else {
            trace!(received, want, "short web seed response");
            return Ok(Fetched::Partial);
        };
        // Never more than the range asked for.
        let take = usize::try_from(want - received)
            .unwrap_or(usize::MAX)
            .min(chunk.len());
        received += take as u64;
        shared.throttle(take).await;
        shared
            .downloaded_web
            .fetch_add(take as u64, std::sync::atomic::Ordering::Relaxed);
        if !assembler.push(&chunk[..take]) {
            return Ok(Fetched::Partial);
        }
    }
    Ok(Fetched::Complete)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(multi: bool) -> Metainfo {
        let pieces = "0".repeat(20);
        let info = if multi {
            format!(
                "d5:filesld6:lengthi5e4:pathl3:sub5:a.mp4eed6:lengthi5e4:pathl4:b c?eee4:name7:Lecture12:piece lengthi16384e6:pieces20:{pieces}e"
            )
        } else {
            format!("d6:lengthi10e4:name5:a.mp412:piece lengthi16384e6:pieces20:{pieces}e")
        };
        Metainfo::from_bytes(format!("d4:info{info}e").as_bytes()).unwrap()
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn builds_bep19_urls() {
        let single = meta(false);
        let f = &single.files[0];
        assert_eq!(
            file_url(&url("https://h.example/x/a.mp4"), &single, f)
                .unwrap()
                .as_str(),
            "https://h.example/x/a.mp4"
        );
        assert_eq!(
            file_url(&url("https://h.example/x/"), &single, f)
                .unwrap()
                .as_str(),
            "https://h.example/x/a.mp4"
        );
        let multi = meta(true);
        assert_eq!(
            file_url(&url("https://h.example/dl/"), &multi, &multi.files[0])
                .unwrap()
                .as_str(),
            "https://h.example/dl/Lecture/sub/a.mp4"
        );
        // Without a trailing slash too, and with path characters escaped.
        assert_eq!(
            file_url(&url("https://h.example/dl?k=1"), &multi, &multi.files[1])
                .unwrap()
                .as_str(),
            "https://h.example/dl/Lecture/b%20c%3F?k=1"
        );
        let mut evil = multi.clone();
        evil.files[0].path = vec!["..".into(), "etc".into()];
        assert!(file_url(&url("https://h.example/dl/"), &evil, &evil.files[0]).is_none());
    }

    #[test]
    fn refuses_hosts_on_the_local_network() {
        for ok in [
            "https://archive.org/download/x",
            "http://93.184.216.34/x",
            "http://[2606:4700::1111]/x",
        ] {
            assert!(url_allowed(&url(ok)), "{ok}");
        }
        for bad in [
            "http://127.0.0.1/x",
            "http://localhost:8080/x",
            "http://LOCALHOST./x",
            "http://a.localhost/x",
            "http://10.0.0.1/x",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/x",
            "http://[fe80::1]/x",
            "http://[::ffff:192.168.1.1]/x",
            "ftp://archive.org/x",
            "file:///etc/passwd",
        ] {
            assert!(!url_allowed(&url(bad)), "{bad}");
        }
    }

    #[test]
    fn parses_content_ranges() {
        assert_eq!(content_range("bytes 0-99/1000"), Some((0, 99, Some(1000))));
        assert_eq!(content_range("bytes 5-5/*"), Some((5, 5, None)));
        assert_eq!(content_range("bytes 10-5/100"), None);
        assert_eq!(content_range("bytes 0-100/100"), None);
        assert_eq!(content_range("items 0-1/2"), None);
        assert_eq!(content_range("bytes -5/100"), None);
    }
}
