// SPDX-License-Identifier: MIT OR Apache-2.0

//! Validation of file paths that come from torrent metadata.
//!
//! Torrent metadata is attacker-controlled. librqbit already rejects `..` components and
//! embedded separators; this module adds the rest of the rules needed so that a file can never
//! be written outside the download directory or alias another file, on any OS. Torrents that
//! break a rule are refused rather than silently renamed, so what the user sees is what gets
//! written.

/// Longest single path component, in bytes. Matches the common filesystem limit.
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Most path components allowed in one file path.
pub const MAX_COMPONENTS: usize = 64;

/// Why a path from torrent metadata was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnsafePath {
    /// The path has no components.
    #[error("the path is empty")]
    Empty,
    /// The path has more than [`MAX_COMPONENTS`] components.
    #[error("the path is nested too deeply")]
    TooDeep,
    /// A component is empty, `.`, or `..`.
    #[error("the path contains a relative component ({0:?})")]
    Relative(String),
    /// A component is longer than [`MAX_COMPONENT_BYTES`].
    #[error("a path component is too long")]
    TooLong,
    /// A component contains a separator, a drive or stream marker (`:`), or a control
    /// character.
    #[error("a path component contains a forbidden character ({0:?})")]
    ForbiddenCharacter(String),
    /// A component ends with a dot or space, which Windows silently strips, so two different
    /// names could map to the same file.
    #[error("a path component ends with a dot or space ({0:?})")]
    TrailingDotOrSpace(String),
    /// A component is a Windows device name such as `CON` or `COM1`, which refer to devices
    /// rather than files.
    #[error("a path component is a reserved device name ({0:?})")]
    ReservedName(String),
}

/// Checks that every component of a relative path is safe to create on Windows, macOS, and
/// Linux inside the download directory.
///
/// # Errors
///
/// Returns the first rule the path breaks.
pub fn validate_components<S: AsRef<str>>(components: &[S]) -> Result<(), UnsafePath> {
    if components.is_empty() {
        return Err(UnsafePath::Empty);
    }
    if components.len() > MAX_COMPONENTS {
        return Err(UnsafePath::TooDeep);
    }
    components
        .iter()
        .try_for_each(|c| validate_component(c.as_ref()))
}

fn validate_component(c: &str) -> Result<(), UnsafePath> {
    if c.is_empty() || c == "." || c == ".." {
        return Err(UnsafePath::Relative(c.to_owned()));
    }
    if c.len() > MAX_COMPONENT_BYTES {
        return Err(UnsafePath::TooLong);
    }
    if c.chars().any(|ch| {
        matches!(ch, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') || ch.is_control()
    }) {
        return Err(UnsafePath::ForbiddenCharacter(c.to_owned()));
    }
    if c.ends_with('.') || c.ends_with(' ') {
        return Err(UnsafePath::TrailingDotOrSpace(c.to_owned()));
    }
    if is_reserved_windows_name(c) {
        return Err(UnsafePath::ReservedName(c.to_owned()));
    }
    Ok(())
}

/// Windows treats these names as devices regardless of case and extension (`con.txt` too).
fn is_reserved_windows_name(c: &str) -> bool {
    let stem = c.split('.').next().unwrap_or(c).trim_end();
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            // `get` rather than slicing: the name may contain multi-byte characters.
            let numbered = matches!(
                (upper.get(..3), upper.get(3..)),
                (
                    Some("COM" | "LPT"),
                    Some("0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                )
            );
            numbered || is_superscript_device(stem)
        }
    }
}

/// `COM¹`, `LPT²`, and `COM³`-style names are also devices on Windows.
fn is_superscript_device(stem: &str) -> bool {
    let mut chars = stem.chars();
    let prefix: String = chars
        .by_ref()
        .take(3)
        .collect::<String>()
        .to_ascii_uppercase();
    let rest: String = chars.collect();
    (prefix == "COM" || prefix == "LPT") && matches!(rest.as_str(), "¹" | "²" | "³")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn check(path: &str) -> Result<(), UnsafePath> {
        let parts: Vec<&str> = path.split('/').collect();
        validate_components(&parts)
    }

    #[test]
    fn accepts_ordinary_lecture_paths() {
        for ok in [
            "MIT 6.006/Lecture 01 - Introduction.mp4",
            "lecture.mkv",
            "Courses/18.03 Differential Equations/Subtitles/lec01.en.srt",
            "Résumé — notes (2024).pdf",
            "コンピュータ/講義1.mp4",
            ".hidden",
            "CONTENT.mp4",
            "console.log",
            "COM10.txt",
            "LPT.txt",
            "abé.mp4",
            "coé",
        ] {
            assert_eq!(check(ok), Ok(()), "{ok}");
        }
    }

    #[test]
    fn rejects_traversal_and_relative_components() {
        assert_eq!(validate_components::<&str>(&[]), Err(UnsafePath::Empty));
        for bad in ["../evil", "a/../b", "./a", "a//b", "a/."] {
            assert!(matches!(check(bad), Err(UnsafePath::Relative(_))), "{bad}");
        }
    }

    #[test]
    fn rejects_drive_letters_streams_and_separators() {
        for bad in [
            "C:",
            "C:evil",
            "file.txt:stream",
            "a\\..\\b",
            "nul\0byte",
            "tab\there",
            "what?",
            "star*",
            "pipe|",
            "quote\"",
            "<angle>",
        ] {
            assert!(
                matches!(check(bad), Err(UnsafePath::ForbiddenCharacter(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn rejects_windows_aliasing_and_devices() {
        for bad in ["name.", "name ", "folder./file"] {
            assert!(
                matches!(check(bad), Err(UnsafePath::TrailingDotOrSpace(_))),
                "{bad:?}"
            );
        }
        for bad in [
            "CON", "con", "Con.txt", "aux.mp4", "NUL", "com1", "COM9.log", "lpt3", "LPT0",
            "CONIN$", "conout$", "COM¹", "lpt²", "AUX .txt",
        ] {
            assert!(
                matches!(check(bad), Err(UnsafePath::ReservedName(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn rejects_oversized_paths() {
        assert_eq!(check(&"a".repeat(256)), Err(UnsafePath::TooLong));
        assert_eq!(check(&"a".repeat(255)), Ok(()));
        let deep = vec!["d"; MAX_COMPONENTS + 1];
        assert_eq!(validate_components(&deep), Err(UnsafePath::TooDeep));
    }

    proptest! {
        /// Whatever the input, an accepted path joined under a root stays under that root.
        #[test]
        fn accepted_paths_stay_inside_the_root(parts in proptest::collection::vec(".{0,12}", 1..6)) {
            if validate_components(&parts).is_ok() {
                let root = std::path::Path::new("root");
                let mut joined = root.to_path_buf();
                for p in &parts {
                    joined.push(p);
                }
                prop_assert!(joined.starts_with(root));
                prop_assert!(joined.components().all(|c| matches!(c, std::path::Component::Normal(_))));
                prop_assert_eq!(joined.components().count(), parts.len() + 1);
            }
        }
    }
}
