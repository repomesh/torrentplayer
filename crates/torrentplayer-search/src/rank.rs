// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ranking: relevance first, then how watchable each result is.
//!
//! Results are ordered by
//!
//! 1. whether the title contains every query word,
//! 2. how many query words it contains,
//! 3. the watch score (below), then seeders and peers.
//!
//! Relevance always wins: a perfect 4K release of the wrong lecture never outranks the right
//! one. Within a relevance group, the watch score prefers sources that start fast and look
//! good in the built-in player.
//!
//! # Watch score
//!
//! In points, with `s` seeders and `l` leechers:
//!
//! | Part | Points |
//! |------|--------|
//! | Availability | `8 · ln(1 + s)`: 1 seeder 5.5, 10 → 19.2, 100 → 36.9, 1000 → 55.3 |
//! | Swarm health | `4 · s / (s + l)`: seeders' share of the swarm |
//! | Dead, fragile, or unknown | −40 with no seeders, −12 with 1–2 (one leaving stalls playback), −15 if the indexer does not say |
//! | Resolution | 2160p 28 (20 with under 10 seeders: too few sources for 4K bitrates), 1080p 26, 720p 16, SD 6, unknown 12 |
//! | Source | Blu-ray 6, WEB-DL 6, WEB rip 4, HDTV 2, DVD 1, unknown 0, CAM/telesync/screener −60 |
//! | HDR | +2 |
//! | Built-in player | plays (MP4/WebM) +10, likely (MKV, web codec) +3, unknown 0, HEVC outside MP4 or Dolby/DTS audio the app cannot convert −3, needs the full player (AVI, TS, Xvid) −10 |
//! | Audio named | AAC, Opus, MP3, FLAC +2; only Dolby (DD, DD+, `TrueHD`, Atmos) or DTS: 0 when the app converts it to stereo (it has the bundled FFmpeg, see [`Quality::converts_audio`]), otherwise −10 (the built-in player plays the picture without sound) |
//! | Fake 4K | −20, and the 4K claim earns no resolution points (see [`MIN_UHD_BYTES`]) |
//! | Not a video | −25 (audio, books, software) |
//!
//! So a 1080p WEB-DL with 50 seeders (about 71) and a 720p release with 400 (about 72) tie,
//! while any camera recording sinks below every honest release with a few seeders, and a
//! single-seeder Blu-ray rip loses to a plain 1080p release with five.
//!
//! [`MIN_UHD_BYTES`]: crate::quality::MIN_UHD_BYTES

use std::cmp::Reverse;

use crate::quality::{ContentKind, Playability, Quality, Resolution, Source};
use crate::results::SearchResult;

/// Watch score in points (see the module documentation).
#[must_use]
pub fn watch_score(seeders: Option<u32>, peers: Option<u32>, quality: &Quality) -> f64 {
    let s = f64::from(seeders.unwrap_or(0));
    let leechers = match (seeders, peers) {
        // Indexers disagree on whether `peers` includes seeders.
        (Some(s), Some(p)) if p >= s => f64::from(p - s),
        (_, Some(p)) => f64::from(p),
        (_, None) => 0.0,
    };
    let mut score = 8.0 * s.ln_1p();
    if s > 0.0 {
        score += 4.0 * s / (s + leechers);
    }
    score -= match seeders {
        Some(0) => 40.0,
        Some(1 | 2) => 12.0,
        None => 15.0,
        Some(_) => 0.0,
    };
    score += match quality.resolution {
        _ if quality.suspect_size => -20.0,
        Some(Resolution::Uhd2160) if s < 10.0 => 20.0,
        Some(Resolution::Uhd2160) => 28.0,
        Some(Resolution::Hd1080) => 26.0,
        Some(Resolution::Hd720) => 16.0,
        Some(Resolution::Sd) => 6.0,
        None => 12.0,
    };
    score += match quality.source {
        Some(Source::BluRay | Source::WebDl) => 6.0,
        Some(Source::WebRip) => 4.0,
        Some(Source::Hdtv) => 2.0,
        Some(Source::Dvd) => 1.0,
        Some(Source::Cam) => -60.0,
        None => 0.0,
    };
    if quality.hdr {
        score += 2.0;
    }
    score += match quality.playability() {
        Playability::Plays => 10.0,
        Playability::Likely => 3.0,
        Playability::Unknown => 0.0,
        Playability::Unlikely => -3.0,
        Playability::NeedsFullPlayer => -10.0,
    };
    score += match quality.audio {
        Some(audio) if audio.plays_in_webview() => 2.0,
        Some(_) if quality.converts_audio => 0.0,
        Some(_) => -10.0,
        None => 0.0,
    };
    if quality.kind == ContentKind::NotVideo {
        score -= 25.0;
    }
    score
}

/// A short explanation of a result's quality, for example
/// `1080p WEB-DL · 412 seeders · plays in built-in player`.
#[must_use]
pub fn reason(seeders: Option<u32>, quality: &Quality) -> String {
    let mut parts: Vec<String> = Vec::new();
    if quality.is_cam() {
        parts.push("camera copy (low quality)".into());
    } else {
        let label = quality.label();
        if !label.is_empty() {
            parts.push(label);
        }
    }
    parts.push(match seeders {
        None => "seeders unknown".into(),
        Some(0) => "no seeders".into(),
        Some(1) => "1 seeder".into(),
        Some(n) => format!("{n} seeders"),
    });
    let playability = quality.playability();
    match (playability, quality.silent_audio()) {
        (Playability::NeedsFullPlayer, _) => parts.push("needs the full player".into()),
        (_, Some(audio)) => parts.push(format!(
            "{} audio: no sound in built-in player",
            audio.label()
        )),
        (Playability::Plays, None) => parts.push("plays in built-in player".into()),
        (Playability::Unlikely, None) => parts.push("may need the full player".into()),
        (Playability::Likely | Playability::Unknown, None) => {}
    }
    if playability != Playability::NeedsFullPlayer
        && let Some(audio) = quality.converted_audio()
    {
        parts.push(format!("{} audio: converted to stereo", audio.label()));
    }
    if quality.suspect_size {
        parts.push("too small to be real 4K".into());
    }
    match quality.kind {
        ContentKind::NotVideo => parts.push("nothing to play".into()),
        ContentKind::Audio => parts.push("audio".into()),
        ContentKind::Video | ContentKind::Unknown => {}
    }
    parts.join(" · ")
}

/// Smallest torrent with no sign of being a video (no video words or category) that may still
/// be the best pick. Smaller ones are nearly always books, documents, or audio, while lecture
/// videos and packs whose titles name no format are larger.
pub const MIN_UNMARKED_VIDEO_BYTES: u64 = 100 * 1024 * 1024;

/// Whether a result may be recommended (and preloaded) as the best pick: it has seeders, is
/// not a camera copy or fake 4K, is a video or audio (or, with nothing to go by, at least
/// [`MIN_UNMARKED_VIDEO_BYTES`]), and should play in the built-in player.
#[must_use]
pub fn pickable(result: &SearchResult) -> bool {
    let q = &result.quality;
    let video = match q.kind {
        ContentKind::Video | ContentKind::Audio => true,
        ContentKind::NotVideo => false,
        ContentKind::Unknown => result
            .size_bytes
            .is_none_or(|size| size >= MIN_UNMARKED_VIDEO_BYTES),
    };
    result.seeders.is_some_and(|s| s > 0)
        && !q.is_cam()
        && !q.suspect_size
        && video
        && q.playability() != Playability::NeedsFullPlayer
}

/// Sorts results by relevance to `query`, then watch score, and marks the best pick.
///
/// The best pick is the highest-ranked [`pickable`] result among those containing every query
/// word; it is moved to the front. There is none if no such result exists.
pub fn rank(results: &mut [SearchResult], query: &str) {
    let wanted = tokens(query);
    let relevance = |title: &str| {
        if wanted.is_empty() {
            return (true, 0);
        }
        let have = tokens(title);
        let matched = wanted.iter().filter(|w| have.contains(w)).count();
        (matched == wanted.len(), matched)
    };
    results.sort_by_cached_key(|r| {
        let (all, matched) = relevance(&r.title);
        (
            Reverse(all),
            Reverse(matched),
            Reverse(r.watch_score),
            Reverse(r.seeders.map_or(-1, i64::from)),
            Reverse(r.peers.map_or(-1, i64::from)),
        )
    });
    for r in results.iter_mut() {
        r.best_pick = false;
    }
    let best = results
        .iter()
        .take_while(|r| relevance(&r.title).0)
        .position(pickable);
    if let Some(at) = best {
        results[..=at].rotate_right(1);
        results[0].best_pick = true;
    }
}

/// Lowercase word tokens; dots are kept only between digits (course numbers like 6.006).
pub(crate) fn tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    let mut out = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let digit_dot = c == '.'
            && i > 0
            && chars[i - 1].is_ascii_digit()
            && chars.get(i + 1).is_some_and(char::is_ascii_digit);
        if c.is_alphanumeric() || digit_dot {
            current.push(c);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::{Candidate, build_response};

    const GIB: u64 = 1024 * 1024 * 1024;

    fn candidate(n: u8, title: &str, seeders: Option<u32>, size: u64) -> Candidate {
        Candidate {
            title: title.into(),
            indexer: Some("T".into()),
            category: Some("Movies".into()),
            size: Some(size),
            seeders,
            peers: seeders.map(|s| s + s / 4),
            infohash: Some(format!("{n:02x}").repeat(20)),
            ..Candidate::default()
        }
    }

    fn ranked(candidates: Vec<Candidate>, query: &str) -> Vec<SearchResult> {
        let mut r = build_response(candidates, Vec::new(), false, false).results;
        rank(&mut r, query);
        r
    }

    fn titles(results: &[SearchResult]) -> Vec<&str> {
        results.iter().map(|r| r.title.as_str()).collect()
    }

    #[test]
    fn relevance_ranks_title_matches_above_popular_unrelated_results() {
        let r = ranked(
            vec![
                candidate(1, "GUNDAM Series (Pack 3) 720p", Some(900), GIB),
                candidate(2, "Submitted papers 6006", Some(500), GIB),
                candidate(
                    3,
                    "Introduction to Algorithms (MIT 6.006) - Video lectures 2020",
                    Some(12),
                    GIB,
                ),
                candidate(4, "MIT 18.03 Differential Equations", Some(40), GIB),
            ],
            "mit 6.006",
        );
        let t = titles(&r);
        assert!(t[0].contains("MIT 6.006"), "{t:?}");
        assert!(
            t[1].contains("18.03"),
            "one matching word beats none: {t:?}"
        );
        assert!(t[2].contains("GUNDAM"), "then by watch score: {t:?}");
    }

    #[test]
    fn camera_copies_rank_below_honest_releases() {
        let r = ranked(
            vec![
                candidate(1, "New Movie 2026 1080p HDCAM x264", Some(3000), 2 * GIB),
                candidate(2, "New Movie 2026 720p WEBRip x264", Some(8), GIB),
                candidate(3, "New Movie 2026 TS 1080p", Some(1500), 2 * GIB),
            ],
            "new movie",
        );
        assert_eq!(
            r[0].title,
            "New Movie 2026 720p WEBRip x264",
            "{:?}",
            titles(&r)
        );
        assert!(r[0].best_pick);
        assert!(r[1].quality.is_cam() && r[2].quality.is_cam());
        assert!(!r[1].best_pick && !r[2].best_pick);
        assert!(r[1].reason.starts_with("camera copy"), "{}", r[1].reason);
    }

    #[test]
    fn fake_4k_is_flagged_and_loses_to_real_hd() {
        let r = ranked(
            vec![
                candidate(
                    1,
                    "Film 2025 2160p 4K HDR x265",
                    Some(200),
                    350 * 1024 * 1024,
                ),
                candidate(2, "Film 2025 1080p BluRay x264 mp4", Some(60), 3 * GIB),
            ],
            "film",
        );
        assert_eq!(r[0].title, "Film 2025 1080p BluRay x264 mp4");
        assert!(r[1].quality.suspect_size);
        assert!(
            r[1].reason.contains("too small to be real 4K"),
            "{}",
            r[1].reason
        );
        assert!(!pickable(&r[1]));
    }

    #[test]
    fn real_4k_with_seeders_beats_1080p_and_hdr_is_shown() {
        let r = ranked(
            vec![
                candidate(1, "Film 2025 1080p WEB-DL x264", Some(100), 4 * GIB),
                candidate(
                    2,
                    "Film 2025 2160p WEB-DL HDR x264 mp4",
                    Some(100),
                    20 * GIB,
                ),
            ],
            "film",
        );
        assert_eq!(r[0].quality.resolution, Some(Resolution::Uhd2160));
        assert_eq!(
            r[0].reason,
            "2160p HDR WEB-DL · 100 seeders · plays in built-in player"
        );
    }

    #[test]
    fn seeders_matter_with_diminishing_returns() {
        let none = Quality::default();
        let s = |n| watch_score(Some(n), Some(n), &none);
        assert!(s(10) > s(1) && s(100) > s(10) && s(1000) > s(100));
        assert!(s(1000) - s(100) < s(10) - s(1) + 10.0, "log-shaped");
        assert!(
            s(0) < watch_score(None, None, &none),
            "known-dead is worse than unknown"
        );
        // A healthy swarm (mostly seeders) beats a crowded one at the same seeder count.
        assert!(watch_score(Some(50), Some(55), &none) > watch_score(Some(50), Some(500), &none));
    }

    #[test]
    fn one_or_two_seeders_are_fragile() {
        let r = ranked(
            vec![
                candidate(1, "Elephants.Dream.2006.1080p.BluRay.x264", Some(1), GIB),
                candidate(2, "Elephants Dream [2006, HD 1080p]", Some(5), GIB),
            ],
            "elephants dream",
        );
        assert_eq!(
            r[0].title,
            "Elephants Dream [2006, HD 1080p]",
            "{:?}",
            titles(&r)
        );
        let one = |s| watch_score(Some(s), Some(s), &Quality::default());
        assert!(
            one(3) - one(2) > one(4) - one(3) + 10.0,
            "a step at three seeders"
        );
    }

    #[test]
    fn best_pick_prefers_audio_the_built_in_player_decodes() {
        // Three times the seeders do not make up for a silent soundtrack.
        let r = ranked(
            vec![
                candidate(
                    1,
                    "Film.2024.1080p.WEB-DL.DDP5.1.Atmos.H.264-GRP",
                    Some(150),
                    4 * GIB,
                ),
                candidate(
                    2,
                    "Film.2024.1080p.WEB-DL.AAC2.0.H.264-GRP",
                    Some(50),
                    3 * GIB,
                ),
            ],
            "film",
        );
        assert_eq!(r[0].title, "Film.2024.1080p.WEB-DL.AAC2.0.H.264-GRP");
        assert!(r[0].best_pick);
        assert_eq!(
            r[1].reason,
            "1080p WEB-DL · 150 seeders · Dolby Digital Plus audio: no sound in built-in player"
        );
        // Still pickable when nothing better exists: the picture plays.
        assert!(pickable(&r[1]));
        let dts = Quality::parse("Film 2024 1080p BluRay DTS-HD x264 mkv", None, None);
        let plain = Quality::parse("Film 2024 1080p BluRay x264 mkv", None, None);
        let score = |q: &Quality| watch_score(Some(40), Some(50), q);
        assert!(score(&plain) - score(&dts) >= 15.0);
    }

    #[test]
    fn converted_dolby_and_dts_audio_is_neither_warned_about_nor_ranked_down() {
        let candidates = vec![
            candidate(
                1,
                "Film.2024.1080p.WEB-DL.DDP5.1.Atmos.H.264-GRP",
                Some(150),
                4 * GIB,
            ),
            candidate(
                2,
                "Film.2024.1080p.WEB-DL.AAC2.0.H.264-GRP",
                Some(50),
                3 * GIB,
            ),
        ];
        let mut r = build_response(candidates, Vec::new(), false, true).results;
        rank(&mut r, "film");
        assert_eq!(
            r[0].title, "Film.2024.1080p.WEB-DL.DDP5.1.Atmos.H.264-GRP",
            "more seeders win once the audio plays"
        );
        assert!(r[0].best_pick);
        assert_eq!(
            r[0].reason,
            "1080p WEB-DL · 150 seeders · Dolby Digital Plus audio: converted to stereo"
        );
        assert!(!r.iter().any(|r| r.reason.contains("no sound")));

        let converted = |title: &str| Quality {
            converts_audio: true,
            ..Quality::parse(title, None, None)
        };
        let dts = converted("Film 2024 1080p BluRay DTS-HD x264 mkv");
        let plain = converted("Film 2024 1080p BluRay x264 mkv");
        let score = |q: &Quality| watch_score(Some(40), Some(50), q);
        assert!(
            (score(&plain) - score(&dts)).abs() < f64::EPSILON,
            "no penalty"
        );
        assert_eq!(
            reason(Some(40), &dts),
            "1080p BluRay · 40 seeders · DTS audio: converted to stereo"
        );
        assert_eq!(
            reason(Some(40), &converted("Film 2024 1080p TrueHD mp4")),
            "1080p · 40 seeders · plays in built-in player · Dolby TrueHD audio: converted to stereo"
        );
        assert_eq!(
            reason(Some(40), &converted("Film 2024 DTS avi")),
            "40 seeders · needs the full player",
            "a file the built-in player cannot open is not converted"
        );
    }

    #[test]
    fn playable_containers_get_a_bonus() {
        let mp4 = Quality::parse("Lecture 1 720p.mp4", None, None);
        let mkv = Quality::parse("Lecture 1 720p.mkv", None, None);
        let avi = Quality::parse("Lecture 1 720p.avi", None, None);
        let score = |q: &Quality| watch_score(Some(20), Some(25), q);
        assert!(score(&mp4) > score(&mkv) && score(&mkv) > score(&avi));
        assert_eq!(
            reason(Some(20), &mp4),
            "720p · 20 seeders · plays in built-in player"
        );
        assert_eq!(
            reason(Some(1), &avi),
            "720p · 1 seeder · needs the full player"
        );
        assert_eq!(reason(None, &Quality::default()), "seeders unknown");
    }

    #[test]
    fn small_torrents_with_no_sign_of_video_are_not_the_best_pick() {
        let unmarked = |n, title: &str, size| Candidate {
            category: Some("Other".into()),
            ..candidate(n, title, Some(42), size)
        };
        // Seen as the best pick for "feynman lectures physics" once audiobooks were excluded.
        let book =
            "The-Feynman-Lectures-on-Physics-Vol-3 -Quantum-Mechanics-[New-millenium-edition]";
        let r = ranked(
            vec![unmarked(1, book, 20 * 1024 * 1024)],
            "feynman lectures physics",
        );
        assert!(!r[0].best_pick);
        let pack = "MIT 6.006 Introduction to Algorithms Fall 2011 lectures";
        let r = ranked(vec![unmarked(2, pack, 8 * GIB)], "mit 6.006");
        assert!(r[0].best_pick, "large, so probably the videos");
    }

    #[test]
    fn audio_can_be_the_best_pick_but_ebooks_never_are() {
        let audiobook = "Feynman Richard - Feynman Lectures on Physics [1961, MP3, 64 kbps, ENG]";
        let mut audio = candidate(1, audiobook, Some(11), GIB);
        audio.category = Some("Аудиокниги".into());
        let r = ranked(vec![audio], "feynman lectures physics");
        assert!(r[0].best_pick, "{}", r[0].reason);
        assert!(r[0].reason.contains("audio"), "{}", r[0].reason);
        let book = candidate(3, "Feynman Lectures on Physics [PDF]", Some(50), GIB);
        let r = ranked(vec![book], "feynman lectures physics");
        assert!(!r[0].best_pick, "{}", r[0].reason);
        assert!(r[0].reason.contains("nothing to play"), "{}", r[0].reason);
    }

    #[test]
    fn best_pick_requires_every_query_word_and_a_playable_video() {
        // The only full match has no seeders: no best pick, even though others have seeders.
        let r = ranked(
            vec![
                candidate(1, "Physics lecture 720p", Some(50), GIB),
                candidate(2, "Yale physics lecture 1080p", Some(0), GIB),
            ],
            "yale physics",
        );
        assert_eq!(r[0].title, "Yale physics lecture 1080p");
        assert!(r.iter().all(|x| !x.best_pick));

        // A non-video full match is skipped for the pick, and the pick moves to the front.
        let mut book = candidate(3, "Yale physics notes", Some(50_000), GIB);
        book.category = Some("Books".into());
        let r = ranked(
            vec![
                book,
                candidate(4, "Yale physics lectures 720p mp4", Some(20), GIB),
            ],
            "yale physics",
        );
        assert!(r[0].best_pick, "{:?}", titles(&r));
        assert_eq!(r[0].title, "Yale physics lectures 720p mp4");
        assert_eq!(r.iter().filter(|x| x.best_pick).count(), 1);
    }

    #[test]
    fn ranking_is_idempotent() {
        let mut r = build_response(
            vec![
                candidate(1, "Course 720p", Some(5), GIB),
                candidate(2, "Course 1080p mp4", Some(9), GIB),
            ],
            Vec::new(),
            false,
            false,
        )
        .results;
        rank(&mut r, "course");
        let first = r.clone();
        rank(&mut r, "course");
        assert_eq!(r, first);
    }

    #[test]
    fn tokens_keep_course_numbers_and_drop_punctuation() {
        assert_eq!(tokens("MIT 6.006, (2020)."), vec!["mit", "6.006", "2020"]);
        assert_eq!(tokens("a.b 1.x"), vec!["a", "b", "1", "x"]);
    }
}
