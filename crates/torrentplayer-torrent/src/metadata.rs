// SPDX-License-Identifier: MIT OR Apache-2.0

//! A small cache of torrent metadata fetched ahead of an add.
//!
//! [`StreamEngine::prefetch_metadata`](crate::StreamEngine::prefetch_metadata) fetches the
//! `.torrent` files of search results the user may play. Adding (or preloading) one of them
//! later takes its metadata, and the peers seen while fetching it, from here instead of asking
//! peers again. The cache holds at most [`MAX_ENTRIES`] torrents and [`MAX_TOTAL_BYTES`] in
//! all, evicting the least recently used first. Fetches of the same torrent wait for each
//! other (see [`MetadataCache::lock`]), so it is fetched once.
//!
//! It also remembers what download links gave ([`LinkTarget`]): a search site's download link
//! usually redirects to a magnet link, and following it costs the site a request to the
//! indexer, so each link is followed once.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use bytes::Bytes;

use crate::source::MAX_TORRENT_FILE_BYTES;

/// Most torrents kept.
pub(crate) const MAX_ENTRIES: usize = 64;
/// Most bytes of metadata kept in all.
pub(crate) const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
/// Most peers kept per torrent.
const MAX_PEERS: usize = 200;
/// Most download links remembered.
const MAX_LINKS: usize = 256;

/// What a download link gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkTarget {
    /// It redirects to this magnet link.
    Magnet(String),
    /// It serves the `.torrent` file with this info hash (kept with the metadata).
    Torrent(String),
}

/// A torrent's metadata and the peers seen while fetching it.
#[derive(Debug, Clone)]
pub(crate) struct Cached {
    pub(crate) bytes: Bytes,
    pub(crate) peers: Vec<SocketAddr>,
}

/// Metadata by lowercase hex info hash.
#[derive(Debug, Default)]
pub(crate) struct MetadataCache {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Each entry with when it was last used (a counter).
    entries: HashMap<String, (u64, Cached)>,
    clock: u64,
    bytes: usize,
    /// One lock per torrent being fetched; dead entries are pruned as new ones are added.
    fetching: HashMap<String, Weak<tokio::sync::Mutex<()>>>,
    /// Download links by URL, with when each was last used.
    links: HashMap<String, (u64, LinkTarget)>,
}

impl MetadataCache {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached metadata of a torrent, marking it recently used.
    pub(crate) fn get(&self, info_hash: &str) -> Option<Cached> {
        let mut inner = self.inner();
        inner.clock += 1;
        let now = inner.clock;
        inner.entries.get_mut(info_hash).map(|(used, cached)| {
            *used = now;
            cached.clone()
        })
    }

    /// Whether a torrent's metadata is cached (without marking it used).
    pub(crate) fn contains(&self, info_hash: &str) -> bool {
        self.inner().entries.contains_key(info_hash)
    }

    /// Keeps a torrent's metadata, evicting the least recently used entries to stay within
    /// the limits. Metadata larger than a `.torrent` file may be is not kept.
    pub(crate) fn put(&self, info_hash: String, bytes: Bytes, mut peers: Vec<SocketAddr>) {
        if bytes.is_empty() || bytes.len() > MAX_TORRENT_FILE_BYTES {
            return;
        }
        peers.sort_unstable();
        peers.dedup();
        peers.truncate(MAX_PEERS);
        let mut inner = self.inner();
        if let Some((_, old)) = inner.entries.remove(&info_hash) {
            inner.bytes -= old.bytes.len();
        }
        while inner.entries.len() >= MAX_ENTRIES || inner.bytes + bytes.len() > MAX_TOTAL_BYTES {
            let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(hash, _)| hash.clone())
            else {
                break;
            };
            if let Some((_, old)) = inner.entries.remove(&oldest) {
                inner.bytes -= old.bytes.len();
            }
        }
        inner.clock += 1;
        let now = inner.clock;
        inner.bytes += bytes.len();
        inner
            .entries
            .insert(info_hash, (now, Cached { bytes, peers }));
    }

    /// What a download link gave, if it was followed before.
    pub(crate) fn link(&self, url: &str) -> Option<LinkTarget> {
        let mut inner = self.inner();
        inner.clock += 1;
        let now = inner.clock;
        inner.links.get_mut(url).map(|(used, target)| {
            *used = now;
            target.clone()
        })
    }

    /// Remembers what a download link gave, forgetting the least recently used beyond
    /// [`MAX_LINKS`].
    pub(crate) fn put_link(&self, url: String, target: LinkTarget) {
        let mut inner = self.inner();
        if inner.links.len() >= MAX_LINKS
            && !inner.links.contains_key(&url)
            && let Some(oldest) = inner
                .links
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(url, _)| url.clone())
        {
            inner.links.remove(&oldest);
        }
        inner.clock += 1;
        let now = inner.clock;
        inner.links.insert(url, (now, target));
    }

    /// Waits until no one else is fetching this torrent's metadata, and holds that until the
    /// guard is dropped. Check the cache again after this returns.
    pub(crate) async fn lock(&self, info_hash: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut inner = self.inner();
            inner.fetching.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = inner.fetching.get(info_hash).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                inner
                    .fetching
                    .insert(info_hash.to_owned(), Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn hash(n: usize) -> String {
        format!("{n:040x}")
    }

    #[test]
    fn evicts_the_least_recently_used() {
        let cache = MetadataCache::default();
        for n in 0..MAX_ENTRIES {
            cache.put(hash(n), Bytes::from_static(b"d4:infoe"), Vec::new());
        }
        // Using the oldest keeps it; the next oldest goes instead.
        assert!(cache.get(&hash(0)).is_some());
        cache.put(hash(1000), Bytes::from_static(b"d4:infoe"), Vec::new());
        assert!(cache.contains(&hash(0)));
        assert!(!cache.contains(&hash(1)));
        assert!(cache.contains(&hash(1000)));
        assert_eq!(cache.inner().entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn stays_within_the_byte_budget_and_refuses_oversized_metadata() {
        let cache = MetadataCache::default();
        let big = Bytes::from(vec![b'x'; MAX_TORRENT_FILE_BYTES]);
        for n in 0..6 {
            cache.put(hash(n), big.clone(), Vec::new());
        }
        assert!(cache.inner().bytes <= MAX_TOTAL_BYTES);
        assert!(!cache.contains(&hash(0)) && cache.contains(&hash(5)));
        cache.put(
            hash(99),
            Bytes::from(vec![b'x'; MAX_TORRENT_FILE_BYTES + 1]),
            Vec::new(),
        );
        assert!(!cache.contains(&hash(99)));
        cache.put(hash(98), Bytes::new(), Vec::new());
        assert!(!cache.contains(&hash(98)));
        // Replacing an entry does not count it twice.
        let before = cache.inner().bytes;
        cache.put(hash(5), big, Vec::new());
        assert_eq!(cache.inner().bytes, before);
    }

    #[test]
    fn keeps_a_bounded_set_of_peers() {
        let cache = MetadataCache::default();
        let peer = |port| SocketAddr::from(([127, 0, 0, 1], port));
        let mut peers: Vec<SocketAddr> = (1..=500).map(peer).collect();
        peers.push(peer(1));
        cache.put(hash(1), Bytes::from_static(b"d4:infoe"), peers);
        let cached = cache.get(&hash(1)).unwrap();
        assert_eq!(cached.peers.len(), MAX_PEERS);
        assert_eq!(cached.peers[0], peer(1));
    }

    #[test]
    fn remembers_a_bounded_number_of_links() {
        let cache = MetadataCache::default();
        let url = |n: usize| format!("https://search.example/download/{n}");
        for n in 0..MAX_LINKS {
            cache.put_link(
                url(n),
                LinkTarget::Magnet(format!("magnet:?xt=urn:btih:{}", hash(n))),
            );
        }
        assert!(cache.link(&url(0)).is_some(), "used, so kept");
        cache.put_link(url(9999), LinkTarget::Torrent(hash(1)));
        assert!(
            cache.link(&url(1)).is_none(),
            "the least recently used is forgotten"
        );
        assert_eq!(cache.link(&url(9999)), Some(LinkTarget::Torrent(hash(1))));
        assert_eq!(cache.inner().links.len(), MAX_LINKS);
    }

    #[tokio::test]
    async fn fetches_of_one_torrent_wait_for_each_other() {
        let cache = Arc::new(MetadataCache::default());
        let first = cache.lock(&hash(1)).await;
        let waiting = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move {
                let _guard = cache.lock(&hash(1)).await;
                cache.contains(&hash(1))
            })
        };
        // Another torrent is not held up.
        drop(cache.lock(&hash(2)).await);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!waiting.is_finished());
        cache.put(hash(1), Bytes::from_static(b"d4:infoe"), Vec::new());
        drop(first);
        assert!(
            waiting.await.unwrap(),
            "the second fetch finds the first one's result"
        );
    }
}
