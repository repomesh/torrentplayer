// SPDX-License-Identifier: MIT OR Apache-2.0

//! Measures a live progressive search against search.emberglow.tech: time to the first
//! results, to the first best pick, to when the best pick would be preloaded (see
//! `torrentplayer_search::settle`), and to the end; how often the top result changed on the way; the
//! requests sent; and a repeat served from the local cache.
//!
//! `--stream` (the default) reads the event stream: one request, plus the indexer list once.
//! `--fan-out` asks the indexers in groups, as TorrentPlayer does when the stream is unavailable:
//! about ten requests. The service caches `/search` answers for 10 minutes but never caches
//! streams, so compare the two on queries nobody searched in the last 10 minutes. Run it
//! sparingly (the service allows 60 requests per minute) and never in CI.
//!
//! ```text
//! cargo run -p torrentplayer-search --example search_timing -- --fan-out mit 6.006 lectures
//! ```

// A command-line tool: printing is its output.
#![allow(clippy::print_stdout)]

use tokio::time::Instant;
use torrentplayer_search::{
    EmberglowProvider, Query, SearchProgress, SearchProvider as _, SearchResponse, Settle,
    SettleAction,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut streaming = true;
    let mut words = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--stream" => streaming = true,
            "--fan-out" => streaming = false,
            _ => words.push(arg),
        }
    }
    let query = Query::parse(&words.join(" "))?;
    let provider = EmberglowProvider::new()?.streaming(streaming);
    println!(
        "{} search for {:?}",
        if streaming { "stream" } else { "fan-out" },
        query.as_str()
    );

    let started = Instant::now();
    let mut reports: Vec<(Instant, SearchProgress)> = Vec::new();
    let response = provider
        .search_progressive(&query, &mut |p| {
            println!(
                "  {:>8.2?}  {}/{} answered, {} results",
                started.elapsed(),
                p.completed,
                p.total,
                p.response.results.len()
            );
            reports.push((Instant::now(), p));
        })
        .await?;
    let total = started.elapsed();

    let since = |at: Instant| format!("{:.2?}", at.duration_since(started));
    let first = reports
        .iter()
        .find(|(_, p)| !p.response.results.is_empty())
        .map(|(at, _)| since(*at));
    let first_pick = reports
        .iter()
        .find(|(_, p)| p.response.results.iter().any(|r| r.best_pick))
        .map(|(at, _)| since(*at));
    // How often the result on top changed after the first one appeared.
    let tops: Vec<Option<&str>> = reports
        .iter()
        .map(|(_, p)| p.response.results.first().map(|r| r.key.as_str()))
        .collect();
    let top_changes = tops
        .windows(2)
        .filter(|w| w[0].is_some() && w[0] != w[1])
        .count();
    // Replay the reports through the preload rule, including its timer between reports.
    let mut settle = Settle::default();
    let mut preloads = Vec::new();
    let mut previous: Option<&SearchProgress> = None;
    for (at, p) in &reports {
        if let (Some(prev), Some(deadline)) = (previous, settle.deadline())
            && deadline <= *at
            && let Some(action) = settle.update(prev, deadline)
        {
            preloads.push((since(deadline), action));
        }
        if let Some(action) = settle.update(p, *at) {
            preloads.push((since(*at), action));
        }
        previous = Some(p);
    }
    let failed = response
        .indexers
        .iter()
        .filter(|i| i.error.is_some())
        .count();
    println!(
        "{} reports; first results {first:?}; first best pick {first_pick:?}; done {total:.2?}; \
         {} results; top result changed {top_changes} times; {failed} of {} indexers failed \
         or skipped; {} requests",
        reports.len(),
        response.results.len(),
        response.indexers.len(),
        provider.requests_sent(),
    );
    for (at, action) in &preloads {
        match action {
            SettleAction::Preload { key, .. } => println!("  preload at {at}: {}", short(key)),
            SettleAction::Evict => println!("  evict at {at}: no best pick"),
        }
    }
    print_indexers(&response);
    for r in response.results.iter().take(8) {
        let pick = if r.best_pick { "best pick: " } else { "" };
        let size = r.size_bytes.map_or_else(
            || "size unknown".to_owned(),
            |b| format!("{} MiB", b / (1024 * 1024)),
        );
        let category = r.category.as_deref().unwrap_or("no category");
        println!("  {pick}{} [{}] ({category}, {size})", r.title, r.reason);
    }

    let again = Instant::now();
    provider.search_progressive(&query, &mut |_| {}).await?;
    println!("repeat (local cache): {:.2?}", again.elapsed());
    Ok(())
}

/// A result key fit to print: download links carry tokens.
fn short(key: &str) -> &str {
    if key.starts_with("download:") {
        "(a download link)"
    } else {
        key
    }
}

/// Which indexers found something, and why the others did not.
fn print_indexers(response: &SearchResponse) {
    let mut found: Vec<_> = response.indexers.iter().filter(|i| i.results > 0).collect();
    found.sort_by_key(|i| std::cmp::Reverse(i.results));
    let found: Vec<String> = found
        .iter()
        .map(|i| format!("{} {}", i.name, i.results))
        .collect();
    println!(
        "  found: {}; {} adult results hidden",
        found.join(", "),
        response.excluded_adult
    );
    for i in response.indexers.iter().filter(|i| i.error.is_some()) {
        println!("  {}: {}", i.name, i.error.as_deref().unwrap_or_default());
    }
}
