//! The in-memory file index.
//!
//! Every file and folder is one entry. Entries live in a handful of flat
//! arrays (one per field) instead of one heap object each, and all the names
//! sit end to end in a single buffer. A search is then a straight run through
//! memory the CPU can prefetch, with no pointers to chase.

use std::collections::HashMap;
use std::fmt;
use std::path::MAIN_SEPARATOR;

use memchr::memmem;

use crate::query::{Query, Scope};

/// Position of an entry in its index.
pub type EntryId = u32;

const NO_PARENT: u32 = u32::MAX;
const NO_EXT: u32 = 0;

/// Whether an entry is a file or a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

/// Why an entry could not be added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// The parent id was not returned by this builder.
    UnknownParent(EntryId),
    /// The parent is a file, and files cannot contain anything.
    ParentIsFile(EntryId),
    /// The index is full: it holds about four billion entries or four
    /// gigabytes of names.
    TooLarge,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::UnknownParent(id) => write!(f, "parent entry {id} does not exist"),
            BuildError::ParentIsFile(id) => write!(f, "parent entry {id} is a file"),
            BuildError::TooLarge => write!(f, "the index is full"),
        }
    }
}

impl std::error::Error for BuildError {}

/// Collects entries, parents before children, and turns them into an
/// [`Index`].
#[derive(Debug)]
pub struct IndexBuilder {
    names: String,
    lower: Vec<u8>,
    name_start: Vec<u32>,
    lower_start: Vec<u32>,
    parent: Vec<u32>,
    size: Vec<u64>,
    ext: Vec<u32>,
    is_dir: Vec<bool>,
    exts: Vec<String>,
    ext_ids: HashMap<String, u32>,
}

impl Default for IndexBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexBuilder {
    pub fn new() -> Self {
        IndexBuilder {
            names: String::new(),
            lower: Vec::new(),
            name_start: vec![0],
            lower_start: vec![0],
            parent: Vec::new(),
            size: Vec::new(),
            ext: Vec::new(),
            is_dir: Vec::new(),
            // Id 0 means "no extension".
            exts: vec![String::new()],
            ext_ids: HashMap::new(),
        }
    }

    /// Number of entries added so far.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Add a file or folder and return its id.
    ///
    /// `parent` is `None` for a root such as `C:\`. Because a child can only
    /// name a parent that already has an id, parents always come before
    /// their children; the search relies on that order.
    pub fn add(
        &mut self,
        parent: Option<EntryId>,
        name: &str,
        kind: Kind,
        size: u64,
    ) -> Result<EntryId, BuildError> {
        let id = u32::try_from(self.len()).map_err(|_| BuildError::TooLarge)?;
        if id == NO_PARENT {
            return Err(BuildError::TooLarge);
        }
        if let Some(parent) = parent {
            match self.is_dir.get(parent as usize) {
                None => return Err(BuildError::UnknownParent(parent)),
                Some(false) => return Err(BuildError::ParentIsFile(parent)),
                Some(true) => {}
            }
        }

        let lowered;
        let lower_name: &str = if name
            .bytes()
            .any(|byte| !byte.is_ascii() || byte.is_ascii_uppercase())
        {
            lowered = name.to_lowercase();
            &lowered
        } else {
            name
        };

        // Work out both end offsets before touching anything, so a full
        // index is left exactly as it was.
        let name_end = self.names.len().checked_add(name.len());
        let lower_end = self
            .lower
            .len()
            .checked_add(lower_name.len())
            .and_then(|end| end.checked_add(1));
        let (Some(name_end), Some(lower_end)) = (name_end, lower_end) else {
            return Err(BuildError::TooLarge);
        };
        let name_end = u32::try_from(name_end).map_err(|_| BuildError::TooLarge)?;
        let lower_end = u32::try_from(lower_end).map_err(|_| BuildError::TooLarge)?;

        let ext = match kind {
            Kind::File => self.ext_id_for(lower_name),
            Kind::Dir => NO_EXT,
        };

        self.names.push_str(name);
        self.lower.extend_from_slice(lower_name.as_bytes());
        // A NUL after every name keeps a match from running across two names.
        self.lower.push(0);
        self.name_start.push(name_end);
        self.lower_start.push(lower_end);
        self.parent.push(parent.unwrap_or(NO_PARENT));
        self.size.push(size);
        self.ext.push(ext);
        self.is_dir.push(kind == Kind::Dir);
        Ok(id)
    }

    fn ext_id_for(&mut self, lower_name: &str) -> u32 {
        let Some(ext) = extension_of(lower_name) else {
            return NO_EXT;
        };
        if let Some(&id) = self.ext_ids.get(ext) {
            return id;
        }
        let Ok(id) = u32::try_from(self.exts.len()) else {
            return NO_EXT;
        };
        self.exts.push(ext.to_owned());
        self.ext_ids.insert(ext.to_owned(), id);
        id
    }

    /// Finish building.
    pub fn finish(self) -> Index {
        Index {
            names: self.names.into_boxed_str(),
            lower: self.lower.into_boxed_slice(),
            name_start: self.name_start.into_boxed_slice(),
            lower_start: self.lower_start.into_boxed_slice(),
            parent: self.parent.into_boxed_slice(),
            size: self.size.into_boxed_slice(),
            ext: self.ext.into_boxed_slice(),
            is_dir: self.is_dir.into_boxed_slice(),
            exts: self.exts,
            ext_ids: self.ext_ids,
        }
    }
}

/// The part of a file name after its last dot, if it has one.
/// `.gitignore` and `notes.` have no extension.
fn extension_of(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    if dot == 0 || dot + 1 == name.len() {
        return None;
    }
    Some(&name[dot + 1..])
}

/// What a search found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResult {
    /// How many entries match in total.
    pub total: usize,
    /// The first matches, in index order, up to the limit that was asked for.
    pub hits: Vec<EntryId>,
}

/// A searchable, read-only set of files and folders.
#[derive(Debug)]
pub struct Index {
    /// Every name as given, end to end.
    names: Box<str>,
    /// Every name in lowercase, each followed by a NUL byte.
    lower: Box<[u8]>,
    /// Where each name starts in `names`; one extra element marks the end.
    name_start: Box<[u32]>,
    /// Where each name starts in `lower`; one extra element marks the end.
    lower_start: Box<[u32]>,
    parent: Box<[u32]>,
    size: Box<[u64]>,
    ext: Box<[u32]>,
    is_dir: Box<[bool]>,
    exts: Vec<String>,
    ext_ids: HashMap<String, u32>,
}

impl Index {
    /// Number of entries.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// The entry's own name, as it was given. Returns `None` for an id that
    /// is not in this index.
    pub fn name(&self, id: EntryId) -> Option<&str> {
        let i = id as usize;
        let start = *self.name_start.get(i)? as usize;
        let end = *self.name_start.get(i + 1)? as usize;
        self.names.get(start..end)
    }

    /// The entry's parent folder, or `None` for a root or an unknown id.
    pub fn parent(&self, id: EntryId) -> Option<EntryId> {
        match self.parent.get(id as usize) {
            Some(&parent) if parent != NO_PARENT => Some(parent),
            _ => None,
        }
    }

    /// Size in bytes, or `None` for an unknown id.
    pub fn size(&self, id: EntryId) -> Option<u64> {
        self.size.get(id as usize).copied()
    }

    pub fn kind(&self, id: EntryId) -> Option<Kind> {
        self.is_dir
            .get(id as usize)
            .map(|&is_dir| if is_dir { Kind::Dir } else { Kind::File })
    }

    /// The lowercase extension of a file, without the dot.
    pub fn extension(&self, id: EntryId) -> Option<&str> {
        match self.ext.get(id as usize) {
            Some(&ext) if ext != NO_EXT => self.exts.get(ext as usize).map(String::as_str),
            _ => None,
        }
    }

    /// The full path, built by walking up through the parents.
    pub fn path(&self, id: EntryId) -> Option<String> {
        let mut chain = vec![id];
        let mut length = self.name(id)?.len();
        let mut current = id;
        while let Some(parent) = self.parent(current) {
            length += self.name(parent)?.len() + 1;
            chain.push(parent);
            current = parent;
        }
        let mut path = String::with_capacity(length);
        for &part in chain.iter().rev() {
            if !path.is_empty() && !path.ends_with(['/', '\\']) {
                path.push(MAIN_SEPARATOR);
            }
            path.push_str(self.name(part)?);
        }
        Some(path)
    }

    /// Bytes of memory the index holds, not counting the small extension
    /// table.
    pub fn memory_bytes(&self) -> usize {
        self.names.len()
            + self.lower.len()
            + std::mem::size_of_val(&*self.name_start)
            + std::mem::size_of_val(&*self.lower_start)
            + std::mem::size_of_val(&*self.parent)
            + std::mem::size_of_val(&*self.size)
            + std::mem::size_of_val(&*self.ext)
            + std::mem::size_of_val(&*self.is_dir)
    }

    /// Find the entries that match `query`. Counts every match and returns
    /// the first `limit` of them.
    pub fn search(&self, query: &Query, limit: usize) -> SearchResult {
        if query.is_empty() {
            return SearchResult::default();
        }
        let wanted_ext = match &query.ext {
            None => None,
            Some(ext) => match self.ext_ids.get(ext) {
                Some(&id) => Some(id),
                // No file in the index has this extension.
                None => return SearchResult::default(),
            },
        };

        let n = self.len();
        let smart = query.scope == Scope::Smart;
        // in_path[i]: every word so far appears somewhere in entry i's path.
        let mut in_path: Vec<bool> = Vec::new();
        // in_name[i]: at least one word appears in entry i's own name.
        let mut in_name = vec![false; if smart { n } else { 0 }];
        let mut word = vec![false; n];

        for (position, term) in query.terms.iter().enumerate() {
            if position > 0 {
                word.fill(false);
            }
            self.mark_names_containing(term.as_bytes(), &mut word);
            if smart {
                for (seen, &here) in in_name.iter_mut().zip(&word) {
                    *seen |= here;
                }
            }
            // Parents come before their children, so by the time the loop
            // reaches an entry its parent's answer is already final. One
            // pass therefore carries a match in a folder's name down to
            // everything inside that folder.
            for i in 0..n {
                if !word[i] {
                    let parent = self.parent[i];
                    if parent != NO_PARENT && word[parent as usize] {
                        word[i] = true;
                    }
                }
            }
            if position == 0 {
                in_path.clone_from(&word);
            } else {
                for (all, &here) in in_path.iter_mut().zip(&word) {
                    *all &= here;
                }
            }
        }

        let has_terms = !query.terms.is_empty();
        let mut result = SearchResult::default();
        for i in 0..n {
            if has_terms && !(in_path[i] && (!smart || in_name[i])) {
                continue;
            }
            if let Some(ext) = wanted_ext
                && self.ext[i] != ext
            {
                continue;
            }
            result.total += 1;
            if result.hits.len() < limit {
                // `add` refuses to grow past u32, so this cannot truncate.
                result.hits.push(i as EntryId);
            }
        }
        result
    }

    /// Set `marks[i]` for every entry whose own lowercase name contains
    /// `needle`.
    ///
    /// All names are one buffer, so this is a single substring search over
    /// that buffer. After a hit the search jumps to the start of the next
    /// name, because one hit per name is enough.
    fn mark_names_containing(&self, needle: &[u8], marks: &mut [bool]) {
        let n = self.len();
        if n == 0 || needle.is_empty() || needle.contains(&0) {
            return;
        }
        let finder = memmem::Finder::new(needle);
        let haystack = &self.lower[..];
        let mut from = 0usize;
        let mut entry = 0usize;
        while let Some(found) = finder.find(&haystack[from..]) {
            let at = from + found;
            // Hits arrive in order, so the entry cursor only moves forward.
            while self.lower_start[entry + 1] as usize <= at {
                entry += 1;
            }
            marks[entry] = true;
            from = self.lower_start[entry + 1] as usize;
            entry += 1;
            if entry == n {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small drive:
    ///
    /// ```text
    /// D:\
    ///   Finance\
    ///     2026\
    ///       invoice-march.pdf
    ///       Invoice-April.PDF
    ///       statement.pdf
    ///   Photos\
    ///     Hunza\
    ///       IMG_2041.jpg
    ///     photos-backup.zip
    ///   Dev\
    ///     node_modules\
    ///       react\
    ///         index.js
    ///     .gitignore
    ///     Über Notes.md
    /// ```
    fn sample() -> (Index, HashMap<&'static str, EntryId>) {
        let mut b = IndexBuilder::new();
        let mut ids = HashMap::new();
        let mut add = |ids: &mut HashMap<&'static str, EntryId>,
                       parent: Option<&str>,
                       name: &'static str,
                       kind: Kind,
                       size: u64| {
            let parent = parent.map(|p| ids[p]);
            let id = b.add(parent, name, kind, size).unwrap();
            ids.insert(name, id);
        };
        add(&mut ids, None, "D:\\", Kind::Dir, 0);
        add(&mut ids, Some("D:\\"), "Finance", Kind::Dir, 0);
        add(&mut ids, Some("Finance"), "2026", Kind::Dir, 0);
        add(&mut ids, Some("2026"), "invoice-march.pdf", Kind::File, 100);
        add(&mut ids, Some("2026"), "Invoice-April.PDF", Kind::File, 200);
        add(&mut ids, Some("2026"), "statement.pdf", Kind::File, 300);
        add(&mut ids, Some("D:\\"), "Photos", Kind::Dir, 0);
        add(&mut ids, Some("Photos"), "Hunza", Kind::Dir, 0);
        add(
            &mut ids,
            Some("Hunza"),
            "IMG_2041.jpg",
            Kind::File,
            6_000_000,
        );
        add(
            &mut ids,
            Some("Photos"),
            "photos-backup.zip",
            Kind::File,
            9_000,
        );
        add(&mut ids, Some("D:\\"), "Dev", Kind::Dir, 0);
        add(&mut ids, Some("Dev"), "node_modules", Kind::Dir, 0);
        add(&mut ids, Some("node_modules"), "react", Kind::Dir, 0);
        add(&mut ids, Some("react"), "index.js", Kind::File, 40);
        add(&mut ids, Some("Dev"), ".gitignore", Kind::File, 12);
        add(&mut ids, Some("Dev"), "Über Notes.md", Kind::File, 77);
        (b.finish(), ids)
    }

    fn names(index: &Index, result: &SearchResult) -> Vec<String> {
        result
            .hits
            .iter()
            .map(|&id| index.name(id).unwrap().to_owned())
            .collect()
    }

    fn search(index: &Index, text: &str) -> SearchResult {
        index.search(&Query::parse(text), 100)
    }

    fn search_path(index: &Index, text: &str) -> SearchResult {
        index.search(&Query::parse(text).with_scope(Scope::Path), 100)
    }

    #[test]
    fn an_empty_query_finds_nothing() {
        let (index, _) = sample();
        assert_eq!(search(&index, "   "), SearchResult::default());
    }

    #[test]
    fn matches_names_without_regard_to_case() {
        let (index, _) = sample();
        let result = search(&index, "INVOICE");
        assert_eq!(result.total, 2);
        assert_eq!(
            names(&index, &result),
            ["invoice-march.pdf", "Invoice-April.PDF"]
        );
    }

    #[test]
    fn a_word_may_match_in_a_parent_folder() {
        let (index, _) = sample();
        // "2026" and "finance" are folders above the file; "invoice" is in
        // the file's own name.
        let result = search(&index, "invoice 2026 finance");
        assert_eq!(
            names(&index, &result),
            ["invoice-march.pdf", "Invoice-April.PDF"]
        );
    }

    #[test]
    fn smart_scope_returns_the_folder_not_its_contents() {
        let (index, _) = sample();
        let result = search(&index, "photos");
        assert_eq!(names(&index, &result), ["Photos", "photos-backup.zip"]);
    }

    #[test]
    fn path_scope_returns_the_folder_and_its_contents() {
        let (index, _) = sample();
        let result = search_path(&index, "photos");
        assert_eq!(
            names(&index, &result),
            ["Photos", "Hunza", "IMG_2041.jpg", "photos-backup.zip"]
        );
    }

    #[test]
    fn smart_scope_needs_one_word_in_the_name_itself() {
        let (index, _) = sample();
        // index.js sits under node_modules\react, but neither word is in
        // its own name. The react folder has "react" in its name.
        assert_eq!(
            names(&index, &search(&index, "node_modules react")),
            ["react"]
        );
        assert_eq!(
            names(&index, &search_path(&index, "node_modules react")),
            ["react", "index.js"]
        );
    }

    #[test]
    fn every_word_has_to_match() {
        let (index, _) = sample();
        assert_eq!(search(&index, "invoice hunza").total, 0);
        assert_eq!(search(&index, "zzzz").total, 0);
    }

    #[test]
    fn a_match_cannot_run_across_two_names() {
        let (index, _) = sample();
        // "pdfinvoice" would only exist if two neighbouring names were read
        // as one.
        assert_eq!(search(&index, "pdfinvoice").total, 0);
        assert_eq!(search(&index, "financephotos").total, 0);
    }

    #[test]
    fn filters_by_extension_exactly() {
        let (index, _) = sample();
        let result = search(&index, "ext:pdf");
        assert_eq!(
            names(&index, &result),
            ["invoice-march.pdf", "Invoice-April.PDF", "statement.pdf"]
        );
        assert_eq!(search(&index, "ext:pd").total, 0);
        assert_eq!(search(&index, "ext:docx").total, 0);
        assert_eq!(
            names(&index, &search(&index, "april ext:pdf")),
            ["Invoice-April.PDF"]
        );
    }

    #[test]
    fn dotfiles_and_folders_have_no_extension() {
        let (index, ids) = sample();
        assert_eq!(index.extension(ids[".gitignore"]), None);
        assert_eq!(index.extension(ids["Finance"]), None);
        assert_eq!(index.extension(ids["Invoice-April.PDF"]), Some("pdf"));
        assert_eq!(search(&index, "ext:gitignore").total, 0);
    }

    #[test]
    fn lowercases_names_beyond_ascii() {
        let (index, _) = sample();
        assert_eq!(names(&index, &search(&index, "über")), ["Über Notes.md"]);
        assert_eq!(
            names(&index, &search(&index, "ÜBER notes")),
            ["Über Notes.md"]
        );
    }

    #[test]
    fn counts_every_match_but_returns_only_the_limit() {
        let (index, _) = sample();
        let result = index.search(&Query::parse("ext:pdf"), 2);
        assert_eq!(result.total, 3);
        assert_eq!(result.hits.len(), 2);
        let none = index.search(&Query::parse("ext:pdf"), 0);
        assert_eq!(none.total, 3);
        assert!(none.hits.is_empty());
    }

    #[test]
    fn rebuilds_full_paths() {
        let (index, ids) = sample();
        let sep = MAIN_SEPARATOR;
        assert_eq!(index.path(ids["D:\\"]).unwrap(), "D:\\");
        assert_eq!(
            index.path(ids["invoice-march.pdf"]).unwrap(),
            format!("D:\\Finance{sep}2026{sep}invoice-march.pdf")
        );
        assert_eq!(index.path(9_999), None);
    }

    #[test]
    fn reports_entry_details() {
        let (index, ids) = sample();
        let photo = ids["IMG_2041.jpg"];
        assert_eq!(index.len(), 16);
        assert_eq!(index.size(photo), Some(6_000_000));
        assert_eq!(index.kind(photo), Some(Kind::File));
        assert_eq!(index.kind(ids["Hunza"]), Some(Kind::Dir));
        assert_eq!(index.parent(photo), Some(ids["Hunza"]));
        assert_eq!(index.parent(ids["D:\\"]), None);
        assert_eq!(index.name(9_999), None);
        assert!(index.memory_bytes() > 0);
    }

    #[test]
    fn the_builder_rejects_bad_parents() {
        let mut builder = IndexBuilder::new();
        let root = builder.add(None, "C:\\", Kind::Dir, 0).unwrap();
        let file = builder.add(Some(root), "a.txt", Kind::File, 1).unwrap();
        assert_eq!(
            builder.add(Some(42), "b.txt", Kind::File, 1),
            Err(BuildError::UnknownParent(42))
        );
        assert_eq!(
            builder.add(Some(file), "c.txt", Kind::File, 1),
            Err(BuildError::ParentIsFile(file))
        );
        // The failed calls left nothing behind.
        assert_eq!(builder.len(), 2);
        assert_eq!(builder.finish().len(), 2);
    }

    #[test]
    fn an_empty_index_can_be_searched() {
        let index = IndexBuilder::new().finish();
        assert!(index.is_empty());
        assert_eq!(index.search(&Query::parse("anything"), 10).total, 0);
    }

    /// The slow, obvious way to answer a query: look at each entry on its
    /// own and walk up its parents. The fast search must agree with it.
    fn search_slowly(index: &Index, query: &Query) -> Vec<EntryId> {
        let mut hits = Vec::new();
        for id in 0..index.len() as EntryId {
            let own = index.name(id).unwrap().to_lowercase();
            let mut parts = vec![own.clone()];
            let mut current = id;
            while let Some(parent) = index.parent(current) {
                parts.push(index.name(parent).unwrap().to_lowercase());
                current = parent;
            }
            let all_in_path = query
                .terms
                .iter()
                .all(|term| parts.iter().any(|part| part.contains(term.as_str())));
            let one_in_name = query.terms.iter().any(|term| own.contains(term.as_str()));
            let terms_ok = query.terms.is_empty()
                || (all_in_path && (query.scope == Scope::Path || one_in_name));
            let ext_ok = match &query.ext {
                None => true,
                Some(ext) => index.extension(id) == Some(ext.as_str()),
            };
            if terms_ok && ext_ok {
                hits.push(id);
            }
        }
        hits
    }

    #[test]
    fn agrees_with_a_slow_reference_search() {
        let index = crate::synthetic::build(4_000).unwrap();
        let texts = [
            "invoice",
            "invoice march 2026",
            "INVOICE ext:pdf",
            "react index",
            "node_modules react",
            "project-00003 src",
            "ext:jpg",
            "ext:ts auth",
            "img_10 hunza",
            "statement 2004",
            "dev",
            "e",
            ".",
            "d:",
            "zzzz",
            "package.json project",
            r"finance\2003 invoice",
        ];
        for text in texts {
            for scope in [Scope::Smart, Scope::Path] {
                let query = Query::parse(text).with_scope(scope);
                let fast = index.search(&query, usize::MAX);
                let slow = search_slowly(&index, &query);
                assert_eq!(fast.total, slow.len(), "{text} {scope:?}");
                assert_eq!(fast.hits, slow, "{text} {scope:?}");
            }
        }
    }

    #[test]
    fn finds_a_match_in_the_very_last_entry() {
        let mut builder = IndexBuilder::new();
        let root = builder.add(None, "/", Kind::Dir, 0).unwrap();
        builder.add(Some(root), "first.txt", Kind::File, 1).unwrap();
        builder.add(Some(root), "last.txt", Kind::File, 1).unwrap();
        let index = builder.finish();
        assert_eq!(names(&index, &search(&index, "last")), ["last.txt"]);
        assert_eq!(
            names(&index, &search(&index, "txt")),
            ["first.txt", "last.txt"]
        );
    }
}
