// SPDX-License-Identifier: MIT OR Apache-2.0

//! Subtitle files (SRT, `WebVTT`, ASS/SSA) parsed into plain-text cues.
//!
//! Subtitle files come from torrents or the user's disk, so they are untrusted. Parsing is
//! linear in the input, never panics, and is bounded: the input is at most
//! [`MAX_SUBTITLE_BYTES`], at most [`MAX_CUES`] cues are kept, and each cue's text is capped.
//! The output is plain text: markup (`<i>`, `<font>`, `WebVTT` `<v>` and `<c>` spans, ASS
//! `{\i1}` override blocks and drawings) is removed, entities are decoded, and control
//! characters are dropped. Whoever displays it must still treat it as text, never as HTML.
//!
//! Text encodings: UTF-8 with or without a byte order mark, UTF-16 LE or BE (by byte order
//! mark, or detected from the zero bytes of mostly-ASCII text), and Windows-1252 for anything
//! that is not valid UTF-8.

/// Largest subtitle file parsed (5 MiB).
pub const MAX_SUBTITLE_BYTES: usize = 5 * 1024 * 1024;
/// Most cues kept; later ones are dropped and [`Subtitles::truncated`] is set.
pub const MAX_CUES: usize = 20_000;
/// Longest cue text kept, in characters.
const MAX_CUE_CHARS: usize = 600;
/// Most lines per cue.
const MAX_CUE_LINES: usize = 8;
/// Most raw lines collected for one cue before cleaning.
const MAX_RAW_LINES: usize = 64;
/// Latest time accepted (100 hours).
const MAX_TIME_MS: u64 = 100 * 3600 * 1000;
/// Longest `<...>` or `{...}` run removed as markup; longer runs stay text.
const MAX_TAG_CHARS: usize = 256;
/// How much of the file is looked at to recognize its format.
const SNIFF_BYTES: usize = 64 * 1024;

/// One subtitle cue.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cue {
    /// When it appears, in milliseconds.
    pub start_ms: u64,
    /// When it disappears, in milliseconds (after `start_ms`).
    pub end_ms: u64,
    /// Plain text; lines separated by `\n`.
    pub text: String,
}

/// A subtitle file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubtitleFormat {
    /// `SubRip` (`.srt`).
    Srt,
    /// `WebVTT` (`.vtt`).
    WebVtt,
    /// Advanced `SubStation` Alpha or `SubStation` Alpha (`.ass`, `.ssa`).
    Ass,
}

impl SubtitleFormat {
    /// Stable lowercase identifier: `srt`, `webvtt`, or `ass`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Srt => "srt",
            Self::WebVtt => "webvtt",
            Self::Ass => "ass",
        }
    }
}

/// A parsed subtitle file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Subtitles {
    /// The format it was in.
    pub format: SubtitleFormat,
    /// Cues sorted by start time, without duplicates.
    pub cues: Vec<Cue>,
    /// More than [`MAX_CUES`] cues were found; the rest were dropped.
    pub truncated: bool,
}

/// Why a subtitle file could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubtitleError {
    /// Larger than [`MAX_SUBTITLE_BYTES`].
    #[error("the subtitle file is larger than 5 MiB")]
    TooLarge,
    /// Not SRT, `WebVTT`, ASS, or SSA.
    #[error("this is not an SRT, WebVTT, ASS, or SSA subtitle file")]
    Unrecognized,
    /// Recognized, but without a single usable cue.
    #[error("the subtitle file has no subtitles in it")]
    Empty,
}

/// Parses a subtitle file, recognizing its format and text encoding from its contents.
///
/// # Errors
///
/// Returns an error if the file is too large, not a subtitle file, or has no cues.
pub fn parse_subtitles(bytes: &[u8]) -> Result<Subtitles, SubtitleError> {
    if bytes.len() > MAX_SUBTITLE_BYTES {
        return Err(SubtitleError::TooLarge);
    }
    let text = decode(bytes).replace("\r\n", "\n").replace('\r', "\n");
    let format = detect(&text).ok_or(SubtitleError::Unrecognized)?;
    let mut sink = Sink::default();
    match format {
        SubtitleFormat::Srt => srt(&text, &mut sink),
        SubtitleFormat::WebVtt => webvtt(&text, &mut sink),
        SubtitleFormat::Ass => ass(&text, &mut sink),
    }
    let (cues, truncated) = sink.finish();
    if cues.is_empty() {
        return Err(SubtitleError::Empty);
    }
    Ok(Subtitles {
        format,
        cues,
        truncated,
    })
}

// --- Text encodings ----------------------------------------------------------------------

/// Windows-1252's characters for bytes 0x80 to 0x9F (the rest match Latin-1). Unassigned
/// bytes map to C1 controls, which cue cleaning drops.
const CP1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
];

/// Decodes subtitle bytes to text (see the module documentation).
fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        return utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        return utf16(rest, false);
    }
    if let Some(little_endian) = utf16_without_bom(bytes) {
        return utf16(bytes, little_endian);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes
            .iter()
            .map(|&b| match b {
                0x80..=0x9F => CP1252
                    .get(usize::from(b - 0x80))
                    .copied()
                    .unwrap_or('\u{FFFD}'),
                _ => char::from(b),
            })
            .collect(),
    }
}

fn utf16(bytes: &[u8], little_endian: bool) -> String {
    let units = bytes.chunks_exact(2).map(|pair| {
        let pair = [pair[0], pair[1]];
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    char::decode_utf16(units)
        .map(|c| c.unwrap_or('\u{FFFD}'))
        .collect()
}

/// UTF-16 without a byte order mark: in mostly-ASCII text, every other byte is zero. Returns
/// whether it is little-endian, or `None` if it does not look like UTF-16.
fn utf16_without_bom(bytes: &[u8]) -> Option<bool> {
    let sample = bytes.get(..bytes.len().min(512) & !1)?;
    if sample.len() < 8 {
        return None;
    }
    let zeros = |skip: usize| {
        sample
            .iter()
            .skip(skip)
            .step_by(2)
            .filter(|b| **b == 0)
            .count()
    };
    let (even, odd) = (zeros(0), zeros(1));
    let half = sample.len() / 2;
    if even == 0 && odd * 10 >= half * 9 {
        Some(true)
    } else if odd == 0 && even * 10 >= half * 9 {
        Some(false)
    } else {
        None
    }
}

// --- Format detection and timestamps -------------------------------------------------------

fn detect(text: &str) -> Option<SubtitleFormat> {
    let text = text.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}');
    if text.starts_with("WEBVTT") {
        return Some(SubtitleFormat::WebVtt);
    }
    let mut end = text.len().min(SNIFF_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = text.get(..end).unwrap_or_default();
    let ass = head.lines().any(|line| {
        let line = line.trim();
        line.eq_ignore_ascii_case("[script info]")
            || line.eq_ignore_ascii_case("[events]")
            || starts_with_ignore_case(line, "dialogue:")
    });
    if ass {
        Some(SubtitleFormat::Ass)
    } else if head.lines().any(|line| timing(line).is_some()) {
        Some(SubtitleFormat::Srt)
    } else {
        None
    }
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

/// A timestamp: `[H:]MM:SS[,.]fff`, with 1 to 3 hour digits and any number of fraction digits
/// (ASS uses centiseconds). Returns milliseconds.
fn timestamp(text: &str) -> Option<u64> {
    let digits = |s: &str, max: usize| {
        (!s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    let (clock, fraction) = match text.find([',', '.']) {
        Some(at) => (text.get(..at)?, Some(text.get(at + 1..)?)),
        None => (text, None),
    };
    let mut parts = clock.rsplit(':');
    let seconds = digits(parts.next()?, 2)?;
    let minutes = digits(parts.next()?, 2)?;
    let hours = match parts.next() {
        Some(h) => digits(h, 3)?,
        None => 0,
    };
    if parts.next().is_some() || seconds >= 60 || minutes >= 60 {
        return None;
    }
    // Milliseconds from the first three fraction digits: `5` is 500, `05` is 50.
    let millis = match fraction {
        Some(fraction) => {
            let all = digits(fraction, 9)?;
            let len = u32::try_from(fraction.len()).ok()?;
            if len >= 3 {
                all / 10u64.pow(len - 3)
            } else {
                all * 10u64.pow(3 - len)
            }
        }
        None => 0,
    };
    let ms = ((hours * 60 + minutes) * 60 + seconds) * 1000 + millis;
    (ms <= MAX_TIME_MS).then_some(ms)
}

/// A timing line, `start --> end`, with anything after `end` (`WebVTT` settings, SRT
/// coordinates) ignored.
fn timing(line: &str) -> Option<(u64, u64)> {
    let (start, rest) = line.split_once("-->")?;
    let end = rest.split_whitespace().next()?;
    Some((timestamp(start.trim())?, timestamp(end)?))
}

// --- Formats -------------------------------------------------------------------------------

/// SRT: a timing line starts a cue; text runs until the next timing line. The next cue's
/// number, which precedes its timing line after a blank line, is removed.
fn srt(text: &str, sink: &mut Sink) {
    let mut cue: Option<(u64, u64, Vec<&str>)> = None;
    for line in text.lines() {
        if let Some((start, end)) = timing(line) {
            if let Some(done) = cue.take() {
                srt_flush(done, sink);
            }
            cue = Some((start, end, Vec::new()));
        } else if let Some((_, _, lines)) = cue.as_mut()
            && lines.len() < MAX_RAW_LINES
        {
            lines.push(line);
        }
    }
    if let Some(done) = cue {
        srt_flush(done, sink);
    }
}

fn srt_flush((start, end, mut lines): (u64, u64, Vec<&str>), sink: &mut Sink) {
    trim_blank_tail(&mut lines);
    let digits = |l: &&str| !l.trim().is_empty() && l.trim().bytes().all(|b| b.is_ascii_digit());
    let numbered = lines.len() > 1
        && lines.last().is_some_and(digits)
        && lines
            .get(lines.len() - 2)
            .is_some_and(|l| l.trim().is_empty());
    if numbered {
        lines.pop();
        trim_blank_tail(&mut lines);
    }
    sink.push(start, end, &strip_markup(&lines.join("\n")));
}

fn trim_blank_tail(lines: &mut Vec<&str>) {
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
}

/// `WebVTT`: blocks separated by blank lines; a cue block has an optional identifier, a timing
/// line, and its text. `NOTE`, `STYLE`, and `REGION` blocks and the header are skipped.
fn webvtt(text: &str, sink: &mut Sink) {
    let mut block: Vec<&str> = Vec::new();
    // The header block (`WEBVTT` and its metadata) comes first.
    let mut header = true;
    for line in text.lines().chain(std::iter::once("")) {
        if header && line.contains("-->") {
            // No blank line after the header: the header ends at the first cue.
            header = false;
            block.clear();
        }
        if !line.trim().is_empty() {
            if block.len() < MAX_RAW_LINES {
                block.push(line);
            }
            continue;
        }
        if header {
            header = false;
        } else if let Some(at) = block.iter().take(2).position(|l| l.contains("-->"))
            && let Some((start, end)) = block.get(at).and_then(|l| timing(l))
        {
            let payload = block.get(at + 1..).unwrap_or_default().join("\n");
            sink.push(start, end, &decode_entities(&strip_markup(&payload)));
        }
        block.clear();
    }
}

/// ASS/SSA: `Dialogue:` lines in `[Events]`, with fields as named by its `Format:` line.
fn ass(text: &str, sink: &mut Sink) {
    // Layer/Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text.
    let mut format = (1, 2, 10);
    let mut events = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !events {
            continue;
        }
        if starts_with_ignore_case(line, "format:") {
            format = ass_format(line.get(7..).unwrap_or_default()).unwrap_or(format);
        } else if starts_with_ignore_case(line, "dialogue:") {
            let (start_at, end_at, count) = format;
            let fields: Vec<&str> = line
                .get(9..)
                .unwrap_or_default()
                .splitn(count, ',')
                .collect();
            if fields.len() != count {
                continue;
            }
            let time = |i: usize| fields.get(i).and_then(|f| timestamp(f.trim()));
            if let (Some(start), Some(end), Some(text)) =
                (time(start_at), time(end_at), fields.last())
            {
                sink.push(start, end, &ass_text(text));
            }
        }
    }
}

/// The positions of Start and End in a `Format:` line, and its field count. Text must be the
/// last field, as the format requires.
fn ass_format(fields: &str) -> Option<(usize, usize, usize)> {
    let names: Vec<String> = fields
        .split(',')
        .map(|f| f.trim().to_ascii_lowercase())
        .collect();
    if names.len() > 32 || names.last().map(String::as_str) != Some("text") {
        return None;
    }
    let at = |name: &str| names.iter().position(|n| n == name);
    Some((at("start")?, at("end")?, names.len()))
}

/// ASS dialogue text: override blocks (`{...}`) removed, drawings (`{\p1}...{\p0}`) dropped,
/// `\N` and `\n` as line breaks, `\h` as a space.
fn ass_text(text: &str) -> String {
    let mut out = String::new();
    let mut drawing = false;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '{' => {
                let rest = text.get(at + 1..).unwrap_or_default();
                if let Some((close, _)) = find_near(rest, &['}'], MAX_TAG_CHARS) {
                    let block = rest.get(..close).unwrap_or_default();
                    if let Some(scale) = drawing_scale(block) {
                        drawing = scale != 0;
                    }
                    // Skip the block and its closing brace.
                    while chars.next_if(|(i, _)| *i <= at + 1 + close).is_some() {}
                } else if !drawing {
                    out.push('{');
                }
            }
            '\\' => match chars.peek().map(|(_, n)| *n) {
                Some('N' | 'n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push(' ');
                }
                _ if !drawing => out.push('\\'),
                _ => {}
            },
            _ if !drawing => out.push(c),
            _ => {}
        }
    }
    out
}

/// The drawing scale set by `\p<n>` in an override block, if any.
fn drawing_scale(block: &str) -> Option<u32> {
    block
        .split('\\')
        .filter_map(|tag| tag.strip_prefix('p'))
        .filter_map(|n| n.trim().parse().ok())
        .next_back()
}

// --- Text cleaning -------------------------------------------------------------------------

/// Removes `<...>` tags (`<i>`, `</font>`, `<v Bob>`, `<00:01.000>`) and `{\...}` override
/// blocks. A `<` that does not start a short, well-formed tag stays text.
fn strip_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(['<', '{']) {
        out.push_str(rest.get(..at).unwrap_or_default());
        let tail = rest.get(at..).unwrap_or_default();
        let (open, close) = if tail.starts_with('<') {
            ('<', '>')
        } else {
            ('{', '}')
        };
        let inner = tail.get(1..).unwrap_or_default();
        let is_tag = match open {
            '<' => inner
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '/'),
            _ => inner.starts_with('\\'),
        };
        let end = find_near(inner, &[close, '\n', open], MAX_TAG_CHARS)
            .filter(|(_, c)| *c == close)
            .map(|(n, _)| n);
        match end {
            Some(n) if is_tag => rest = inner.get(n + 1..).unwrap_or_default(),
            _ => {
                out.push(open);
                rest = inner;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The byte offset and character of the first of `stops` within the first `limit` characters
/// of `text`. Bounding the search keeps markup removal linear on hostile input.
fn find_near(text: &str, stops: &[char], limit: usize) -> Option<(usize, char)> {
    text.char_indices()
        .take(limit + 1)
        .find(|(_, c)| stops.contains(c))
}

/// Decodes `WebVTT` character references: the named ones `WebVTT` uses and numeric ones.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let tail = rest.get(at + 1..).unwrap_or_default();
        let entity =
            find_near(tail, &[';'], 10).and_then(|(n, _)| Some((n, entity(tail.get(..n)?)?)));
        if let Some((n, c)) = entity {
            out.push(c);
            rest = tail.get(n + 1..).unwrap_or_default();
        } else {
            out.push('&');
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{A0}'),
        "lrm" => Some('\u{200E}'),
        "rlm" => Some('\u{200F}'),
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Collects cues: checks times, cleans text, enforces the limits.
#[derive(Default)]
struct Sink {
    cues: Vec<Cue>,
    truncated: bool,
}

impl Sink {
    fn push(&mut self, start_ms: u64, end_ms: u64, text: &str) {
        if end_ms <= start_ms {
            return;
        }
        let text = clean(text);
        if text.is_empty() {
            return;
        }
        if self.cues.len() == MAX_CUES {
            self.truncated = true;
            return;
        }
        self.cues.push(Cue {
            start_ms,
            end_ms,
            text,
        });
    }

    fn finish(mut self) -> (Vec<Cue>, bool) {
        self.cues.sort_by_key(|c| (c.start_ms, c.end_ms));
        // ASS files often repeat a line in several layers (outline, shadow).
        self.cues.dedup();
        (self.cues, self.truncated)
    }
}

/// Plain text: control characters dropped (tabs become spaces), lines trimmed, blank lines
/// removed, at most [`MAX_CUE_LINES`] lines and [`MAX_CUE_CHARS`] characters.
fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut chars = 0;
    for line in text.split('\n').take(MAX_CUE_LINES * 4) {
        let line: String = line
            .chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control() && *c != '\u{FEFF}')
            .collect();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if out.split('\n').count() == MAX_CUE_LINES && !out.is_empty() {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        for c in line.chars() {
            if chars == MAX_CUE_CHARS {
                out.push('…');
                return out;
            }
            out.push(c);
            chars += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn cues(text: &str) -> Vec<(u64, u64, String)> {
        parse_subtitles(text.as_bytes())
            .unwrap()
            .cues
            .into_iter()
            .map(|c| (c.start_ms, c.end_ms, c.text))
            .collect()
    }

    fn cue(start_ms: u64, end_ms: u64, text: &str) -> (u64, u64, String) {
        (start_ms, end_ms, text.to_owned())
    }

    #[test]
    fn parses_srt() {
        let srt = "1\n00:00:01,000 --> 00:00:02,500\nHello <i>there</i>.\n\n2\n00:00:03,000 --> 00:00:04,000 X1:10 X2:20\n{\\an8}Line one\nLine two\n\n\n3\n00:01:00.5 --> 00:01:01,25\n<font color=\"#fff\">1984</font>\n";
        assert_eq!(
            cues(srt),
            [
                cue(1000, 2500, "Hello there."),
                cue(3000, 4000, "Line one\nLine two"),
                cue(60_500, 61_250, "1984"),
            ]
        );
        assert_eq!(
            parse_subtitles(srt.as_bytes()).unwrap().format,
            SubtitleFormat::Srt
        );
    }

    #[test]
    fn srt_keeps_blank_lines_inside_text_and_odd_numbering() {
        let srt = "00:00:01,000 --> 00:00:02,000\nfirst part\n\nsecond part\n\n7\n00:00:03,000 --> 00:00:04,000\n<3 a < b > c\n";
        assert_eq!(
            cues(srt),
            [
                cue(1000, 2000, "first part\nsecond part"),
                cue(3000, 4000, "<3 a < b > c"),
            ]
        );
    }

    #[test]
    fn parses_webvtt() {
        let vtt = "WEBVTT - a title\nKind: captions\n\nNOTE this is\na comment\n\nSTYLE\n::cue { color: red }\n\nintro\n00:01.000 --> 00:02.000 align:start line:0\n<v Bob>Hi &amp; <c.loud>welcome</c> &lt;3 &#x263A;</v>\n\n01:00:00.000 --> 01:00:01.000\n<00:00:00.500><b>bold</b>\n";
        assert_eq!(
            cues(vtt),
            [
                cue(1000, 2000, "Hi & welcome <3 ☺"),
                cue(3_600_000, 3_601_000, "bold"),
            ]
        );
    }

    #[test]
    fn parses_ass_dialogue_text_only() {
        let ass = "[Script Info]\nTitle: x\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nComment: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,ignored\nDialogue: 0,0:00:01.50,0:00:03.00,Default,,0,0,0,,{\\i1}Hello{\\i0}, world\\Nnext\\hline\nDialogue: 1,0:00:01.50,0:00:03.00,Default,,0,0,0,,{\\i1}Hello{\\i0}, world\\Nnext\\hline\nDialogue: 0,0:00:00.10,0:00:00.90,Default,,0,0,0,,{\\p1}m 0 0 l 100 0{\\p0}shape done\nDialogue: 0,0:00:04.00,0:00:05.00,Default,,0,0,0,,{unclosed brace\nDialogue: broken line\n";
        assert_eq!(
            cues(ass),
            [
                cue(100, 900, "shape done"),
                cue(1500, 3000, "Hello, world\nnext line"),
                cue(4000, 5000, "{unclosed brace"),
            ],
            "layers deduplicated, sorted by time"
        );
        // SSA with a different field order.
        let ssa = "[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: Marked=0,0:00:02.00,0:00:03.00,Default,NTP,0000,0000,0000,!Effect,Text, with comma\n";
        assert_eq!(cues(ssa), [cue(2000, 3000, "Text, with comma")]);
    }

    #[test]
    fn decodes_text_encodings() {
        let srt = "00:00:01,000 --> 00:00:02,000\nCafé – “quoted”\n";
        let expect = [cue(1000, 2000, "Café – “quoted”")];
        // UTF-8 with and without a byte order mark.
        assert_eq!(cues(srt), expect);
        let bom = [b"\xEF\xBB\xBF".as_slice(), srt.as_bytes()].concat();
        let parsed = |bytes: &[u8]| -> Vec<(u64, u64, String)> {
            parse_subtitles(bytes)
                .unwrap()
                .cues
                .into_iter()
                .map(|c| (c.start_ms, c.end_ms, c.text))
                .collect()
        };
        assert_eq!(parsed(&bom), expect);
        // UTF-16 LE and BE, with and without a byte order mark.
        let le: Vec<u8> = srt.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let be: Vec<u8> = srt.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(parsed(&[&[0xFF, 0xFE][..], &le].concat()), expect);
        assert_eq!(parsed(&[&[0xFE, 0xFF][..], &be].concat()), expect);
        assert_eq!(parsed(&le), expect);
        assert_eq!(parsed(&be), expect);
        // Windows-1252.
        let cp1252 = b"00:00:01,000 --> 00:00:02,000\nCaf\xE9 \x96 \x93quoted\x94\n";
        assert_eq!(parsed(cp1252), expect);
    }

    #[test]
    fn cleans_and_bounds_text() {
        let long = "x".repeat(5000);
        let srt = format!(
            "00:00:01,000 --> 00:00:02,000\n{long}\n\n00:00:03,000 --> 00:00:04,000\na\x07b\tc\u{FEFF}\n1\n2\n3\n4\n5\n6\n7\n8\n9\n"
        );
        let got = cues(&srt);
        assert_eq!(got[0].2.chars().count(), MAX_CUE_CHARS + 1);
        assert!(got[0].2.ends_with('…'));
        assert_eq!(got[1].2.lines().next(), Some("ab c"));
        assert_eq!(got[1].2.lines().count(), MAX_CUE_LINES);
    }

    #[test]
    fn rejects_bad_times_and_files() {
        // End before start, minutes over 59, and a time past 100 hours are skipped.
        let srt = "00:00:05,000 --> 00:00:01,000\nbackwards\n\n00:61:00,000 --> 00:62:00,000\nbad\n\n101:00:00,000 --> 101:00:01,000\nlate\n\n00:00:01,000 --> 00:00:02,000\nok\n";
        assert_eq!(cues(srt), [cue(1000, 2000, "ok")]);
        assert_eq!(
            parse_subtitles(b"just some text\nwith no cues"),
            Err(SubtitleError::Unrecognized)
        );
        assert_eq!(parse_subtitles(b"WEBVTT\n\n"), Err(SubtitleError::Empty));
        assert_eq!(
            parse_subtitles(&vec![b'a'; MAX_SUBTITLE_BYTES + 1]),
            Err(SubtitleError::TooLarge)
        );
        assert_eq!(timestamp("1:02:03.456"), Some(3_723_456));
        assert_eq!(timestamp("02:03"), Some(123_000));
        assert_eq!(timestamp("1:2:3"), Some(3_723_000));
        assert_eq!(timestamp("00:00:01,123456"), Some(1123));
        for bad in [
            "",
            ":",
            "aa:bb",
            "1:2:3:4",
            "00:00:01,",
            "-1:00",
            "00:00:01,1e3",
        ] {
            assert_eq!(timestamp(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn caps_the_number_of_cues() {
        use std::fmt::Write as _;

        let mut srt = String::new();
        for i in 0..MAX_CUES + 10 {
            let s = i % 3600;
            let _ = write!(
                srt,
                "{}\n{:02}:{:02}:{:02},000 --> {:02}:{:02}:{:02},500\nline {i}\n\n",
                i + 1,
                i / 3600,
                s / 60,
                s % 60,
                i / 3600,
                s / 60,
                s % 60
            );
        }
        let parsed = parse_subtitles(srt.as_bytes()).unwrap();
        assert_eq!(parsed.cues.len(), MAX_CUES);
        assert!(parsed.truncated);
    }

    #[test]
    fn huge_single_line_is_linear() {
        // A 4 MiB line of `<` must not take quadratic time.
        let mut srt = "00:00:01,000 --> 00:00:02,000\n".to_owned();
        srt.push_str(&"<".repeat(4 * 1024 * 1024));
        let start = std::time::Instant::now();
        let parsed = parse_subtitles(srt.as_bytes()).unwrap();
        assert_eq!(parsed.cues[0].text.chars().count(), MAX_CUE_CHARS + 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn arbitrary_input_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2000)) {
            let _ = parse_subtitles(&bytes);
            let srt = [b"00:00:01,000 --> 00:00:02,000\n".as_slice(), &bytes].concat();
            let _ = parse_subtitles(&srt);
            let ass = [b"[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,,,,,,,".as_slice(), &bytes].concat();
            let _ = parse_subtitles(&ass);
            let vtt = [b"WEBVTT\n\n00:01.000 --> 00:02.000\n".as_slice(), &bytes].concat();
            let _ = parse_subtitles(&vtt);
        }

        #[test]
        fn markup_text_is_plain(text in "[a-z<>{}&;\\\\ /]{0,200}") {
            let srt = format!("00:00:01,000 --> 00:00:02,000\n{text}\n");
            if let Ok(parsed) = parse_subtitles(srt.as_bytes()) {
                for cue in parsed.cues {
                    prop_assert!(!cue.text.chars().any(char::is_control));
                }
            }
        }
    }
}
