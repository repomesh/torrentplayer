// SPDX-License-Identifier: MIT OR Apache-2.0

//! Search query validation.

use std::fmt;

use crate::SearchError;

/// Longest query accepted, in characters (the emberglow API's own limit).
pub const MAX_QUERY_CHARS: usize = 200;
/// Most categories a search can be limited to.
pub const MAX_CATEGORIES: usize = 4;

/// A validated search query: trimmed, 1–200 characters, no control characters, optionally
/// limited to a few Torznab categories.
#[derive(Clone, PartialEq, Eq)]
pub struct Query {
    text: String,
    /// Torznab category ids, sorted and without duplicates; empty for all categories.
    categories: Vec<u32>,
}

impl Query {
    /// Validates user input.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::InvalidQuery`] if the trimmed text is empty, longer than
    /// [`MAX_QUERY_CHARS`], or contains control characters.
    pub fn parse(text: &str) -> Result<Self, SearchError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(SearchError::InvalidQuery(
                "Type something to search for.".into(),
            ));
        }
        if text.chars().count() > MAX_QUERY_CHARS {
            return Err(SearchError::InvalidQuery(format!(
                "Searches can be at most {MAX_QUERY_CHARS} characters."
            )));
        }
        if text.chars().any(char::is_control) {
            return Err(SearchError::InvalidQuery(
                "Searches cannot contain control characters.".into(),
            ));
        }
        Ok(Self {
            text: text.to_owned(),
            categories: Vec::new(),
        })
    }

    /// Limits the search to these Torznab categories (for example 2000 Movies, 5000 TV, 5070
    /// Anime); none means all of them. Adult categories (6000–6999) are refused, whatever the
    /// adult filter says: the filter's job is to hide them, never to search for them.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::InvalidQuery`] for an id outside 1000–8999, an adult id, or more
    /// than [`MAX_CATEGORIES`] ids.
    pub fn with_categories(mut self, ids: &[u32]) -> Result<Self, SearchError> {
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() > MAX_CATEGORIES {
            return Err(SearchError::InvalidQuery(format!(
                "Search at most {MAX_CATEGORIES} categories at once."
            )));
        }
        if ids
            .iter()
            .any(|id| !(1000..=8999).contains(id) || (6000..=6999).contains(id))
        {
            return Err(SearchError::InvalidQuery(
                "That is not a category TorrentPlayer can search.".into(),
            ));
        }
        self.categories = ids;
        Ok(self)
    }

    /// The query text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The Torznab categories the search is limited to (sorted); empty for all.
    #[must_use]
    pub fn categories(&self) -> &[u32] {
        &self.categories
    }
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Queries can be personal; keep them out of debug logs.
        write!(
            f,
            "Query({} chars, categories {:?})",
            self.text.chars().count(),
            self.categories
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_accepts_ordinary_queries() {
        assert_eq!(Query::parse("  mit 6.006 ").unwrap().as_str(), "mit 6.006");
        assert!(Query::parse("Résumé — 講義").is_ok());
        assert!(Query::parse(&"a".repeat(MAX_QUERY_CHARS)).is_ok());
    }

    #[test]
    fn rejects_empty_long_and_control_characters() {
        for bad in ["", "   ", "\t\n"] {
            assert!(matches!(
                Query::parse(bad),
                Err(SearchError::InvalidQuery(_))
            ));
        }
        assert!(Query::parse(&"a".repeat(MAX_QUERY_CHARS + 1)).is_err());
        assert!(Query::parse("ubuntu\u{0}iso").is_err());
        assert!(Query::parse("a\u{7f}b").is_err());
    }

    #[test]
    fn limits_to_a_few_non_adult_categories() {
        let q = Query::parse("sintel").unwrap();
        assert!(q.categories().is_empty());
        let movies = q.clone().with_categories(&[5000, 2000, 2000]).unwrap();
        assert_eq!(
            movies.categories(),
            [2000, 5000],
            "sorted, without duplicates"
        );
        assert!(
            q.clone()
                .with_categories(&[])
                .unwrap()
                .categories()
                .is_empty()
        );
        for bad in [
            &[6000][..],
            &[6010],
            &[999],
            &[9000],
            &[0],
            &[1000, 2000, 3000, 4000, 5000],
        ] {
            assert!(
                matches!(
                    q.clone().with_categories(bad),
                    Err(SearchError::InvalidQuery(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn debug_hides_the_text() {
        let q = Query::parse("something private").unwrap();
        assert!(!format!("{q:?}").contains("private"));
    }
}
