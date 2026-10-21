// SPDX-License-Identifier: MIT OR Apache-2.0

//! Which subtitle files in a torrent belong to the video that plays.
//!
//! A subtitle file belongs to a video when it is next to it with the same name
//! (`Movie.srt`, `Movie.en.srt`, `Movie.English.forced.srt`), in a `Subs` or `Subtitles`
//! folder next to it (`Subs/Movie.en.srt`, or `Subs/Movie/2_English.srt` as in TV releases),
//! or when the torrent has only one video. Every other subtitle file is listed as "other".
//! The language and the `forced` and SDH flags come from the words after the video's name.

use crate::language::{Language, is_code, language};

/// File extensions of the subtitle formats that can be parsed.
pub const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "vtt", "ass", "ssa"];
/// Most subtitle files listed (matched ones first).
pub const MAX_SUBTITLE_FILES: usize = 200;
/// Folder names that hold subtitles.
const SUBTITLE_FOLDERS: &[&str] = &["subs", "subtitles", "sub", "subtitle", "subz"];

/// A subtitle file in a torrent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubtitleFile {
    /// The file's index in the torrent.
    pub index: usize,
    /// Its path in the torrent.
    pub path: String,
    /// The language its name gives, if any.
    pub language: Option<Language>,
    /// Marked `forced` (only the foreign-language parts).
    pub forced: bool,
    /// Marked for the deaf and hard of hearing (`SDH`, `CC`, `HI`).
    pub sdh: bool,
    /// Whether it belongs to the playing video.
    pub matched: bool,
}

impl SubtitleFile {
    /// The file name (the last path component).
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// A short name for menus: the language and flags (`English (SDH)`), or the file name.
    #[must_use]
    pub fn label(&self) -> String {
        let mut label = self
            .language
            .map_or_else(|| self.file_name().to_owned(), |l| l.name.to_owned());
        let flags: Vec<&str> = [(self.forced, "forced"), (self.sdh, "SDH")]
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
            .collect();
        if !flags.is_empty() {
            label = format!("{label} ({})", flags.join(", "));
        }
        label
    }
}

/// The subtitle files among `files` (index and path pairs, `/`-separated), for `video` (one of
/// the paths). `videos` is how many videos the torrent has. Matched files come first, each
/// group in the order given; at most [`MAX_SUBTITLE_FILES`] are returned.
#[must_use]
pub fn subtitle_files<'a>(
    video: &str,
    videos: usize,
    files: impl IntoIterator<Item = (usize, &'a str)>,
) -> Vec<SubtitleFile> {
    let video = video.to_lowercase();
    let (video_dir, video_name) = split(&video);
    let video_stem = stem(video_name);
    let (mut matched, mut others) = (Vec::new(), Vec::new());
    for (index, path) in files {
        let lower = path.to_lowercase();
        let (dir, name) = split(&lower);
        if !SUBTITLE_EXTENSIONS.contains(&extension(name)) {
            continue;
        }
        let sub_stem = stem(name);
        let after_video = after_stem(sub_stem, video_stem);
        // The folder of the subtitle relative to the video's, as path components.
        let relative: Option<Vec<&str>> = if video_dir.is_empty() {
            Some(dir.split('/').filter(|c| !c.is_empty()).collect())
        } else if dir == video_dir {
            Some(Vec::new())
        } else {
            dir.strip_prefix(video_dir)
                .and_then(|rest| rest.strip_prefix('/'))
                .map(|rest| rest.split('/').collect())
        };
        let in_subs = |c: &&str| SUBTITLE_FOLDERS.contains(c);
        let words = match relative.as_deref() {
            // Next to the video, or in its Subs folder, named after it.
            Some([] | [_])
                if after_video.is_some()
                    && relative.as_deref().is_some_and(|r| r.iter().all(in_subs)) =>
            {
                after_video
            }
            // Subs/<video name>/2_English.srt.
            Some([subs, folder]) if in_subs(subs) && *folder == video_stem => Some(sub_stem),
            _ => None,
        };
        let is_match = words.is_some() || videos == 1;
        let (language, forced, sdh) = tags(words.or(after_video).unwrap_or(sub_stem));
        let file = SubtitleFile {
            index,
            path: path.to_owned(),
            language,
            forced,
            sdh,
            matched: is_match,
        };
        if is_match {
            matched.push(file);
        } else {
            others.push(file);
        }
    }
    matched.extend(others);
    matched.truncate(MAX_SUBTITLE_FILES);
    matched
}

/// Splits a path into its folder (empty at the top) and file name.
fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

fn extension(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// What follows `video` at the start of `name`, if `name` starts with it as a whole word.
fn after_stem<'a>(name: &'a str, video: &str) -> Option<&'a str> {
    let rest = name.strip_prefix(video)?;
    let boundary = rest.is_empty() || rest.starts_with(['.', '_', '-', ' ', '[', '(']);
    (boundary && !video.is_empty()).then_some(rest)
}

/// The language and flags in the words of a name. Names count anywhere (the last one wins);
/// codes only as the last word that is not a flag or a number.
fn tags(words: &str) -> (Option<Language>, bool, bool) {
    let words: Vec<&str> = words
        .split(|c: char| !(c.is_alphanumeric() || c == '-'))
        .map(|w| w.trim_matches('-'))
        .filter(|w| !w.is_empty())
        .collect();
    let forced = words.contains(&"forced");
    let sdh = words
        .iter()
        .any(|w| matches!(*w, "sdh" | "cc" | "hi" | "hoh"));
    let meaningful: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| {
            !matches!(
                *w,
                "forced" | "sdh" | "cc" | "hi" | "hoh" | "full" | "default"
            ) && !w.bytes().all(|b| b.is_ascii_digit())
        })
        .collect();
    let last = meaningful.len().checked_sub(1);
    let found = meaningful.iter().enumerate().rev().find_map(|(i, w)| {
        let lang = language(w)?;
        (!is_code(w) || Some(i) == last).then_some(lang)
    });
    (found, forced, sdh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matched(
        video: &str,
        videos: usize,
        paths: &[&str],
    ) -> Vec<(String, bool, Option<&'static str>)> {
        subtitle_files(video, videos, paths.iter().copied().enumerate())
            .into_iter()
            .map(|f| (f.path, f.matched, f.language.map(|l| l.code)))
            .collect()
    }

    #[test]
    fn matches_files_named_after_the_video() {
        let got = matched(
            "Show/Show.S01E01.mkv",
            2,
            &[
                "Show/Show.S01E01.mkv",
                "Show/Show.S01E01.srt",
                "Show/Show.S01E01.en.srt",
                "Show/show.s01e01.English.forced.SRT",
                "Show/Show.S01E02.en.srt",
                "Show/Show.S01E010.srt",
                "Show/notes.txt",
                "Show/Show.S01E01.idx",
            ],
        );
        assert_eq!(
            got,
            [
                ("Show/Show.S01E01.srt".to_owned(), true, None),
                ("Show/Show.S01E01.en.srt".to_owned(), true, Some("en")),
                (
                    "Show/show.s01e01.English.forced.SRT".to_owned(),
                    true,
                    Some("en")
                ),
                ("Show/Show.S01E02.en.srt".to_owned(), false, Some("en")),
                ("Show/Show.S01E010.srt".to_owned(), false, None),
            ]
        );
    }

    #[test]
    fn matches_subs_folders() {
        let got = matched(
            "Pack/Show.S01E01.mp4",
            3,
            &[
                "Pack/Subs/Show.S01E01/2_English.srt",
                "Pack/Subs/Show.S01E01/3_French.ass",
                "Pack/Subs/Show.S01E02/2_English.srt",
                "Pack/Subtitles/Show.S01E01.pt-BR.vtt",
                "Pack/Subs/English.srt",
            ],
        );
        assert_eq!(
            got,
            [
                (
                    "Pack/Subs/Show.S01E01/2_English.srt".to_owned(),
                    true,
                    Some("en")
                ),
                (
                    "Pack/Subs/Show.S01E01/3_French.ass".to_owned(),
                    true,
                    Some("fr")
                ),
                (
                    "Pack/Subtitles/Show.S01E01.pt-BR.vtt".to_owned(),
                    true,
                    Some("pt-BR")
                ),
                (
                    "Pack/Subs/Show.S01E02/2_English.srt".to_owned(),
                    false,
                    Some("en")
                ),
                ("Pack/Subs/English.srt".to_owned(), false, Some("en")),
            ]
        );
    }

    #[test]
    fn a_single_video_owns_every_subtitle() {
        let got = matched(
            "Movie (2020)/Movie.2020.1080p.mkv",
            1,
            &["Movie (2020)/Subs/English.srt", "Extras/commentary.srt"],
        );
        assert!(got.iter().all(|(_, m, _)| *m));
        assert_eq!(got[0].2, Some("en"));
        // A top-level video.
        let got = matched("lecture.mp4", 1, &["lecture.vtt", "subs/lecture.es.vtt"]);
        assert_eq!(got[1], ("subs/lecture.es.vtt".to_owned(), true, Some("es")));
    }

    #[test]
    fn labels_and_flags() {
        let files = subtitle_files(
            "Movie.mkv",
            2,
            [
                (1, "Movie.eng.SDH.srt"),
                (2, "Movie.forced.srt"),
                (3, "Movie.Chinese (Simplified).ass"),
                (4, "Histoire de France.srt"),
                (5, "It.2017.srt"),
            ],
        );
        let labels: Vec<String> = files.iter().map(SubtitleFile::label).collect();
        assert_eq!(
            labels,
            [
                "English (SDH)",
                "Movie.forced.srt (forced)",
                "Chinese (Simplified)",
                "Histoire de France.srt",
                "Italian",
            ],
            "codes count only as the last word: `de` is not German"
        );
        assert_eq!(files[0].file_name(), "Movie.eng.SDH.srt");
    }

    #[test]
    fn caps_the_list() {
        let paths: Vec<String> = (0..500).map(|i| format!("Other.{i}.srt")).collect();
        let files = subtitle_files("Movie.mkv", 2, paths.iter().map(String::as_str).enumerate());
        assert_eq!(files.len(), MAX_SUBTITLE_FILES);
    }
}
