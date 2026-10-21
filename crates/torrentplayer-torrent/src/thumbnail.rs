// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pictures inside torrents, shown as search-result thumbnails.
//!
//! Many torrents carry a small picture next to their videos: a poster, cover, folder image, or
//! screenshot (and Internet Archive torrents an `__ia_thumb.jpg`). [`choose_image`] picks the
//! most likely one: JPEG, PNG, or WebP only (never SVG, which can carry scripts), between
//! [`MIN_IMAGE_BYTES`] and [`MAX_IMAGE_BYTES`], whose pieces come to at most
//! [`MAX_PIECE_BYTES`], and never an indexer's advertising banner (`www.YTS.MX.jpg`,
//! `RARBG.com.jpg`). The picture is untrusted: [`image_type`] checks that the downloaded bytes
//! really are one of those formats before they reach the webview.

use torrentplayer_swarm::{MetaFile, Metainfo};

/// Largest picture file chosen.
pub const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024;
/// Smallest picture file chosen; smaller ones are icons.
pub const MIN_IMAGE_BYTES: u64 = 2 * 1024;
/// Most bytes downloaded for one thumbnail. Whole pieces are downloaded (and checked against
/// the torrent's hashes), so a picture in a torrent with large pieces can cost more than its
/// own size; such pictures are skipped.
pub const MAX_PIECE_BYTES: u64 = 8 * 1024 * 1024;

/// Words that mark an advertising banner rather than a picture of the content.
const AD_WORDS: &[&str] = &[
    "www",
    "yts",
    "yify",
    "rarbg",
    "1337x",
    "ettv",
    "eztv",
    "torrent",
    "torrents",
    "torrenting",
    "proxy",
    "proxies",
    "downloaded",
    "visit",
    "advert",
    "advertisement",
];
/// Top-level domains: a name ending in one (`Site.to.png`) is a banner too.
const DOMAIN_ENDINGS: &[&str] = &[
    "com", "net", "org", "to", "mx", "lt", "ag", "se", "is", "cc", "me", "io", "xyz", "info",
];
/// Names of pictures that show the content, and how much each is preferred.
const PREFERRED: &[(&str, i32)] = &[
    ("poster", 50),
    ("cover", 45),
    ("folder", 40),
    ("thumb", 35),
    ("thumbnail", 35),
    ("front", 25),
    ("screenshot", 20),
    ("screenshots", 20),
    ("screen", 15),
    ("screens", 15),
];

/// A picture format the webview can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    /// JPEG.
    Jpeg,
    /// PNG.
    Png,
    /// WebP.
    Webp,
}

impl ImageType {
    /// The MIME type.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
        }
    }

    fn of_extension(extension: &str) -> Option<Self> {
        match extension {
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "webp" => Some(Self::Webp),
            _ => None,
        }
    }
}

/// What the bytes of a file are, judged by their first bytes: `None` unless JPEG, PNG, or
/// WebP, whatever the file name says.
#[must_use]
pub fn image_type(bytes: &[u8]) -> Option<ImageType> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageType::Jpeg)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageType::Png)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP" {
        Some(ImageType::Webp)
    } else {
        None
    }
}

/// The torrent's best picture to show as its thumbnail (an index into `meta.files`), if it has
/// one within the limits.
#[must_use]
pub fn choose_image(meta: &Metainfo) -> Option<usize> {
    meta.files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.padding && (MIN_IMAGE_BYTES..=MAX_IMAGE_BYTES).contains(&f.len))
        .filter(|(_, f)| piece_bytes(meta, f) <= MAX_PIECE_BYTES)
        .filter_map(|(i, f)| score(f).map(|s| (i, s, f.path.join("/"))))
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.2.cmp(&a.2)))
        .map(|(i, _, _)| i)
}

/// How many bytes of whole pieces cover a file.
#[must_use]
pub fn piece_bytes(meta: &Metainfo, file: &MetaFile) -> u64 {
    let piece = u64::from(meta.piece_len).max(1);
    if file.len == 0 {
        return 0;
    }
    let first = file.offset / piece;
    let last = (file.offset + file.len - 1) / piece;
    (last - first + 1) * piece
}

/// How good a picture this file is for a thumbnail, or `None` if it is not one at all.
fn score(file: &MetaFile) -> Option<i32> {
    let name = file.path.last()?.to_lowercase();
    let (stem, extension) = name.rsplit_once('.')?;
    ImageType::of_extension(extension)?;
    if stem == "__ia_thumb" {
        return Some(100);
    }
    let words: Vec<&str> = stem
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let banner = words.iter().any(|w| AD_WORDS.contains(w))
        || (words.len() >= 2 && words.last().is_some_and(|w| DOMAIN_ENDINGS.contains(w)));
    if banner {
        return None;
    }
    let preferred = PREFERRED
        .iter()
        .filter(|(word, _)| words.contains(word))
        .map(|(_, points)| *points)
        .max()
        .unwrap_or(0);
    // Pictures deeper in the torrent's folders are less likely to show what it is about.
    let depth = i32::try_from(file.path.len().saturating_sub(1)).unwrap_or(i32::MAX);
    Some(preferred.saturating_sub(depth.saturating_mul(5)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, len: u64, offset: u64) -> MetaFile {
        MetaFile {
            path: path.split('/').map(str::to_owned).collect(),
            len,
            offset,
            padding: false,
        }
    }

    fn meta(files: Vec<MetaFile>, piece_len: u32) -> Metainfo {
        let mut m = Metainfo::from_bytes(
            b"d4:infod6:lengthi4e4:name1:a12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee",
        )
        .unwrap();
        m.total_len = files.iter().map(|f| f.offset + f.len).max().unwrap_or(0);
        m.files = files;
        m.piece_len = piece_len;
        m
    }

    fn chosen(files: Vec<MetaFile>) -> Option<String> {
        let m = meta(files, 16 * 1024);
        choose_image(&m).map(|i| m.files[i].path.join("/"))
    }

    #[test]
    fn prefers_posters_and_covers_and_skips_banners() {
        let kb = 1024;
        let picked = chosen(vec![
            file("Movie/Movie.2010.1080p.mp4", 900 * kb * kb, 0),
            file("Movie/www.YTS.MX.jpg", 50 * kb, 0),
            file("Movie/RARBG.com.png", 50 * kb, 0),
            file("Movie/Visit Site.to.png", 50 * kb, 0),
            file("Movie/Screens/screen1.jpg", 200 * kb, 0),
            file("Movie/poster.jpg", 300 * kb, 0),
        ]);
        assert_eq!(picked.as_deref(), Some("Movie/poster.jpg"));
        // Internet Archive's own thumbnail comes first.
        let picked = chosen(vec![
            file("item/cover.jpg", 300 * kb, 0),
            file("item/__ia_thumb.jpg", 20 * kb, 0),
        ]);
        assert_eq!(picked.as_deref(), Some("item/__ia_thumb.jpg"));
        // Any other picture will do when there is no better one; banners never.
        assert_eq!(
            chosen(vec![file("Lecture/slide 01.png", 80 * kb, 0)]).as_deref(),
            Some("Lecture/slide 01.png")
        );
        assert_eq!(chosen(vec![file("a/www.YTS.MX.jpg", 50 * kb, 0)]), None);
    }

    #[test]
    fn only_small_jpeg_png_or_webp_files() {
        let kb = 1024;
        assert_eq!(
            chosen(vec![file("a/poster.svg", 50 * kb, 0)]),
            None,
            "SVG can carry scripts"
        );
        assert_eq!(chosen(vec![file("a/poster.gif", 50 * kb, 0)]), None);
        assert_eq!(chosen(vec![file("a/poster.bmp", 50 * kb, 0)]), None);
        assert_eq!(
            chosen(vec![file("a/poster.jpg", MAX_IMAGE_BYTES + 1, 0)]),
            None
        );
        assert_eq!(
            chosen(vec![file("a/poster.jpg", MIN_IMAGE_BYTES - 1, 0)]),
            None,
            "an icon"
        );
        assert!(chosen(vec![file("a/POSTER.JPEG", 50 * kb, 0)]).is_some());
        assert!(chosen(vec![file("a/cover.webp", 50 * kb, 0)]).is_some());
        let mut padding = file("a/poster.jpg", 50 * kb, 0);
        padding.padding = true;
        assert_eq!(chosen(vec![padding]), None);
    }

    #[test]
    fn skips_pictures_whose_pieces_cost_too_much() {
        let kb = 1024;
        // 16 MiB pieces: the picture alone would cost a whole piece.
        let big_pieces = meta(vec![file("a/poster.jpg", 100 * kb, 0)], 16 * 1024 * 1024);
        assert_eq!(choose_image(&big_pieces), None);
        // A picture straddling two 4 MiB pieces costs 8 MiB: just within the limit.
        let straddling = meta(
            vec![file("a/poster.jpg", 100 * kb, 4 * 1024 * 1024 - 1)],
            4 * 1024 * 1024,
        );
        assert_eq!(
            piece_bytes(&straddling, &straddling.files[0]),
            MAX_PIECE_BYTES
        );
        assert_eq!(choose_image(&straddling), Some(0));
    }

    #[test]
    fn checks_the_bytes_not_the_name() {
        assert_eq!(
            image_type(&[0xFF, 0xD8, 0xFF, 0xE0, 0]),
            Some(ImageType::Jpeg)
        );
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n...."), Some(ImageType::Png));
        assert_eq!(image_type(b"RIFF\x10\0\0\0WEBPVP8 "), Some(ImageType::Webp));
        assert_eq!(
            image_type(b"<svg xmlns=\"http://www.w3.org/2000/svg\">"),
            None
        );
        assert_eq!(image_type(b"GIF89a"), None);
        assert_eq!(image_type(b"RIFF\x10\0\0\0WAVE"), None);
        assert_eq!(image_type(b""), None);
        assert_eq!(ImageType::Png.mime(), "image/png");
    }
}
