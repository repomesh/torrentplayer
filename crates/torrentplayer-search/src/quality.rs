// SPDX-License-Identifier: MIT OR Apache-2.0

//! Video quality parsed from torrent titles.
//!
//! Release titles follow loose conventions (`Movie.2010.1080p.WEB-DL.x264-GROUP.mp4`). The
//! parser looks for whole words only, so "Cambridge" is not a camera recording and "Avocado"
//! is not an AVC video. Words that are also ordinary English (`cam`, `ts`, `tc`) count only
//! when the title also looks like a video release (it names a resolution, codec, or year).

/// Vertical resolution class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Resolution {
    /// 480p, 576p, and other standard-definition releases (DVD rips).
    Sd,
    /// 720p.
    Hd720,
    /// 1080p or 1080i.
    Hd1080,
    /// 2160p, "4K", or UHD.
    Uhd2160,
}

impl Resolution {
    /// Short label, for example `1080p`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sd => "480p",
            Self::Hd720 => "720p",
            Self::Hd1080 => "1080p",
            Self::Uhd2160 => "2160p",
        }
    }
}

/// Video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// H.264 / AVC (x264): plays everywhere.
    Avc,
    /// H.265 / HEVC (x265): efficient, but webviews decode it only with hardware support.
    Hevc,
    /// AV1.
    Av1,
    /// VP9.
    Vp9,
    /// Xvid or `DivX` (MPEG-4 Part 2): needs the full player.
    Xvid,
}

impl Codec {
    /// Short label, for example `x264`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Avc => "x264",
            Self::Hevc => "x265",
            Self::Av1 => "AV1",
            Self::Vp9 => "VP9",
            Self::Xvid => "Xvid",
        }
    }
}

/// Where the video came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// Blu-ray rip or remux.
    BluRay,
    /// Untouched download from a streaming service.
    WebDl,
    /// Re-encoded capture from a streaming service.
    WebRip,
    /// Television broadcast capture.
    Hdtv,
    /// DVD rip.
    Dvd,
    /// Camera recording in a cinema, telesync, telecine, or screener: poor quality.
    Cam,
}

impl Source {
    /// Short label, for example `WEB-DL`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BluRay => "BluRay",
            Self::WebDl => "WEB-DL",
            Self::WebRip => "WEBRip",
            Self::Hdtv => "HDTV",
            Self::Dvd => "DVD",
            Self::Cam => "CAM",
        }
    }
}

/// File container named in the title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Container {
    /// MP4 or M4V.
    Mp4,
    /// `WebM`.
    Webm,
    /// Matroska.
    Mkv,
    /// AVI, WMV, FLV, MPEG-TS, or MPEG-PS: the webview player cannot open these.
    Legacy,
}

impl Container {
    /// Short label, for example `MP4`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mp4 => "MP4",
            Self::Webm => "WebM",
            Self::Mkv => "MKV",
            Self::Legacy => "AVI/TS",
        }
    }
}

/// Audio codec named in the title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Audio {
    /// AAC, Opus, MP3, FLAC, or Vorbis: the built-in player decodes these.
    Web,
    /// Dolby Digital (AC-3, `DD5.1`).
    DolbyDigital,
    /// Dolby Digital Plus (E-AC-3, `DDP5.1`, `DD+`), which carries Atmos in WEB releases.
    DolbyDigitalPlus,
    /// Dolby `TrueHD`, which carries Atmos on Blu-ray.
    TrueHd,
    /// DTS, DTS-HD, or DTS:X.
    Dts,
}

impl Audio {
    /// Short label, for example `DTS`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Web => "AAC/Opus",
            Self::DolbyDigital => "Dolby Digital",
            Self::DolbyDigitalPlus => "Dolby Digital Plus",
            Self::TrueHd => "Dolby TrueHD",
            Self::Dts => "DTS",
        }
    }

    /// Whether the built-in (webview) player decodes it. Chromium-based webviews decode none
    /// of the Dolby or DTS codecs, so such releases play without sound.
    #[must_use]
    pub const fn plays_in_webview(self) -> bool {
        matches!(self, Self::Web)
    }
}

/// Whether the built-in (webview) player can be expected to play a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Playability {
    /// An MP4 or `WebM` container with a web codec.
    Plays,
    /// Probably plays (Matroska, or a web codec in an unnamed container); the audio may still
    /// be Dolby or DTS if the title does not say.
    Likely,
    /// HEVC outside MP4 (plays only where the system has a hardware HEVC decoder), or Dolby or
    /// DTS audio named in the title (the picture plays without sound).
    Unlikely,
    /// A container or codec the webview cannot decode at all.
    NeedsFullPlayer,
    /// The title does not say.
    Unknown,
}

/// What a torrent contains, judged by its category and title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ContentKind {
    /// Movies, TV, anime, or a title with video markers.
    Video,
    /// Music, audiobooks, podcasts: playable, without a picture.
    Audio,
    /// Books, software, or games: nothing to play.
    NotVideo,
    /// Nothing to go by.
    #[default]
    Unknown,
}

/// Quality information parsed from a title, category, and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Quality {
    /// Resolution, if the title names one.
    pub resolution: Option<Resolution>,
    /// High dynamic range (HDR10, HDR10+, Dolby Vision).
    pub hdr: bool,
    /// Video codec, if named.
    pub codec: Option<Codec>,
    /// Release source, if named.
    pub source: Option<Source>,
    /// Container, if named (or implied, as for YTS releases).
    pub container: Option<Container>,
    /// Audio codec, if named; a web codec if the title names several.
    pub audio: Option<Audio>,
    /// Claims 4K but is too small to be real 4K video (likely mislabeled or fake).
    pub suspect_size: bool,
    /// Video or not.
    pub kind: ContentKind,
}

/// Total size below which a "4K" torrent is flagged as suspect. Real 2160p video runs at
/// 15–80 Mbit/s, so even a 10-minute clip at the low end is over 1 GB.
pub const MIN_UHD_BYTES: u64 = 600 * 1024 * 1024;

impl Quality {
    /// Parses a title. `category` is the indexer's category (name or Torznab id) and `size`
    /// the total size in bytes, when known.
    #[must_use]
    pub fn parse(title: &str, category: Option<&str>, size: Option<u64>) -> Self {
        let words = Words::new(title);
        let resolution = resolution(&words);
        let codec = codec(&words);
        let hdr = words.any(&["hdr", "hdr10", "hdr10plus", "dovi", "dolbyvision"])
            || words.pair("dolby", "vision")
            || (resolution == Some(Resolution::Uhd2160) && words.has("dv"));
        // Ambiguous words count only in titles that look like video releases.
        let release_like =
            resolution.is_some() || codec.is_some() || words.0.iter().any(|w| is_year(w));
        let source = source(&words, release_like);
        let container = container(&words);
        let audio = audio(&words, &title.to_lowercase(), release_like);
        let suspect_size =
            resolution == Some(Resolution::Uhd2160) && size.is_some_and(|s| s < MIN_UHD_BYTES);

        let video_markers = resolution.is_some() || codec.is_some() || source.is_some();
        let unmarked = !video_markers && container.is_none();
        let kind = match category.map(category_kind) {
            // An album or audiobook, or an ebook, whatever the category says.
            _ if unmarked && audio_only(&words) => ContentKind::Audio,
            _ if unmarked && ebook(&words) => ContentKind::NotVideo,
            Some(ContentKind::Video) => ContentKind::Video,
            Some(kind @ (ContentKind::Audio | ContentKind::NotVideo)) if !video_markers => kind,
            _ if video_markers || container.is_some() => ContentKind::Video,
            _ => ContentKind::Unknown,
        };

        Self {
            resolution,
            hdr,
            codec,
            source,
            container,
            audio,
            suspect_size,
            kind,
        }
    }

    /// Whether this is a camera recording, telesync, telecine, or screener.
    #[must_use]
    pub fn is_cam(&self) -> bool {
        self.source == Some(Source::Cam)
    }

    /// The audio codec named in the title, if the built-in player cannot decode it.
    #[must_use]
    pub fn silent_audio(&self) -> Option<Audio> {
        self.audio.filter(|a| !a.plays_in_webview())
    }

    /// Whether the built-in player can be expected to play it.
    #[must_use]
    pub fn playability(&self) -> Playability {
        if self.container == Some(Container::Legacy) || self.codec == Some(Codec::Xvid) {
            return Playability::NeedsFullPlayer;
        }
        if self.silent_audio().is_some() {
            return Playability::Unlikely;
        }
        match (self.container, self.codec) {
            (Some(Container::Mp4 | Container::Webm), Some(Codec::Hevc)) => Playability::Likely,
            (Some(Container::Mp4 | Container::Webm), _) => Playability::Plays,
            (_, Some(Codec::Hevc)) => Playability::Unlikely,
            (_, Some(_)) | (Some(Container::Mkv), None) => Playability::Likely,
            _ => Playability::Unknown,
        }
    }

    /// Resolution, HDR, and source, for example `2160p HDR WEB-DL`. Empty if none is known.
    #[must_use]
    pub fn label(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(r) = self.resolution {
            parts.push(r.label());
        }
        if self.hdr {
            parts.push("HDR");
        }
        if let Some(s) = self.source {
            parts.push(s.label());
        }
        parts.join(" ")
    }
}

/// Lowercase alphanumeric words of a title; `hdr10+` becomes `hdr10plus`.
struct Words(Vec<String>);

impl Words {
    fn new(title: &str) -> Self {
        let lower = title.to_lowercase().replace("hdr10+", "hdr10plus ");
        Self(
            lower
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    fn has(&self, word: &str) -> bool {
        self.0.iter().any(|w| w == word)
    }

    fn any(&self, words: &[&str]) -> bool {
        self.0.iter().any(|w| words.contains(&w.as_str()))
    }

    /// Two adjacent words, as in `WEB-DL` or `H.264`.
    fn pair(&self, a: &str, b: &str) -> bool {
        self.0.windows(2).any(|p| p[0] == a && p[1] == b)
    }

    fn yts(&self) -> bool {
        self.has("yts") || self.has("yify")
    }
}

fn resolution(words: &Words) -> Option<Resolution> {
    if words.any(&["2160p", "4k", "uhd", "3840x2160", "4096x2160"]) {
        Some(Resolution::Uhd2160)
    } else if words.any(&["1080p", "1080i", "1920x1080", "fhd"]) {
        Some(Resolution::Hd1080)
    } else if words.any(&["720p", "1280x720"]) {
        Some(Resolution::Hd720)
    } else if words.any(&["480p", "576p", "360p", "sd", "dvdrip"]) {
        Some(Resolution::Sd)
    } else {
        None
    }
}

fn codec(words: &Words) -> Option<Codec> {
    if words.any(&["x265", "h265", "hevc"]) || words.pair("h", "265") {
        Some(Codec::Hevc)
    } else if words.has("av1") {
        Some(Codec::Av1)
    } else if words.has("vp9") {
        Some(Codec::Vp9)
    } else if words.any(&["x264", "h264", "avc"]) || words.pair("h", "264") || words.yts() {
        Some(Codec::Avc)
    } else if words.any(&["xvid", "divx"]) {
        Some(Codec::Xvid)
    } else {
        None
    }
}

const CAM_WORDS: &[&str] = &[
    "camrip",
    "hdcam",
    "hqcam",
    "telesync",
    "hdts",
    "hdtc",
    "telecine",
    "screener",
    "dvdscr",
    "bdscr",
    "webscreener",
    "predvd",
];

fn source(words: &Words, release_like: bool) -> Option<Source> {
    let cam = words.any(CAM_WORDS)
        || (release_like
            && words.0.iter().enumerate().any(|(i, w)| {
                // A trailing `ts` is a file extension, not a telesync.
                matches!(w.as_str(), "cam" | "tc" | "scr") || (w == "ts" && i + 1 < words.0.len())
            }));
    if cam {
        Some(Source::Cam)
    } else if words.any(&["bluray", "bdrip", "brrip", "bdremux", "remux"])
        || words.pair("blu", "ray")
    {
        Some(Source::BluRay)
    } else if words.has("webdl") || words.pair("web", "dl") {
        Some(Source::WebDl)
    } else if words.has("webrip") || words.pair("web", "rip") {
        Some(Source::WebRip)
    } else if words.has("web") {
        Some(Source::WebDl)
    } else if words.any(&["hdtv", "pdtv"]) {
        Some(Source::Hdtv)
    } else if words.any(&["dvdrip", "dvd", "dvd5", "dvd9"]) {
        Some(Source::Dvd)
    } else {
        None
    }
}

fn container(words: &Words) -> Option<Container> {
    if words.any(&["mp4", "m4v"]) || words.yts() {
        Some(Container::Mp4)
    } else if words.has("webm") {
        Some(Container::Webm)
    } else if words.has("mkv") {
        Some(Container::Mkv)
    } else if words.any(&["avi", "wmv", "flv", "m2ts", "mpg", "mpeg"])
        || words.0.last().is_some_and(|w| w == "ts")
    {
        Some(Container::Legacy)
    } else {
        None
    }
}

/// The audio codec a title names: `AAC2.0`, `DD5.1`, `DDP5.1`, `DD+`, `EAC3`, `AC3`, `DTS-HD`,
/// `DTS-X`, `TrueHD`, `Atmos`. A web codec wins when several are named, since the release then
/// likely has a track the built-in player decodes. `lower` is the lowercase title (`DD+` loses
/// its plus in [`Words`]); a bare `DD` counts only in release-like titles.
fn audio(words: &Words, lower: &str, release_like: bool) -> Option<Audio> {
    // A codec word, alone or followed by its channel count (`aac2`, `dd5`, `ddp5`, `dts5`).
    let stem = |name: &str| {
        words.0.iter().any(|w| {
            w.strip_prefix(name)
                .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
        })
    };
    let triple = |a: &str, b: &str, c: &str| {
        words
            .0
            .windows(3)
            .any(|t| t[0] == a && t[1] == b && t[2] == c)
    };
    let dd_plus = lower.match_indices("dd+").any(|(at, _)| {
        !lower[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
    });
    if stem("aac") || words.any(&["opus", "mp3", "flac", "vorbis"]) {
        Some(Audio::Web)
    } else if words.has("truehd") || words.pair("true", "hd") {
        Some(Audio::TrueHd)
    } else if stem("dts") || words.any(&["dtshd", "dtsx", "dtsma", "dtses"]) {
        Some(Audio::Dts)
    } else if dd_plus
        || stem("ddp")
        || words.any(&["eac3", "atmos"])
        || triple("e", "ac", "3")
        || triple("dolby", "digital", "plus")
    {
        Some(Audio::DolbyDigitalPlus)
    } else if words.0.iter().any(|w| {
        w.strip_prefix("dd")
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
    }) || (release_like && words.has("dd"))
        || words.has("ac3")
        || words.pair("ac", "3")
        || words.pair("dolby", "digital")
    {
        Some(Audio::DolbyDigital)
    } else {
        None
    }
}

/// Whether a title names audio formats: an audio file type, an audiobook, or a bitrate in
/// kbit/s (`64 kbps`, `320kbps`). Only checked when nothing in the title says video.
fn audio_only(words: &Words) -> bool {
    words.any(&[
        "mp3",
        "flac",
        "m4b",
        "m4a",
        "wav",
        "alac",
        "kbps",
        "audiobook",
        "audiobooks",
        "аудиокнига",
        "аудиокниги",
    ]) || words.pair("audio", "book")
        || words.0.iter().any(|w| {
            w.strip_suffix("kbps")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        })
}

/// Whether a title names an ebook or document format.
fn ebook(words: &Words) -> bool {
    words.any(&[
        "pdf", "epub", "mobi", "djvu", "azw3", "chm", "ebook", "ebooks",
    ]) || words.pair("e", "book")
}

/// What Torznab category ids say a torrent is: video if any is Movies (2000s) or TV (5000s),
/// otherwise not video if any is Console, Audio, PC, or Books. Indexer-specific ids (over
/// 100000) and XXX or Other say nothing.
#[must_use]
pub fn kind_of_category_ids(ids: impl IntoIterator<Item = u64>) -> Option<ContentKind> {
    let mut kind = None;
    for id in ids {
        match id / 1000 {
            2 | 5 => return Some(ContentKind::Video),
            1 | 3 | 4 | 7 => kind = Some(ContentKind::NotVideo),
            _ => {}
        }
    }
    kind
}

fn is_year(word: &str) -> bool {
    word.len() == 4
        && word
            .parse::<u16>()
            .is_ok_and(|y| (1900..=2099).contains(&y))
}

/// Classifies an indexer category name (`Movies/HD`, `Audio`) or Torznab id (`2040`).
fn category_kind(category: &str) -> ContentKind {
    if let Ok(id) = category.trim().parse::<u32>() {
        return match id / 1000 {
            2 | 5 => ContentKind::Video,
            3 => ContentKind::Audio,
            1 | 4 | 7 => ContentKind::NotVideo,
            _ => ContentKind::Unknown,
        };
    }
    let lower = category.to_ascii_lowercase();
    let any = |names: &[&str]| names.iter().any(|n| lower.contains(n));
    if any(&[
        "movie",
        "tv",
        "anime",
        "video",
        "documentar",
        "film",
        "series",
    ]) {
        ContentKind::Video
    } else if any(&["audiobook", "audio book"]) {
        ContentKind::Audio
    } else if any(&["book", "comic"]) {
        ContentKind::NotVideo
    } else if any(&["audio", "music", "podcast", "flac", "mp3", "lossless"]) {
        ContentKind::Audio
    } else if any(&[
        "software", "game", "console", "app", "pc/", "iso", "mac", "linux", "windows",
    ]) || lower == "pc"
    {
        ContentKind::NotVideo
    } else {
        ContentKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn q(title: &str) -> Quality {
        Quality::parse(title, None, None)
    }

    #[test]
    fn parses_common_release_titles() {
        let x = q("Sintel.2010.1080p.WEB-DL.H.264-CLASSiCALHD");
        assert_eq!(x.resolution, Some(Resolution::Hd1080));
        assert_eq!(x.source, Some(Source::WebDl));
        assert_eq!(x.codec, Some(Codec::Avc));
        assert_eq!(x.container, None);
        assert_eq!(x.kind, ContentKind::Video);
        assert_eq!(x.label(), "1080p WEB-DL");
        assert_eq!(x.playability(), Playability::Likely);

        let x = q("Sintel (2010) 720p BRRip x264 -YTS");
        assert_eq!(x.resolution, Some(Resolution::Hd720));
        assert_eq!(x.source, Some(Source::BluRay));
        assert_eq!(x.container, Some(Container::Mp4), "YTS releases are MP4");
        assert_eq!(x.playability(), Playability::Plays);

        let x = q("Lecture 01 - Introduction [720p].webm");
        assert_eq!(x.container, Some(Container::Webm));
        assert_eq!(x.playability(), Playability::Plays);

        let x = q("Show S01E01 480p DVDRip XviD.avi");
        assert_eq!(x.resolution, Some(Resolution::Sd));
        assert_eq!(x.source, Some(Source::Dvd));
        assert_eq!(x.playability(), Playability::NeedsFullPlayer);
    }

    #[test]
    fn detects_hdr_and_uhd() {
        let x = q("Planet.2023.2160p.UHD.BluRay.REMUX.HDR10+.DV.HEVC-GRP");
        assert_eq!(x.resolution, Some(Resolution::Uhd2160));
        assert!(x.hdr);
        assert_eq!(x.codec, Some(Codec::Hevc));
        assert_eq!(x.source, Some(Source::BluRay));
        assert_eq!(x.label(), "2160p HDR BluRay");
        assert_eq!(x.playability(), Playability::Unlikely, "HEVC outside MP4");

        assert!(q("Movie 2019 4K Dolby Vision x265").hdr);
        assert!(!q("Movie 2019 1080p x264").hdr);
        assert!(
            !q("DV camcorder repair guide").hdr,
            "DV only means HDR with 4K"
        );
    }

    #[test]
    fn flags_cam_telesync_and_screeners() {
        for title in [
            "New.Movie.2026.HDCAM.x264",
            "New Movie (2026) 1080p TS x264",
            "New.Movie.2026.TELESYNC.720p",
            "New Movie 2026 DVDScr XviD",
            "New Movie 2026 CAM",
            "New.Movie.2026.HDTC.1080p",
        ] {
            assert!(q(title).is_cam(), "{title}");
        }
        // Ordinary words that look like source tags.
        for title in [
            "Cambridge lectures on physics",
            "Webcam basics",
            "TS Eliot poetry reading",
            "lecture01.ts",
            "Scrubs complete",
        ] {
            assert!(!q(title).is_cam(), "{title}");
        }
        assert_eq!(q("lecture01.ts").container, Some(Container::Legacy));
    }

    #[test]
    fn flags_fake_4k_by_size() {
        let fake = Quality::parse("Movie.2025.2160p.4K.x265", None, Some(400 * 1024 * 1024));
        assert!(fake.suspect_size);
        let real = Quality::parse("Movie.2025.2160p.4K.x265", None, Some(14 * GIB));
        assert!(!real.suspect_size);
        let unknown = Quality::parse("Movie.2025.2160p", None, None);
        assert!(!unknown.suspect_size, "no size, no verdict");
        let hd = Quality::parse("Movie.2025.1080p", None, Some(300 * 1024 * 1024));
        assert!(!hd.suspect_size, "only 4K claims are checked");
    }

    #[test]
    fn classifies_content_kind() {
        let k = |title: &str, cat: &str| Quality::parse(title, Some(cat), None).kind;
        assert_eq!(k("Anything", "Movies/HD"), ContentKind::Video);
        assert_eq!(k("Anything", "2040"), ContentKind::Video);
        assert_eq!(k("Algorithms textbook", "Books"), ContentKind::NotVideo);
        assert_eq!(k("Ubuntu 24.04", "PC/ISO"), ContentKind::NotVideo);
        assert_eq!(k("Album", "3000"), ContentKind::Audio);
        assert_eq!(
            k("Lecture 3 720p x264", "Other"),
            ContentKind::Video,
            "video markers"
        );
        assert_eq!(
            k("Course 1080p mp4", "Books"),
            ContentKind::Video,
            "markers beat a wrong category"
        );
        assert_eq!(k("MIT 6.006 lectures", "Other"), ContentKind::Unknown);
    }

    #[test]
    fn audio_only_titles_are_audio() {
        let k = |title: &str, cat: Option<&str>| Quality::parse(title, cat, None).kind;
        // Seen as the best pick for "feynman lectures physics" (October 2026).
        let feynman = "Feynman Richard / Feynman Richard - Feynman Lectures on Physics /                        Feynman lectures on physics [1961, MP3, 64 kbps, ENG]";
        assert_eq!(k(feynman, Some("Аудиокниги")), ContentKind::Audio);
        assert_eq!(
            k(feynman, Some("Movies")),
            ContentKind::Audio,
            "despite the category"
        );
        assert_eq!(k("Lectures on Physics audiobook", None), ContentKind::Audio);
        assert_eq!(k("Lectures (Audio Book) m4b", None), ContentKind::Audio);
        assert_eq!(k("Course recordings 320kbps", None), ContentKind::Audio);
        assert_eq!(k("Album [FLAC]", None), ContentKind::Audio);
        assert_eq!(k("Anything", Some("Music/Lossless")), ContentKind::Audio);
        assert_eq!(k("Anything", Some("Audiobooks")), ContentKind::Audio);
        assert_eq!(k("Anything", Some("Books/EBook")), ContentKind::NotVideo);
        // Video markers or a video container win.
        assert_eq!(k("Lecture 1 720p mp4 AAC", None), ContentKind::Video);
        assert_eq!(k("Lectures 1080p x264 FLAC", None), ContentKind::Video);
        assert_eq!(k("Course mp4 with mp3 notes", None), ContentKind::Video);
        assert_eq!(
            k("Kbpsx lectures", None),
            ContentKind::Unknown,
            "whole words only"
        );
        assert_eq!(k("Lectures on Physics [PDF]", None), ContentKind::NotVideo);
        assert_eq!(
            k("Lectures on Physics (e-book, EPUB)", None),
            ContentKind::NotVideo
        );
        assert_eq!(k("Lectures 720p (slides as pdf)", None), ContentKind::Video);
    }

    #[test]
    fn reads_torznab_category_ids() {
        assert_eq!(
            kind_of_category_ids([3000, 103_030]),
            Some(ContentKind::NotVideo)
        );
        assert_eq!(kind_of_category_ids([7020]), Some(ContentKind::NotVideo));
        assert_eq!(kind_of_category_ids([3000, 2040]), Some(ContentKind::Video));
        assert_eq!(kind_of_category_ids([5070]), Some(ContentKind::Video));
        assert_eq!(kind_of_category_ids([6000, 8000, 126_537]), None);
        assert_eq!(kind_of_category_ids([]), None);
    }

    #[test]
    fn whole_words_only() {
        assert_eq!(q("Avocado farming").codec, None);
        assert_eq!(q("Hevc").codec, Some(Codec::Hevc));
        assert_eq!(q("Uhdx").resolution, None);
        assert_eq!(q("Webinar recording").source, None);
    }

    #[test]
    fn detects_audio_codecs() {
        for (title, audio) in [
            (
                "Movie.2024.1080p.WEB-DL.DDP5.1.H.264-GRP",
                Audio::DolbyDigitalPlus,
            ),
            (
                "Movie 2024 1080p WEB-DL DD+ 5.1 x264",
                Audio::DolbyDigitalPlus,
            ),
            (
                "Movie.2024.2160p.WEB-DL.DDP5.1.Atmos.HEVC",
                Audio::DolbyDigitalPlus,
            ),
            ("Movie 2024 1080p WEB E-AC-3 x264", Audio::DolbyDigitalPlus),
            ("Movie.2024.720p.HDTV.DD5.1.x264", Audio::DolbyDigital),
            ("Movie 2024 DVDRip AC3 XviD", Audio::DolbyDigital),
            ("Movie 2024 1080p BluRay AC-3 x264", Audio::DolbyDigital),
            ("Movie 2024 1080p DD 2.0", Audio::DolbyDigital),
            ("Movie.2024.1080p.BluRay.DTS-HD.MA.5.1.x264", Audio::Dts),
            ("Movie.2024.2160p.UHD.BluRay.DTS-X.7.1", Audio::Dts),
            (
                "Movie.2024.2160p.BluRay.REMUX.TrueHD.7.1.Atmos",
                Audio::TrueHd,
            ),
            ("Movie.2024.1080p.WEB-DL.AAC2.0.H.264", Audio::Web),
            ("Movie 2024 1080p WEBRip x265 Opus", Audio::Web),
            ("Movie 2024 1080p BluRay DTS AAC x264", Audio::Web),
        ] {
            assert_eq!(q(title).audio, Some(audio), "{title}");
        }
        for title in [
            "Sintel 2010 1080p x264",
            "Add+ multiply lecture",
            "DD notes",
            "Kodd2 drill",
        ] {
            assert_eq!(q(title).audio, None, "{title}");
        }
    }

    #[test]
    fn dolby_and_dts_audio_lower_playability() {
        let ddp = q("Movie.2024.1080p.WEB-DL.DDP5.1.H.264.mp4");
        assert_eq!(ddp.silent_audio(), Some(Audio::DolbyDigitalPlus));
        assert_eq!(ddp.playability(), Playability::Unlikely);
        assert_eq!(
            q("Movie 2024 1080p DTS mkv").playability(),
            Playability::Unlikely
        );
        assert_eq!(
            q("Movie 2024 720p AAC mp4").playability(),
            Playability::Plays
        );
        assert_eq!(
            q("Movie 2024 DTS avi").playability(),
            Playability::NeedsFullPlayer
        );
        assert_eq!(Audio::Dts.label(), "DTS");
        assert!(!Audio::TrueHd.plays_in_webview() && Audio::Web.plays_in_webview());
    }
}
