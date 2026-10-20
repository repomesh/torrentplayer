// SPDX-License-Identifier: MIT OR Apache-2.0

//! Preloading the best search result, so that playing it starts at once.
//!
//! When a search's best pick has settled (see `lumen_search::settle`: it stayed best for 2 s,
//! or the search finished) and [`PreloadPolicy::preload_top_result`] allows it, the engine
//! downloads the start and end of that torrent's best video in the background (see
//! `lumen_torrent::preload` for the limits: one at a time, at most about 100 MB, never
//! uploaded). A search starts at most two preloads: the settled pick, and the final one if a
//! late indexer changed it. A search that finishes without a best pick, or with preloading
//! turned off, evicts the previous preload. Adding the same magnet link later reuses the
//! preloaded torrent. Nothing is preloaded while a video is playing, so a search never slows
//! down playback.
//!
//! The UI polls `preload_status` to show "Preloading… 40%" and "Ready to play instantly".
//!
//! The play queue shares the one preload slot: near the end of a video it preloads the next
//! queued video ([`PreloadState::preload_for_queue`]). While a video plays, the queue's preload
//! beats a search's best pick: a search neither replaces nor evicts it. With nothing playing,
//! a search's best pick replaces it as usual.

use std::sync::{Arc, Mutex, PoisonError};

use lumen_search::SearchResponse;
use lumen_torrent::{PreloadPhase, PreloadStatus, TorrentSource, magnet_info_hash};
use serde::Serialize;
use tauri::State;
use ts_rs::TS;

use crate::core::{AppState, Core};
use crate::settings::SettingsState;

/// Decides whether a finished search preloads its best pick.
///
/// This is the hook for the settings page: [`SettingsState`] implements it from the
/// `search.preloadTopResult` setting (default on). Other policies (for example, "not on
/// metered connections") can be added by implementing this trait.
pub(crate) trait PreloadPolicy: Send + Sync {
    /// Whether to preload the best result of each search.
    fn preload_top_result(&self) -> bool;
}

/// Preloading on: the documented default of `preload_top_result`.
#[derive(Debug, Clone, Copy, Default)]
#[allow(
    dead_code,
    reason = "the default policy, for tests and builds without settings"
)]
pub(crate) struct DefaultPreloadPolicy;

impl PreloadPolicy for DefaultPreloadPolicy {
    fn preload_top_result(&self) -> bool {
        true
    }
}

impl PreloadPolicy for SettingsState {
    fn preload_top_result(&self) -> bool {
        self.current().search.preload_top_result
    }
}

/// Managed state: the streaming core, once a preload has started it.
#[derive(Default)]
pub(crate) struct PreloadState {
    core: Mutex<Option<Arc<Core>>>,
    /// Info hash of the last preload the queue started (the next video to play).
    queue_hash: Mutex<Option<String>>,
}

impl std::fmt::Debug for PreloadState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreloadState").finish_non_exhaustive()
    }
}

/// What to do after a search.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    /// Preload this link (a magnet link, or a download link).
    Preload(String),
    /// Evict the previous preload.
    Evict,
}

/// Preload the best pick (its link) if allowed and nothing is playing; otherwise evict.
fn decide(enabled: bool, playing: bool, best: Option<&str>) -> Decision {
    match best {
        Some(magnet) if enabled && !playing => Decision::Preload(magnet.to_owned()),
        _ => Decision::Evict,
    }
}

impl PreloadState {
    fn started_core(&self) -> Option<Arc<Core>> {
        self.core
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Starts preloading `response`'s best pick in the background, or evicts the previous
    /// preload. Returns once the engine has started.
    pub(crate) async fn after_search(
        &self,
        app: &AppState,
        policy: &dyn PreloadPolicy,
        response: &SearchResponse,
    ) {
        let best = response.results.iter().find(|r| r.best_pick);
        self.apply(app, policy, best.map(|r| r.link.as_str())).await;
    }

    /// Starts preloading `best` (a best pick's link) in the background, or evicts the
    /// previous preload if there is none. Returns once the engine has started.
    pub(crate) async fn apply(
        &self,
        app: &AppState,
        policy: &dyn PreloadPolicy,
        best: Option<&str>,
    ) {
        let enabled = policy.preload_top_result();
        // Only start the engine when there is something to preload.
        let core = if decide(enabled, false, best) == Decision::Evict {
            self.started_core()
        } else {
            match app.core().await {
                Ok(core) => {
                    *self.core.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(Arc::clone(core));
                    Some(Arc::clone(core))
                }
                Err(error) => {
                    tracing::warn!(kind = ?error.kind, "cannot preload: the engine did not start");
                    return;
                }
            }
        };
        let Some(core) = core else {
            return;
        };
        let playing = !core
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty();
        if playing && self.queue_owns_slot(&core) {
            // The next queued video wins while one plays (see the module docs).
            return;
        }
        match decide(enabled, playing, best) {
            Decision::Preload(magnet) => {
                let Ok(source) = TorrentSource::parse_link(&magnet) else {
                    return;
                };
                tauri::async_runtime::spawn(async move {
                    core.engine.preload(source).await;
                });
            }
            Decision::Evict => {
                tauri::async_runtime::spawn(async move {
                    core.engine.cancel_preload().await;
                });
            }
        }
    }

    /// Preloads the next queued video in the background: `file` of `source` (or the file a
    /// preload would choose), replacing a search's preload. `info_hash` is the item's, if the
    /// queue knows it (a magnet link's own is used otherwise). The caller makes sure the video
    /// playing can no longer be slowed down by it.
    pub(crate) fn preload_for_queue(
        &self,
        core: &Arc<Core>,
        source: TorrentSource,
        file: Option<usize>,
        info_hash: Option<String>,
    ) {
        *self.core.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(core));
        let hash = info_hash
            .filter(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(|h| h.to_ascii_lowercase())
            .or_else(|| match &source {
                TorrentSource::Magnet(link) => magnet_info_hash(link),
                TorrentSource::Url(_) | TorrentSource::File(_) => None,
            });
        *self
            .queue_hash
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = hash;
        let core = Arc::clone(core);
        tauri::async_runtime::spawn(async move {
            core.engine.preload_file(source, file).await;
        });
    }

    /// Whether the slot holds the queue's preload, still working or ready.
    fn queue_owns_slot(&self, core: &Core) -> bool {
        let queued = self
            .queue_hash
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let current = core.engine.preload_status();
        queue_owns(queued.as_deref(), current.as_ref())
    }
}

/// Whether `current` is the queue's preload (`queued`), and not failed or replaced.
fn queue_owns(queued: Option<&str>, current: Option<&PreloadStatus>) -> bool {
    match (queued, current) {
        (Some(hash), Some(status)) => {
            status.info_hash == hash
                && !matches!(status.phase, PreloadPhase::Failed | PreloadPhase::Cancelled)
        }
        _ => false,
    }
}

/// What the current preload is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum PreloadPhaseDto {
    /// Fetching the torrent's details.
    Resolving,
    /// Downloading the start of the video.
    Buffering,
    /// Done: playing it starts at once.
    Ready,
    /// Nothing could be preloaded.
    Failed,
    /// Replaced by a newer preload.
    Cancelled,
}

/// The current preload.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct PreloadStatusDto {
    /// Lowercase hex info hash (matches `SearchResultDto.infoHash`).
    pub info_hash: String,
    /// Phase.
    pub phase: PreloadPhaseDto,
    /// Whether the user has added this torrent.
    pub claimed: bool,
    /// Index of the preloaded file, once chosen.
    pub file_index: Option<usize>,
    /// Its path in the torrent (plain text; never render as HTML).
    pub file_name: Option<String>,
    /// Bytes preloaded so far.
    pub buffered_bytes: u64,
    /// Bytes to preload.
    pub target_bytes: u64,
    /// `bufferedBytes / targetBytes`, 0–1.
    pub progress: f64,
    /// Why it failed, if it did.
    pub error: Option<String>,
}

impl From<PreloadStatus> for PreloadStatusDto {
    fn from(s: PreloadStatus) -> Self {
        Self {
            progress: s.fraction(),
            phase: match s.phase {
                PreloadPhase::Resolving => PreloadPhaseDto::Resolving,
                PreloadPhase::Buffering => PreloadPhaseDto::Buffering,
                PreloadPhase::Ready => PreloadPhaseDto::Ready,
                PreloadPhase::Failed => PreloadPhaseDto::Failed,
                PreloadPhase::Cancelled => PreloadPhaseDto::Cancelled,
            },
            info_hash: s.info_hash,
            claimed: s.claimed,
            file_index: s.file,
            file_name: s.file_name,
            buffered_bytes: s.buffered_bytes,
            target_bytes: s.target_bytes,
            error: s.error,
        }
    }
}

/// The current preload, or `null` if there is none. Never starts the engine.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri passes managed state by value"
)]
pub(crate) fn preload_status(state: State<'_, PreloadState>) -> Option<PreloadStatusDto> {
    let core = state.started_core()?;
    core.engine.preload_status().map(Into::into)
}

#[cfg(test)]
mod tests {
    use lumen_search::rank::rank;
    use lumen_search::results::{Candidate, build_response};

    use super::*;

    fn best(titles: &[(&str, u32)]) -> Option<String> {
        let candidates = titles
            .iter()
            .enumerate()
            .map(|(i, (title, seeders))| Candidate {
                title: (*title).into(),
                seeders: Some(*seeders),
                infohash: Some(format!("{i:02x}").repeat(20)),
                ..Candidate::default()
            })
            .collect();
        let mut r = build_response(candidates, Vec::new(), false);
        rank(&mut r.results, "sintel");
        r.results.into_iter().find(|r| r.best_pick).map(|r| r.link)
    }

    #[test]
    fn preloads_the_best_pick_only_when_allowed_and_idle() {
        let r = best(&[("Sintel 720p mp4", 40), ("Other film", 900)]);
        let Decision::Preload(magnet) = decide(true, false, r.as_deref()) else {
            panic!("expected a preload");
        };
        assert!(magnet.starts_with(&format!("magnet:?xt=urn:btih:{}", "00".repeat(20))));
        assert_eq!(
            decide(false, false, r.as_deref()),
            Decision::Evict,
            "setting off"
        );
        assert_eq!(
            decide(true, true, r.as_deref()),
            Decision::Evict,
            "something is playing"
        );
        let none = best(&[("Other film", 900)]);
        assert_eq!(
            decide(true, false, none.as_deref()),
            Decision::Evict,
            "no best pick"
        );
    }

    #[test]
    fn the_queue_keeps_its_preload_while_it_works() {
        let hash = "ab".repeat(20);
        let mut status = PreloadStatus::new(hash.clone());
        assert!(queue_owns(Some(&hash), Some(&status)), "resolving");
        status.phase = PreloadPhase::Ready;
        assert!(queue_owns(Some(&hash), Some(&status)), "ready");
        status.phase = PreloadPhase::Failed;
        assert!(
            !queue_owns(Some(&hash), Some(&status)),
            "failed: a search may use the slot"
        );
        status.phase = PreloadPhase::Buffering;
        assert!(
            !queue_owns(Some(&"cd".repeat(20)), Some(&status)),
            "another torrent"
        );
        assert!(
            !queue_owns(None, Some(&status)),
            "the queue preloaded nothing"
        );
        assert!(!queue_owns(Some(&hash), None), "the slot is empty");
    }

    #[test]
    fn default_policy_preloads() {
        assert!(DefaultPreloadPolicy.preload_top_result());
    }

    #[test]
    fn status_dto_reports_progress() {
        let mut s = PreloadStatus::new("ab".repeat(20));
        s.phase = PreloadPhase::Buffering;
        s.buffered_bytes = 10;
        s.target_bytes = 40;
        let json = serde_json::to_value(PreloadStatusDto::from(s)).unwrap();
        assert_eq!(json["phase"], "buffering");
        assert_eq!(json["progress"], 0.25);
        assert_eq!(json["infoHash"], "ab".repeat(20));
    }
}
