// SPDX-License-Identifier: MIT OR Apache-2.0

//! Languages named in subtitle file names (`Movie.en.srt`, `2_English.srt`) and track tags
//! (`eng`, `pt-BR`).

/// A language: its BCP 47 tag (for `<track srclang>`) and English name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Language {
    /// BCP 47 tag, for example `en` or `pt-BR`.
    pub code: &'static str,
    /// English name, for example `Portuguese (Brazil)`.
    pub name: &'static str,
}

/// Names and codes, lowercase: two-letter ISO 639-1, three-letter ISO 639-2 (both B and T),
/// and English and native names. Two-letter codes that are common English words (`is`, `no`,
/// `hi`, `id`, `ms`, `he`) are left out; their three-letter codes and names remain.
const LANGUAGES: &[(&[&str], &str, &str)] = &[
    (&["en", "eng", "english"], "en", "English"),
    (
        &["fr", "fre", "fra", "french", "francais", "français"],
        "fr",
        "French",
    ),
    (&["de", "ger", "deu", "german", "deutsch"], "de", "German"),
    (
        &["es", "spa", "spanish", "espanol", "español", "castellano"],
        "es",
        "Spanish",
    ),
    (
        &["es-419", "lat", "latino", "latam"],
        "es-419",
        "Spanish (Latin America)",
    ),
    (&["it", "ita", "italian", "italiano"], "it", "Italian"),
    (
        &["pt", "por", "portuguese", "portugues", "português"],
        "pt",
        "Portuguese",
    ),
    (
        &["pt-br", "ptbr", "pob", "brazilian"],
        "pt-BR",
        "Portuguese (Brazil)",
    ),
    (&["nl", "dut", "nld", "dutch", "nederlands"], "nl", "Dutch"),
    (&["ru", "rus", "russian"], "ru", "Russian"),
    (&["uk", "ukr", "ukrainian"], "uk", "Ukrainian"),
    (&["pl", "pol", "polish", "polski"], "pl", "Polish"),
    (&["cs", "cze", "ces", "czech"], "cs", "Czech"),
    (&["sk", "slo", "slk", "slovak"], "sk", "Slovak"),
    (&["sl", "slv", "slovenian", "slovene"], "sl", "Slovenian"),
    (&["hr", "hrv", "croatian"], "hr", "Croatian"),
    (&["sr", "srp", "serbian"], "sr", "Serbian"),
    (&["bg", "bul", "bulgarian"], "bg", "Bulgarian"),
    (&["ro", "rum", "ron", "romanian"], "ro", "Romanian"),
    (&["hu", "hun", "hungarian", "magyar"], "hu", "Hungarian"),
    (&["el", "gre", "ell", "greek"], "el", "Greek"),
    (&["tr", "tur", "turkish"], "tr", "Turkish"),
    (&["sv", "swe", "swedish", "svenska"], "sv", "Swedish"),
    (&["da", "dan", "danish", "dansk"], "da", "Danish"),
    (
        &["nb", "nor", "nob", "norwegian", "norsk"],
        "no",
        "Norwegian",
    ),
    (&["fi", "fin", "finnish", "suomi"], "fi", "Finnish"),
    (&["ice", "isl", "icelandic"], "is", "Icelandic"),
    (&["et", "est", "estonian"], "et", "Estonian"),
    (&["lv", "lav", "latvian"], "lv", "Latvian"),
    (&["lt", "lit", "lithuanian"], "lt", "Lithuanian"),
    (&["ca", "cat", "catalan"], "ca", "Catalan"),
    (&["eu", "baq", "eus", "basque"], "eu", "Basque"),
    (&["gl", "glg", "galician"], "gl", "Galician"),
    (&["ja", "jpn", "japanese"], "ja", "Japanese"),
    (&["zh", "chi", "zho", "chinese"], "zh", "Chinese"),
    (
        &["chs", "zh-hans", "zh-cn", "simplified"],
        "zh-Hans",
        "Chinese (Simplified)",
    ),
    (
        &["cht", "zh-hant", "zh-tw", "zh-hk", "traditional"],
        "zh-Hant",
        "Chinese (Traditional)",
    ),
    (&["ko", "kor", "korean"], "ko", "Korean"),
    (&["ar", "ara", "arabic"], "ar", "Arabic"),
    (&["heb", "iw", "hebrew"], "he", "Hebrew"),
    (&["fa", "per", "fas", "persian", "farsi"], "fa", "Persian"),
    (&["hin", "hindi"], "hi", "Hindi"),
    (&["bn", "ben", "bengali"], "bn", "Bengali"),
    (&["ta", "tam", "tamil"], "ta", "Tamil"),
    (&["te", "tel", "telugu"], "te", "Telugu"),
    (&["ur", "urd", "urdu"], "ur", "Urdu"),
    (&["th", "tha", "thai"], "th", "Thai"),
    (&["vi", "vie", "vietnamese"], "vi", "Vietnamese"),
    (&["ind", "indonesian"], "id", "Indonesian"),
    (&["may", "msa", "malay"], "ms", "Malay"),
    (
        &["fil", "tl", "tgl", "filipino", "tagalog"],
        "fil",
        "Filipino",
    ),
];

/// The language a word or tag names, ignoring case: `en`, `ENG`, `English`, `pt-BR`. A tag
/// with a region falls back to its language (`de-CH` is German).
#[must_use]
pub fn language(word: &str) -> Option<Language> {
    let word = word.to_lowercase();
    let find = |w: &str| {
        LANGUAGES
            .iter()
            .find(|(names, _, _)| names.contains(&w))
            .map(|&(_, code, name)| Language { code, name })
    };
    find(&word).or_else(|| find(word.split_once('-')?.0))
}

/// Whether `word` is a two- or three-letter code (rather than a name). Codes count only as the
/// last word of a file name, since short words like `it` or `de` also occur in titles.
pub(crate) fn is_code(word: &str) -> bool {
    let primary = word.split_once('-').map_or(word, |(p, _)| p);
    primary.chars().count() <= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_languages_by_code_and_name() {
        for (word, code) in [
            ("en", "en"),
            ("ENG", "en"),
            ("English", "en"),
            ("fre", "fr"),
            ("Français", "fr"),
            ("pt-BR", "pt-BR"),
            ("de-CH", "de"),
            ("chs", "zh-Hans"),
            ("nob", "no"),
        ] {
            assert_eq!(language(word).map(|l| l.code), Some(code), "{word}");
        }
        assert_eq!(
            language("pt-BR").map(|l| l.name),
            Some("Portuguese (Brazil)")
        );
        for word in ["is", "no", "hi", "xx", "", "-", "forced"] {
            assert_eq!(language(word), None, "{word}");
        }
        assert!(is_code("en") && is_code("pt-br") && !is_code("english"));
    }
}
