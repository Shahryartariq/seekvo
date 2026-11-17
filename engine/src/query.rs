//! Turning what a person typed into something the index can search for.

/// Where a query's words are allowed to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// Every word must appear somewhere in the path, and at least one word
    /// must appear in the entry's own name. Searching for a folder name
    /// returns that folder, not everything inside it.
    #[default]
    Smart,
    /// Every word must appear somewhere in the path. Searching for a folder
    /// name returns the folder and everything inside it.
    Path,
}

/// A parsed search query.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    /// Lowercase words. All of them must match.
    pub terms: Vec<String>,
    /// Lowercase extension from an `ext:` filter, without the dot.
    pub ext: Option<String>,
    pub scope: Scope,
}

impl Query {
    /// Parse a line of text.
    ///
    /// Words are separated by spaces and compared without regard to case.
    /// `ext:pdf` (or `ext:.pdf`) keeps only files with that extension. A word
    /// that contains a path separator is split into one word per path part,
    /// so `finance\2026` means "under something called finance, and 2026".
    pub fn parse(raw: &str) -> Self {
        let mut terms: Vec<String> = Vec::new();
        let mut ext = None;
        for word in raw.split_whitespace() {
            let word = word.to_lowercase();
            if let Some(value) = word.strip_prefix("ext:") {
                let value = value.trim_start_matches('.');
                if !value.is_empty() {
                    ext = Some(value.to_owned());
                }
                continue;
            }
            for part in word.split(['\\', '/']) {
                // A NUL can never be part of a file name, and the index uses
                // it as a separator, so a word containing one matches nothing
                // useful. Dropping it keeps the search code simple.
                if part.is_empty() || part.contains('\0') {
                    continue;
                }
                if !terms.iter().any(|existing| existing == part) {
                    terms.push(part.to_owned());
                }
            }
        }
        Query {
            terms,
            ext,
            scope: Scope::default(),
        }
    }

    /// The same query with a different scope.
    pub fn with_scope(mut self, scope: Scope) -> Self {
        self.scope = scope;
        self
    }

    /// True when there is nothing to search for.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.ext.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_words_and_lowercases_them() {
        let query = Query::parse("  Invoice   MARCH ");
        assert_eq!(query.terms, ["invoice", "march"]);
        assert_eq!(query.ext, None);
        assert_eq!(query.scope, Scope::Smart);
    }

    #[test]
    fn reads_an_ext_filter_with_or_without_a_dot() {
        assert_eq!(Query::parse("report ext:PDF").ext.as_deref(), Some("pdf"));
        assert_eq!(Query::parse("ext:.mp4").ext.as_deref(), Some("mp4"));
        assert!(Query::parse("ext:.mp4").terms.is_empty());
    }

    #[test]
    fn ignores_an_ext_filter_with_no_value() {
        assert!(Query::parse("ext:").is_empty());
        assert!(Query::parse("   ").is_empty());
    }

    #[test]
    fn splits_a_word_on_path_separators() {
        assert_eq!(Query::parse(r"Finance\2026").terms, ["finance", "2026"]);
        assert_eq!(Query::parse("src/lib/").terms, ["src", "lib"]);
    }

    #[test]
    fn drops_repeated_words() {
        assert_eq!(Query::parse("img IMG img").terms, ["img"]);
    }

    #[test]
    fn lowercases_beyond_ascii() {
        assert_eq!(Query::parse("ÜBER Straße").terms, ["über", "straße"]);
    }
}
