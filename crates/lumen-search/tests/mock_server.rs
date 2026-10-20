// SPDX-License-Identifier: MIT OR Apache-2.0

//! The emberglow provider against a local mock HTTP server. No internet access is used.

// Test code: panicking on failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lumen_search::{EmberglowProvider, Query, SearchError, SearchProgress, SearchProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves one canned response per connection; returns the address.
async fn serve(status: u16, body: Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut request = vec![0u8; 4096];
                let _ = tcp.read(&mut request).await;
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tcp.write_all(head.as_bytes()).await;
                let _ = tcp.write_all(&body).await;
            });
        }
    });
    addr
}

fn provider(addr: SocketAddr) -> EmberglowProvider {
    EmberglowProvider::with_base_url(format!("http://{addr}/").parse().unwrap()).unwrap()
}

/// A provider that asks the indexers in groups instead of reading the event stream (the
/// stream and its fallback are tested in `src/emberglow.rs`).
fn fan_out_provider(addr: SocketAddr) -> EmberglowProvider {
    provider(addr).streaming(false)
}

#[tokio::test]
async fn returns_validated_results() {
    let body = br#"{"results": [{"title": "Sintel", "tracker": "T", "category": "Movies",
        "size": 100, "seeders": 7, "peers": 9, "published": "2026-01-01T00:00:00Z",
        "infohash": "08ada5a7a6183aae1e09d831df6748d566095a10", "magnet": null,
        "details": "https://example.org/sintel"}],
        "indexers": [{"id": "t", "name": "T", "results": 1, "error": null}]}"#;
    let addr = serve(200, body.to_vec()).await;
    let r = provider(addr)
        .search(&Query::parse("sintel").unwrap())
        .await
        .unwrap();
    assert_eq!(r.results.len(), 1);
    assert_eq!(r.results[0].title, "Sintel");
    assert!(
        r.results[0]
            .link
            .starts_with("magnet:?xt=urn:btih:08ada5a7")
    );
    assert_eq!(r.indexers[0].results, 1);
}

#[tokio::test]
async fn reports_rate_limiting_and_outages() {
    let q = Query::parse("anything").unwrap();
    let r = provider(serve(429, b"{}".to_vec()).await).search(&q).await;
    assert_eq!(r, Err(SearchError::RateLimited));
    let r = provider(serve(502, b"{}".to_vec()).await).search(&q).await;
    assert!(matches!(r, Err(SearchError::Unavailable(_))), "{r:?}");
    let r = provider(serve(422, br#"{"detail": "q"}"#.to_vec()).await)
        .search(&q)
        .await;
    assert!(matches!(r, Err(SearchError::InvalidQuery(_))), "{r:?}");
    let r = provider(serve(500, b"{}".to_vec()).await).search(&q).await;
    assert!(matches!(r, Err(SearchError::Unavailable(_))), "{r:?}");
}

#[tokio::test]
async fn refuses_oversized_and_malformed_responses() {
    let q = Query::parse("anything").unwrap();
    let huge = vec![b' '; 5 * 1024 * 1024];
    let r = provider(serve(200, huge).await).search(&q).await;
    assert_eq!(r, Err(SearchError::TooLarge));
    let r = provider(serve(200, b"<html>".to_vec()).await)
        .search(&q)
        .await;
    assert_eq!(r, Err(SearchError::BadResponse));
}

#[tokio::test]
async fn unreachable_service_is_reported_not_panicked() {
    // Bind and drop to get a port with nothing listening.
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let r = provider(addr).search(&Query::parse("x").unwrap()).await;
    assert!(matches!(r, Err(SearchError::Unavailable(_))), "{r:?}");
}

/// Serves responses chosen by `route` from the request target (path and query), after the
/// delay it returns; counts requests.
async fn serve_routes(
    route: impl Fn(&str) -> (u16, Vec<u8>, u64) + Send + Sync + 'static,
) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let route = Arc::new(route);
    let counter = Arc::clone(&count);
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let route = Arc::clone(&route);
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let mut request = vec![0u8; 4096];
                let n = tcp.read(&mut request).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&request[..n]).into_owned();
                let target = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                counter.fetch_add(1, Ordering::SeqCst);
                let (status, body, delay_ms) = route(&target);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tcp.write_all(head.as_bytes()).await;
                let _ = tcp.write_all(&body).await;
            });
        }
    });
    (addr, count)
}

const INDEXERS: &str = r#"{"count": 4, "indexers": [
    {"id": "fast-a", "name": "Fast A", "status": "ok", "failures": 0, "last_seconds": 0.2},
    {"id": "fast-b", "name": "Fast B", "status": "ok", "failures": 0, "last_seconds": 0.4},
    {"id": "slow", "name": "Slow", "status": "ok", "failures": 0, "last_seconds": 9.5},
    {"id": "broken", "name": "Broken", "status": "skipped", "failures": 3, "last_seconds": null}
]}"#;

fn result_json(title: &str, hash: &str, seeders: u32, tracker: &str) -> String {
    format!(
        r#"{{"title": "{title}", "tracker": "{tracker}", "category": "Movies", "size": 2000000000,
            "seeders": {seeders}, "peers": {seeders}, "published": null, "infohash": "{hash}",
            "magnet": null, "details": null}}"#
    )
}

fn search_body(results: &[String], indexers: &[(&str, &str)]) -> Vec<u8> {
    let statuses: Vec<String> = indexers
        .iter()
        .map(|(id, name)| {
            format!(r#"{{"id": "{id}", "name": "{name}", "results": 1, "error": null}}"#)
        })
        .collect();
    format!(
        r#"{{"results": [{}], "indexers": [{}]}}"#,
        results.join(","),
        statuses.join(",")
    )
    .into_bytes()
}

/// The fast group answers at once; the slow indexer after 400 ms.
fn fan_out_routes(target: &str) -> (u16, Vec<u8>, u64) {
    if target.starts_with("/indexers") {
        return (200, INDEXERS.as_bytes().to_vec(), 0);
    }
    assert!(
        target.starts_with("/search?q=sintel+2010&limit=100"),
        "normalized query: {target}"
    );
    assert!(!target.contains("broken"), "skipped indexers are not asked");
    if target.contains("indexer=slow") {
        let r = result_json(
            "Sintel 2010 2160p WEB-DL x264 mp4",
            "1111111111111111111111111111111111111111",
            80,
            "Slow",
        );
        (200, search_body(&[r], &[("slow", "Slow")]), 400)
    } else {
        assert!(
            target.ends_with("&indexer=fast-a&indexer=fast-b"),
            "{target}"
        );
        let r = result_json(
            "Sintel 2010 720p x264",
            "2222222222222222222222222222222222222222",
            40,
            "Fast A",
        );
        (
            200,
            search_body(&[r], &[("fast-a", "Fast A"), ("fast-b", "Fast B")]),
            0,
        )
    }
}

#[tokio::test]
async fn progressive_search_shows_fast_indexers_first_and_caches_the_result() {
    let (addr, requests) = serve_routes(fan_out_routes).await;
    let p = fan_out_provider(addr);
    let q = Query::parse("  Sintel   2010 ").unwrap();
    let mut updates: Vec<SearchProgress> = Vec::new();
    let started = std::time::Instant::now();
    let mut first_at = None;
    let final_response = p
        .search_progressive(&q, &mut |u| {
            first_at.get_or_insert_with(|| started.elapsed());
            updates.push(u);
        })
        .await
        .unwrap();
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "indexer list + 2 searches"
    );
    assert_eq!(updates.len(), 2);
    assert!(
        first_at.unwrap() < Duration::from_millis(300),
        "fast group first"
    );
    assert_eq!((updates[0].completed, updates[0].total), (1, 2));
    assert!(!updates[0].done);
    assert_eq!(updates[0].response.results.len(), 1);
    assert_eq!(
        updates[0].response.results[0].title,
        "Sintel 2010 720p x264"
    );

    let last = updates.last().unwrap();
    assert!(last.done && !last.cached);
    assert_eq!(last.response, final_response);
    let titles: Vec<&str> = final_response
        .results
        .iter()
        .map(|r| r.title.as_str())
        .collect();
    assert_eq!(
        titles,
        ["Sintel 2010 2160p WEB-DL x264 mp4", "Sintel 2010 720p x264"],
        "re-ranked as results arrive"
    );
    assert!(final_response.results[0].best_pick);
    assert!(
        final_response.results[0]
            .reason
            .contains("plays in built-in player")
    );
    let broken = final_response
        .indexers
        .iter()
        .find(|i| i.id.as_deref() == Some("broken"))
        .expect("skipped indexer reported");
    assert!(broken.error.is_some());

    // The same search again (any case or spacing) comes from the local cache.
    let mut cached = Vec::new();
    let again = p
        .search_progressive(&Query::parse("sintel 2010").unwrap(), &mut |u| {
            cached.push(u);
        })
        .await
        .unwrap();
    assert_eq!(again, final_response);
    assert_eq!(cached.len(), 1);
    assert!(cached[0].cached && cached[0].done);
    assert_eq!(requests.load(Ordering::SeqCst), 3, "no new requests");
}

#[tokio::test]
async fn progressive_search_falls_back_to_one_request_without_an_indexer_list() {
    let (addr, requests) = serve_routes(|target| {
        if target.starts_with("/indexers") {
            return (404, b"{}".to_vec(), 0);
        }
        assert!(!target.contains("indexer="), "{target}");
        let r = result_json(
            "Sintel 2010 1080p",
            "3333333333333333333333333333333333333333",
            9,
            "T",
        );
        (200, search_body(&[r], &[("t", "T")]), 0)
    })
    .await;
    let p = fan_out_provider(addr);
    let mut updates = Vec::new();
    let r = p
        .search_progressive(&Query::parse("sintel").unwrap(), &mut |u| updates.push(u))
        .await
        .unwrap();
    assert_eq!(r.results.len(), 1);
    assert_eq!(updates.len(), 1);
    assert!(updates[0].done);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failed_indexer_groups_are_reported_and_not_cached() {
    let (addr, requests) = serve_routes(|target| {
        if target.starts_with("/indexers") {
            return (200, INDEXERS.as_bytes().to_vec(), 0);
        }
        if target.contains("indexer=slow") {
            return (502, b"{}".to_vec(), 0);
        }
        let r = result_json(
            "Sintel 720p",
            "4444444444444444444444444444444444444444",
            3,
            "Fast A",
        );
        (200, search_body(&[r], &[("fast-a", "Fast A")]), 0)
    })
    .await;
    let p = fan_out_provider(addr);
    let q = Query::parse("sintel").unwrap();
    let r = p.search_progressive(&q, &mut |_| {}).await.unwrap();
    assert_eq!(r.results.len(), 1);
    let slow = r
        .indexers
        .iter()
        .find(|i| i.id.as_deref() == Some("slow"))
        .unwrap();
    assert_eq!(slow.name, "Slow");
    assert!(slow.error.as_deref().unwrap().contains("unavailable"));
    let before = requests.load(Ordering::SeqCst);
    p.search_progressive(&q, &mut |_| {}).await.unwrap();
    assert!(
        requests.load(Ordering::SeqCst) > before,
        "partial results are not cached"
    );
}

#[tokio::test]
async fn every_group_failing_is_an_error() {
    let (addr, _) = serve_routes(|target| {
        if target.starts_with("/indexers") {
            (200, INDEXERS.as_bytes().to_vec(), 0)
        } else {
            (503, b"{}".to_vec(), 0)
        }
    })
    .await;
    let r = fan_out_provider(addr)
        .search_progressive(&Query::parse("sintel").unwrap(), &mut |_| {})
        .await;
    assert!(matches!(r, Err(SearchError::Unavailable(_))), "{r:?}");
}
