// SPDX-License-Identifier: MIT OR Apache-2.0

//! Validating, filtering, and merging search results.
//!
//! Providers produce [`Candidate`]s straight from untrusted responses; [`build_response`] turns
//! them into bounded [`SearchResult`]s that are safe to show and add.

use std::collections::HashMap;

use crate::quality::{ContentKind, Quality};
use crate::rank::{reason, watch_score};

/// Largest provider response accepted, in bytes.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Longest title kept, in characters.
pub const MAX_TITLE_CHARS: usize = 300;
/// Longest magnet link accepted, in bytes.
pub const MAX_MAGNET_BYTES: usize = 8 * 1024;
const MAX_LABEL_CHARS: usize = 100;
const MAX_DATE_CHARS: usize = 64;
const MAX_ERROR_CHARS: usize = 200;
const MAX_URL_BYTES: usize = 2048;
const MAGNET_PREFIX: &str = "magnet:?xt=urn:btih:";

/// One result as a provider reported it. Every field is untrusted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Candidate {
    /// Torrent name.
    pub title: String,
    /// Indexer (tracker site) that listed it.
    pub indexer: Option<String>,
    /// Provider category, for example `Movies/HD` or `XXX`.
    pub category: Option<String>,
    /// The provider files it under an adult category id (Torznab 6000–6999), whatever its
    /// category label says.
    pub adult: bool,
    /// What the provider's category ids say it is (see
    /// [`kind_of_category_ids`](crate::quality::kind_of_category_ids)); used when the title and
    /// category label do not say.
    pub category_kind: Option<ContentKind>,
    /// Total size in bytes.
    pub size: Option<u64>,
    /// Peers with the complete torrent.
    pub seeders: Option<u32>,
    /// All peers, seeders included.
    pub peers: Option<u32>,
    /// When the indexer published it (ISO 8601).
    pub published: Option<String>,
    /// Hex info hash.
    pub infohash: Option<String>,
    /// Magnet link.
    pub magnet: Option<String>,
    /// A link on the provider's own site that resolves the torrent, for indexers that give
    /// neither a magnet link nor an info hash. Providers set it only for their own links.
    pub download: Option<String>,
    /// The indexer's page for the torrent.
    pub details: Option<String>,
    /// License, when the provider knows it (SPDX identifier or a short name).
    pub license: Option<String>,
    /// The provider's verdict on the swarm: `good`, `low`, `dead`, or `unknown`.
    pub health: Option<String>,
    /// The seed count is a placeholder that fake listings use (and so is not kept).
    pub suspicious: bool,
}

/// A validated result, ready to show and add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// Torrent name: no control characters, at most [`MAX_TITLE_CHARS`] characters.
    pub title: String,
    /// Total size in bytes.
    pub size_bytes: Option<u64>,
    /// Peers with the complete torrent.
    pub seeders: Option<u32>,
    /// All peers, seeders included.
    pub peers: Option<u32>,
    /// Identifies the result within a response, and stays the same as more copies of it
    /// arrive: the info hash, or `download:` and the download link if the first copy seen was
    /// offered as a download link (even once its magnet link is known). Copies are merged by
    /// info hash when it is known, and by download link otherwise.
    pub key: String,
    /// What to add: the magnet link, or else the download link.
    pub link: String,
    /// A magnet link starting with `magnet:?xt=urn:btih:`, if known.
    pub magnet: Option<String>,
    /// Lowercase hex info hash, if known.
    pub info_hash: Option<String>,
    /// The provider's download link (http or https), if it offers one. Following it usually
    /// leads to a magnet link; the provider's site is asked each time a new one is followed.
    pub download: Option<String>,
    /// The indexer's page for the torrent (http or https only).
    pub details_url: Option<String>,
    /// Every indexer that listed this torrent.
    pub indexers: Vec<String>,
    /// Provider category.
    pub category: Option<String>,
    /// When it was published (ISO 8601), as reported.
    pub published: Option<String>,
    /// License, if known. `None` means unknown, not "unlicensed".
    pub license: Option<String>,
    /// The provider's verdict on the swarm: `good`, `low`, `dead`, or `unknown`.
    pub health: Option<String>,
    /// Every copy came with a placeholder seed count (a sign of a fake listing).
    pub suspicious: bool,
    /// Quality parsed from the title, category, and size.
    pub quality: Quality,
    /// Watch score in tenths of a point (see [`crate::rank`]).
    pub watch_score: i32,
    /// Short explanation of the quality, for example `1080p · 412 seeders · plays in built-in
    /// player`.
    pub reason: String,
    /// The recommended result of this search (set by [`crate::rank::rank`]).
    pub best_pick: bool,
}

/// How one indexer behind a provider did on this search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerStatus {
    /// The provider's id for the indexer, if it has one (`[a-z0-9_-]`, at most 64 characters).
    pub id: Option<String>,
    /// Indexer name.
    pub name: String,
    /// Results it returned.
    pub results: u32,
    /// Why it failed this time, if it did.
    pub error: Option<String>,
}

/// Results of one search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResponse {
    /// Validated results, most seeders first.
    pub results: Vec<SearchResult>,
    /// Per-indexer outcomes.
    pub indexers: Vec<IndexerStatus>,
    /// Results hidden because they are in an adult category.
    pub excluded_adult: u32,
    /// Results dropped because they could not be added (no valid magnet link, info hash, or
    /// download link).
    pub dropped: u32,
}

/// Turns raw candidates into a validated, de-duplicated, sorted response.
///
/// With `include_adult` false, results in adult categories are removed and counted.
#[must_use]
pub fn build_response(
    candidates: Vec<Candidate>,
    indexers: Vec<IndexerStatus>,
    include_adult: bool,
) -> SearchResponse {
    let mut response = SearchResponse {
        indexers: clean_statuses(indexers),
        ..SearchResponse::default()
    };
    // Results by what identifies a torrent: its info hash, or its download link.
    let mut by_identity: HashMap<String, usize> = HashMap::new();
    // What each result's category ids say, by position.
    let mut id_kinds: Vec<Option<ContentKind>> = Vec::new();
    for candidate in candidates {
        let category = candidate.category.as_deref().and_then(clean_label);
        if !include_adult && (candidate.adult || category.as_deref().is_some_and(is_adult)) {
            response.excluded_adult += 1;
            continue;
        }
        let Some(address) = addressable(&candidate) else {
            response.dropped += 1;
            continue;
        };
        let identity = address.identity();
        let indexer = candidate.indexer.as_deref().and_then(clean_label);
        if let Some(&at) = by_identity.get(&identity) {
            // Video if any copy is filed as video.
            if id_kinds[at] != Some(ContentKind::Video) && candidate.category_kind.is_some() {
                id_kinds[at] = candidate.category_kind;
            }
            let existing = &mut response.results[at];
            if let Some(name) = indexer
                && !existing.indexers.contains(&name)
            {
                existing.indexers.push(name);
            }
            // Keep the details of the best-seeded copy.
            if candidate.seeders > existing.seeders {
                existing.seeders = candidate.seeders;
                existing.peers = candidate.peers;
            }
            existing.suspicious &= candidate.suspicious;
            // A magnet link from any copy is better than following a download link.
            if existing.magnet.is_none() && address.magnet.is_some() {
                existing.link = address.link();
                existing.magnet = address.magnet;
            }
            if existing.download.is_none() {
                existing.download = address.download;
            }
            continue;
        }
        let key = address.key();
        by_identity.insert(identity, response.results.len());
        id_kinds.push(candidate.category_kind);
        response.results.push(SearchResult {
            title: clean_text(&candidate.title, MAX_TITLE_CHARS)
                .unwrap_or_else(|| "(untitled)".into()),
            size_bytes: candidate.size,
            seeders: candidate.seeders,
            peers: candidate.peers,
            link: address.link(),
            key,
            magnet: address.magnet,
            info_hash: address.info_hash,
            download: address.download,
            details_url: candidate.details.as_deref().and_then(web_url),
            indexers: indexer.into_iter().collect(),
            category,
            published: candidate
                .published
                .as_deref()
                .and_then(|p| clean_text(p, MAX_DATE_CHARS)),
            license: candidate.license.as_deref().and_then(clean_label),
            health: candidate
                .health
                .as_deref()
                .filter(|h| matches!(*h, "good" | "low" | "dead" | "unknown"))
                .map(str::to_owned),
            suspicious: candidate.suspicious,
            quality: Quality::default(),
            watch_score: 0,
            reason: String::new(),
            best_pick: false,
        });
    }
    // Scored after merging, because a duplicate can raise the seeder count.
    for (r, id_kind) in response.results.iter_mut().zip(id_kinds) {
        r.quality = Quality::parse(&r.title, r.category.as_deref(), r.size_bytes);
        if r.quality.kind == ContentKind::Unknown
            && let Some(kind) = id_kind
        {
            r.quality.kind = kind;
        }
        // Scores are small (well under ±1000 points), so tenths fit an i32.
        #[allow(clippy::cast_possible_truncation)]
        let score = (watch_score(r.seeders, r.peers, &r.quality) * 10.0).round() as i32;
        r.watch_score = score;
        r.reason = reason(r.seeders, &r.quality);
    }
    response.results.sort_by(|a, b| {
        // Most seeders first; unknown seeders last.
        b.seeders
            .map_or(-1, i64::from)
            .cmp(&a.seeders.map_or(-1, i64::from))
            .then_with(|| b.peers.cmp(&a.peers))
            .then_with(|| a.title.cmp(&b.title))
    });
    response
}

/// Validates per-indexer outcomes: ids of the form the API uses, names and errors as bounded
/// plain text.
pub(crate) fn clean_statuses(indexers: Vec<IndexerStatus>) -> Vec<IndexerStatus> {
    indexers
        .into_iter()
        .map(|i| IndexerStatus {
            id: i.id.filter(|id| valid_indexer_id(id)),
            name: clean_label(&i.name).unwrap_or_else(|| "unknown".into()),
            results: i.results,
            error: i.error.and_then(|e| clean_text(&e, MAX_ERROR_CHARS)),
        })
        .collect()
}

/// Whether a category is adult content (by name, or by a Torznab id in 6000–6999).
#[must_use]
pub fn is_adult(category: &str) -> bool {
    let lower = category.to_ascii_lowercase();
    lower.contains("xxx")
        || lower.contains("adult")
        || category
            .trim()
            .parse::<u32>()
            .is_ok_and(|id| (6000..=6999).contains(&id))
}

/// Whether an indexer id has the form the emberglow API accepts.
pub(crate) fn valid_indexer_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// How a result can be added: by info hash (with a magnet link, built from the hash if need
/// be), or by download link alone.
struct Address {
    info_hash: Option<String>,
    magnet: Option<String>,
    download: Option<String>,
}

impl Address {
    /// What copies of one torrent share: the info hash, or the download link without it.
    fn identity(&self) -> String {
        match (&self.info_hash, &self.download) {
            (Some(hash), _) => hash.clone(),
            (None, Some(download)) => format!("download:{download}"),
            (None, None) => String::new(),
        }
    }

    /// The key of a result whose first copy this is: the download link if it has one, so
    /// that the key does not change when the magnet link arrives later.
    fn key(&self) -> String {
        match (&self.download, &self.info_hash) {
            (Some(download), _) => format!("download:{download}"),
            (None, Some(hash)) => hash.clone(),
            (None, None) => String::new(),
        }
    }

    fn link(&self) -> String {
        self.magnet
            .clone()
            .or_else(|| self.download.clone())
            .unwrap_or_default()
    }
}

/// How a candidate can be added, if it can be added at all.
fn addressable(candidate: &Candidate) -> Option<Address> {
    let download = candidate.download.as_deref().and_then(web_url);
    let from_field = candidate.infohash.as_deref().and_then(normalize_hex_hash);
    let magnet = candidate.magnet.as_deref().filter(|m| valid_magnet(m));
    let from_magnet = magnet.and_then(magnet_info_hash);
    let Some(info_hash) = from_field.or(from_magnet) else {
        return download.map(|download| Address {
            info_hash: None,
            magnet: None,
            download: Some(download),
        });
    };
    let magnet = match magnet {
        Some(m) => m.to_owned(),
        None => build_magnet(&info_hash, &candidate.title),
    };
    Some(Address {
        info_hash: Some(info_hash),
        magnet: Some(magnet),
        download,
    })
}

fn valid_magnet(magnet: &str) -> bool {
    magnet.len() <= MAX_MAGNET_BYTES
        && magnet
            .get(..MAGNET_PREFIX.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(MAGNET_PREFIX))
        && !magnet.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// The info hash inside a magnet link's `btih` value (40 hex or 32 base32 characters).
fn magnet_info_hash(magnet: &str) -> Option<String> {
    let value = magnet.get(MAGNET_PREFIX.len()..)?.split('&').next()?;
    normalize_hex_hash(value).or_else(|| base32_to_hex(value))
}

fn normalize_hex_hash(hash: &str) -> Option<String> {
    let hash = hash.trim();
    (hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
}

/// Decodes a 32-character RFC 4648 base32 info hash to lowercase hex.
fn base32_to_hex(value: &str) -> Option<String> {
    if value.len() != 32 {
        return None;
    }
    let mut bits: u64 = 0;
    let mut nbits = 0;
    let mut bytes = Vec::with_capacity(20);
    for c in value.bytes() {
        let v = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u64::from(v);
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            // Truncation to the low byte is the point.
            #[allow(clippy::cast_possible_truncation)]
            bytes.push((bits >> nbits) as u8);
        }
    }
    (bytes.len() == 20).then(|| to_hex(&bytes))
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        hex.push(char::from(DIGITS[usize::from(b >> 4)]));
        hex.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    hex
}

fn build_magnet(info_hash: &str, title: &str) -> String {
    let name: String = url::form_urlencoded::byte_serialize(
        clean_text(title, MAX_TITLE_CHARS)
            .unwrap_or_default()
            .as_bytes(),
    )
    .collect();
    if name.is_empty() {
        format!("{MAGNET_PREFIX}{info_hash}")
    } else {
        format!("{MAGNET_PREFIX}{info_hash}&dn={name}")
    }
}

/// Keeps http(s) URLs only (no `javascript:`, `file:`, or `data:` links).
fn web_url(raw: &str) -> Option<String> {
    if raw.len() > MAX_URL_BYTES {
        return None;
    }
    let url = url::Url::parse(raw.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then(|| url.into())
}

/// Removes control characters, trims, and truncates to `max` characters. `None` if empty.
fn clean_text(text: &str, max: usize) -> Option<String> {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned: String = cleaned.trim().chars().take(max).collect();
    let cleaned = cleaned.trim_end().to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn clean_label(text: &str) -> Option<String> {
    clean_text(text, MAX_LABEL_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "3f9aac158c7de8dfcab171ea58a17aabdf7fbc93";

    fn candidate(title: &str, hash: Option<&str>, seeders: Option<u32>) -> Candidate {
        Candidate {
            title: title.into(),
            indexer: Some("ExampleTracker".into()),
            category: Some("PC/ISO".into()),
            adult: false,
            category_kind: None,
            size: Some(6_114_656_256),
            seeders,
            peers: seeders.map(|s| s + 10),
            published: Some("2026-08-29T00:00:00Z".into()),
            infohash: hash.map(Into::into),
            magnet: hash.map(|h| format!("magnet:?xt=urn:btih:{h}&dn=x")),
            download: None,
            details: Some("https://tracker.example/torrent/1".into()),
            license: None,
            health: None,
            suspicious: false,
        }
    }

    #[test]
    fn excludes_adult_categories_by_default() {
        let mut adult = candidate("adult", Some(HASH), Some(5));
        adult.category = Some("XXX".into());
        let mut numbered = candidate("numbered", Some(&"a".repeat(40)), Some(5));
        numbered.category = Some("6010".into());
        // Labelled harmlessly, but filed under an adult category id.
        let mut by_id = candidate("by id", Some(&"b".repeat(40)), Some(5));
        by_id.category = Some("Other".into());
        by_id.adult = true;
        let r = build_response(vec![adult.clone(), numbered, by_id], Vec::new(), false);
        assert!(r.results.is_empty());
        assert_eq!(r.excluded_adult, 3);
        let r = build_response(vec![adult], Vec::new(), true);
        assert_eq!(r.results.len(), 1);
        assert!(is_adult("Adult/Video") && !is_adult("Movies/HD") && !is_adult("5000"));
    }

    #[test]
    fn merges_duplicates_and_sorts_by_seeders() {
        let mut first = candidate("Ubuntu", Some(HASH), Some(10));
        first.indexer = Some("A".into());
        let mut second = candidate("Ubuntu again", Some(&HASH.to_uppercase()), Some(400));
        second.indexer = Some("B".into());
        let other = candidate("Other", Some(&"b".repeat(40)), Some(50));
        let unknown = candidate("Unknown seeders", Some(&"c".repeat(40)), None);
        let r = build_response(vec![first, second, other, unknown], Vec::new(), false);
        assert_eq!(r.results.len(), 3, "the two copies of HASH are merged");
        assert_eq!(r.results[0].key, HASH);
        assert_eq!(r.results[0].info_hash.as_deref(), Some(HASH));
        assert_eq!(r.results[0].seeders, Some(400), "best-seeded copy wins");
        assert_eq!(r.results[0].indexers, ["A", "B"]);
        assert_eq!(r.results[1].title, "Other");
        assert_eq!(r.results[2].seeders, None, "unknown seeders sort last");
    }

    #[test]
    fn builds_magnets_from_info_hashes_and_drops_unaddable_results() {
        let mut no_magnet = candidate("Sintel 2010", Some(HASH), Some(3));
        no_magnet.magnet = None;
        let mut neither = candidate("nothing", None, Some(9));
        neither.magnet = None;
        let mut bad_magnet = candidate("bad", None, Some(9));
        bad_magnet.magnet = Some("javascript:alert(1)".into());
        let r = build_response(vec![no_magnet, neither, bad_magnet], Vec::new(), false);
        assert_eq!(r.dropped, 2);
        assert_eq!(r.results.len(), 1);
        let built = format!("magnet:?xt=urn:btih:{HASH}&dn=Sintel+2010");
        assert_eq!(r.results[0].magnet.as_deref(), Some(built.as_str()));
        assert_eq!(
            r.results[0].link, built,
            "the magnet link is what gets added"
        );
    }

    #[test]
    fn category_ids_decide_when_the_title_and_label_do_not() {
        let mut audio = candidate("Feynman Lectures on Physics", Some(HASH), Some(11));
        audio.category = Some("Other".into());
        audio.category_kind = Some(ContentKind::Audio);
        let mut video = candidate("MIT 6.006 lectures", Some(&"b".repeat(40)), Some(5));
        video.category = Some("Other".into());
        video.category_kind = Some(ContentKind::Video);
        let mut labelled = candidate("Lecture 1 720p x264", Some(&"c".repeat(40)), Some(5));
        labelled.category_kind = Some(ContentKind::NotVideo);
        let r = build_response(vec![audio, video, labelled], Vec::new(), false);
        let kind = |hash: &str| {
            r.results
                .iter()
                .find(|x| x.key == hash)
                .map(|x| x.quality.kind)
        };
        assert_eq!(kind(HASH), Some(ContentKind::Audio));
        assert_eq!(kind(&"b".repeat(40)), Some(ContentKind::Video));
        assert_eq!(
            kind(&"c".repeat(40)),
            Some(ContentKind::Video),
            "the title says video"
        );
    }

    #[test]
    fn keeps_results_offered_only_as_a_download_link() {
        let link = "https://search.example/download/WyJ0b2tlbiJd";
        let download = |title: &str, link: &str| Candidate {
            download: Some(link.into()),
            ..candidate(title, None, Some(4))
        };
        let mut no_magnet = download("Lecture 1 720p mp4", link);
        no_magnet.magnet = None;
        let mut again = download("Lecture 1 720p mp4", link);
        again.indexer = Some("Other".into());
        let mut unsafe_link = download("Lecture 2", "javascript:alert(1)");
        unsafe_link.magnet = None;
        let mut with_hash = download("Lecture 3", "https://search.example/download/x");
        with_hash.infohash = Some(HASH.into());
        let r = build_response(
            vec![no_magnet, again, unsafe_link, with_hash],
            Vec::new(),
            false,
        );
        assert_eq!(r.dropped, 1, "only http(s) download links count");
        assert_eq!(r.results.len(), 2);
        let only = r.results.iter().find(|x| x.info_hash.is_none()).unwrap();
        assert_eq!(only.key, format!("download:{link}"));
        assert_eq!(only.link, link);
        assert_eq!(only.magnet, None);
        assert_eq!(only.indexers, ["ExampleTracker", "Other"], "merged by link");
        // With an info hash the magnet link is what gets added; the download link is kept
        // (and, being on the first copy, gives the key).
        let hashed = r.results.iter().find(|x| x.info_hash.is_some()).unwrap();
        assert_eq!(hashed.key, "download:https://search.example/download/x");
        assert!(hashed.link.starts_with("magnet:"));
        assert_eq!(
            hashed.download.as_deref(),
            Some("https://search.example/download/x")
        );
    }

    #[test]
    fn copies_merge_by_info_hash_and_keep_the_first_copys_key() {
        let link = "https://search.example/download/a";
        // First seen as a download link whose magnet arrived later, then from another indexer.
        let resolved = Candidate {
            download: Some(link.into()),
            ..candidate("Lecture 1 720p mp4", Some(HASH), Some(4))
        };
        let mut other = candidate("Lecture 1 720p mp4", Some(HASH), Some(40));
        other.indexer = Some("Other".into());
        let r = build_response(vec![resolved.clone(), other.clone()], Vec::new(), false);
        assert_eq!(r.results.len(), 1);
        assert_eq!(r.results[0].key, format!("download:{link}"));
        assert_eq!(r.results[0].seeders, Some(40));
        assert!(r.results[0].link.starts_with("magnet:"));
        // The other way round, the magnet copy's key stays.
        let r = build_response(vec![other, resolved], Vec::new(), false);
        assert_eq!(r.results.len(), 1);
        assert_eq!(r.results[0].key, HASH);
        assert_eq!(r.results[0].download.as_deref(), Some(link));
    }

    #[test]
    fn reads_base32_magnets() {
        let mut c = candidate("b32", None, Some(1));
        // Base32 of 20 zero bytes.
        c.magnet = Some(format!("magnet:?xt=urn:btih:{}", "A".repeat(32)));
        let r = build_response(vec![c], Vec::new(), false);
        assert_eq!(r.results[0].key, "0".repeat(40));
    }

    #[test]
    fn sanitizes_untrusted_text_and_links() {
        let mut c = candidate(
            &format!("bad\u{0}title\n{}", "x".repeat(400)),
            Some(HASH),
            Some(1),
        );
        c.details = Some("javascript:alert(1)".into());
        c.category = Some("  Movies\u{7}  ".into());
        let r = build_response(vec![c], Vec::new(), false);
        let result = &r.results[0];
        assert!(!result.title.chars().any(char::is_control));
        assert_eq!(result.title.chars().count(), MAX_TITLE_CHARS);
        assert!(result.title.starts_with("bad title"));
        assert_eq!(
            result.details_url, None,
            "only http(s) details links are kept"
        );
        assert_eq!(result.category.as_deref(), Some("Movies"));
        assert_eq!(result.license, None);
    }

    #[test]
    fn rejects_malformed_magnets_and_hashes() {
        for bad in [
            "magnet:?xt=urn:sha1:abc",
            "magnet:?xt=urn:btih:zz",
            "magnet:?xt=urn:btih:3f9aac158c7de8dfcab171ea58a17aabdf7fbc93 extra",
            "http://example.org/a.torrent",
        ] {
            let mut c = candidate("x", None, Some(1));
            c.magnet = Some(bad.into());
            assert_eq!(
                build_response(vec![c], Vec::new(), false).dropped,
                1,
                "{bad}"
            );
        }
        assert_eq!(normalize_hex_hash("g".repeat(40).as_str()), None);
        assert_eq!(base32_to_hex(&"1".repeat(32)), None);
    }

    #[test]
    fn indexer_errors_are_bounded() {
        let r = build_response(
            Vec::new(),
            vec![IndexerStatus {
                id: Some("../evil".into()),
                name: "\u{1b}[31mEvil".into(),
                results: 0,
                error: Some("e".repeat(1000)),
            }],
            false,
        );
        assert_eq!(r.indexers[0].name, "[31mEvil");
        assert_eq!(r.indexers[0].id, None, "ids outside [a-z0-9_-] are dropped");
        assert_eq!(
            r.indexers[0].error.as_ref().map(String::len),
            Some(MAX_ERROR_CHARS)
        );
    }
}
