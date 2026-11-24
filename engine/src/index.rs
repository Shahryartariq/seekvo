//! The in-memory file index.
//!
//! Every file and folder is one entry. Entries live in a handful of flat
//! arrays (one per field) instead of one heap object each, and all the names
//! sit end to end in a single buffer. A search is then a straight run through
//! memory the CPU can prefetch, with no pointers to chase.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
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
    modified: Vec<u32>,
    depth: Vec<u8>,
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
            modified: Vec::new(),
            depth: Vec::new(),
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
        self.add_dated(parent, name, kind, size, 0)
    }

    /// [`add`](Self::add), with the time the entry was last changed, in
    /// seconds since the start of 1970. Pass 0 when the time is not known.
    pub fn add_dated(
        &mut self,
        parent: Option<EntryId>,
        name: &str,
        kind: Kind,
        size: u64,
        modified: u32,
    ) -> Result<EntryId, BuildError> {
        let id = u32::try_from(self.len()).map_err(|_| BuildError::TooLarge)?;
        if id == NO_PARENT {
            return Err(BuildError::TooLarge);
        }
        let mut depth = 0u8;
        if let Some(parent) = parent {
            match self.is_dir.get(parent as usize) {
                None => return Err(BuildError::UnknownParent(parent)),
                Some(false) => return Err(BuildError::ParentIsFile(parent)),
                Some(true) => depth = self.depth[parent as usize].saturating_add(1),
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
        self.modified.push(modified);
        self.depth.push(depth);
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
            newest: self.modified.iter().copied().max().unwrap_or(0),
            modified: self.modified.into_boxed_slice(),
            depth: self.depth.into_boxed_slice(),
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
    /// Seconds since 1970, or 0 when not known.
    modified: Box<[u32]>,
    /// How many folders deep the entry is; top-level entries are 0.
    depth: Box<[u8]>,
    /// The latest `modified` of any entry. "Recent" is measured from here,
    /// so ranking needs no clock.
    newest: u32,
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

    /// When the entry was last changed, in seconds since the start of
    /// 1970, if that is known.
    pub fn modified(&self, id: EntryId) -> Option<u32> {
        self.modified
            .get(id as usize)
            .copied()
            .filter(|&seconds| seconds != 0)
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
            + std::mem::size_of_val(&*self.modified)
            + std::mem::size_of_val(&*self.depth)
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
        let mut word = vec![false; n];
        // quality[i]: how well the words fit entry i's own name, added up
        // over the words. 0 means no word is in the name at all.
        let mut quality = vec![0u8; n];
        // covered[i]: how many bytes of entry i's name the words account
        // for.
        let mut covered = vec![0u8; n];

        for (position, term) in query.terms.iter().enumerate() {
            if position > 0 {
                word.fill(false);
            }
            self.mark_names_containing(term.as_bytes(), &mut word, &mut quality, &mut covered);
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
        let mut total = 0usize;
        // The best `limit` so far, with the worst of them on top so it is
        // the one to go when something better turns up.
        let mut best: BinaryHeap<Reverse<(u32, Reverse<EntryId>)>> =
            BinaryHeap::with_capacity(limit.min(n) + 1);
        // The score of the worst entry in `best`, once `best` is full.
        let mut worst_kept = 0u32;
        for i in 0..n {
            if has_terms && !(in_path[i] && (!smart || quality[i] > 0)) {
                continue;
            }
            if let Some(ext) = wanted_ext
                && self.ext[i] != ext
            {
                continue;
            }
            total += 1;
            if limit == 0 {
                continue;
            }
            if best.len() == limit {
                // Most matches cannot beat the worst one kept, and the
                // quality alone shows it. Those are not scored at all.
                let at_most = u32::from(quality[i]) * QUALITY_WEIGHT + REST_AT_MOST;
                if at_most < worst_kept {
                    continue;
                }
            }
            // `add` refuses to grow past u32, so this cannot truncate.
            let score = self.score(i, quality[i], covered[i]);
            let candidate = Reverse((score, Reverse(i as EntryId)));
            if best.len() < limit {
                best.push(candidate);
            } else if best.peek().is_some_and(|worst| candidate < *worst) {
                best.pop();
                best.push(candidate);
            } else {
                continue;
            }
            if best.len() == limit
                && let Some(Reverse((score, _))) = best.peek()
            {
                worst_kept = *score;
            }
        }
        let hits = best
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse((_, Reverse(id)))| id)
            .collect();
        SearchResult { total, hits }
    }

    /// How high entry `i` should come in the results. Higher is better.
    ///
    /// In order of weight: how well the words fit the name (see
    /// [`fit`]), how much of the name they account for, how recently the
    /// entry changed, and how close to the top of the drive it is.
    fn score(&self, i: usize, quality: u8, covered: u8) -> u32 {
        let name_length = self.lower_start[i + 1] - self.lower_start[i] - 1;
        // Nothing covered is the common case for a search by extension
        // alone, and is not worth a division.
        let coverage = match covered {
            0 => 0,
            covered => (u32::from(covered).min(name_length) * COVERAGE_AT_MOST)
                .checked_div(name_length)
                .unwrap_or(0),
        };
        const DAY: u32 = 86_400;
        let recency = match self.modified[i] {
            0 => 0,
            modified => match self.newest.saturating_sub(modified) {
                age if age < DAY => RECENCY_AT_MOST,
                age if age < 8 * DAY => 48,
                age if age < 31 * DAY => 36,
                age if age < 366 * DAY => 20,
                age if age < 1096 * DAY => 8,
                _ => 0,
            },
        };
        let closeness = CLOSENESS_AT_MOST - (u32::from(self.depth[i]) * 4).min(CLOSENESS_AT_MOST);
        u32::from(quality) * QUALITY_WEIGHT + coverage + recency + closeness
    }

    /// Set `marks[i]` for every entry whose own lowercase name contains
    /// `needle`, and add how well it fits to `quality[i]` and its length to
    /// `covered[i]`.
    ///
    /// All names are one buffer, so this is a single substring search over
    /// that buffer. After a hit the search jumps to the start of the next
    /// name.
    fn mark_names_containing(
        &self,
        needle: &[u8],
        marks: &mut [bool],
        quality: &mut [u8],
        covered: &mut [u8],
    ) {
        let n = self.len();
        if n == 0 || needle.is_empty() || needle.contains(&0) {
            return;
        }
        let finder = memmem::Finder::new(needle);
        let haystack = &self.lower[..];
        let length = u8::try_from(needle.len()).unwrap_or(u8::MAX);
        let mut from = 0usize;
        let mut entry = 0usize;
        while let Some(found) = finder.find(&haystack[from..]) {
            let at = from + found;
            // Hits arrive in order, so the entry cursor only moves forward.
            while self.lower_start[entry + 1] as usize <= at {
                entry += 1;
            }
            let start = self.lower_start[entry] as usize;
            let next = self.lower_start[entry + 1] as usize;
            // Without the NUL that ends it.
            let name = &haystack[start..next - 1];

            // The word may be in the name more than once; the best place
            // counts.
            let mut fit_here = fit(name, at - start, needle.len());
            let mut place = at - start + 1;
            // Names are short, so looking for the first byte and comparing
            // the rest is quicker here than starting the big search again.
            // Any further place is past the start of the name, where a
            // whole word is the best there is.
            while fit_here < BEST_FIT_PAST_START && place + needle.len() <= name.len() {
                let last_place = name.len() - needle.len();
                let Some(more) = memchr::memchr(needle[0], &name[place..=last_place]) else {
                    break;
                };
                place += more;
                if name[place..place + needle.len()] == *needle {
                    fit_here = fit_here.max(fit(name, place, needle.len()));
                }
                place += 1;
            }

            marks[entry] = true;
            quality[entry] = quality[entry].saturating_add(fit_here);
            covered[entry] = covered[entry].saturating_add(length);
            from = next;
            entry += 1;
            if entry == n {
                break;
            }
        }
    }
}

/// The highest value [`fit`] gives.
const BEST_FIT: u8 = 12;
/// The highest value [`fit`] gives anywhere but the start of the name.
const BEST_FIT_PAST_START: u8 = 10;

/// One step of quality outweighs everything else put together.
const QUALITY_WEIGHT: u32 = 1000;
const COVERAGE_AT_MOST: u32 = 200;
const RECENCY_AT_MOST: u32 = 60;
const CLOSENESS_AT_MOST: u32 = 60;
const REST_AT_MOST: u32 = COVERAGE_AT_MOST + RECENCY_AT_MOST + CLOSENESS_AT_MOST;

/// How well a word found at `at` fits `name`. Both are lowercase.
///
/// A word is better placed the more it lines up with the words the name is
/// made of, where `-`, `_`, `.`, spaces and a switch between letters and
/// digits all start a new word:
///
/// | The word is | Example for `report` | Value |
/// | --- | --- | ---: |
/// | a whole word at the start of the name | `report-2026.pdf` | 12 |
/// | a whole word elsewhere | `annual report.pdf` | 10 |
/// | the start of the name | `reporting.pdf` | 8 |
/// | the start of a word | `annual-reporting.pdf` | 6 |
/// | the end of a word | `prereport.pdf` | 4 |
/// | inside a word | `prereporting.pdf` | 3 |
fn fit(name: &[u8], at: usize, length: usize) -> u8 {
    let end = at + length;
    let starts_word = at == 0 || is_word_break(name[at - 1], name[at]);
    let ends_word = end >= name.len() || is_word_break(name[end - 1], name[end]);
    match (at == 0, starts_word, ends_word) {
        (true, _, true) => BEST_FIT,
        (false, true, true) => BEST_FIT_PAST_START,
        (true, _, false) => 8,
        (false, true, false) => 6,
        (false, false, true) => 4,
        (false, false, false) => 3,
    }
}

/// Whether a new word starts between two neighbouring bytes of a name.
fn is_word_break(before: u8, after: u8) -> bool {
    // Bytes of non-ASCII characters count as letters.
    let letter = |byte: u8| byte.is_ascii_lowercase() || byte >= 0x80;
    let digit = |byte: u8| byte.is_ascii_digit();
    match (letter(before), digit(before), letter(after), digit(after)) {
        (true, _, true, _) | (_, true, _, true) => false,
        // Letter next to digit, or punctuation on either side.
        _ => true,
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
            ["Photos", "photos-backup.zip", "Hunza", "IMG_2041.jpg"]
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
    /// own and walk up its parents. The fast search must agree with it,
    /// in what it finds and in the order it puts it.
    fn search_slowly(index: &Index, query: &Query) -> Vec<EntryId> {
        let newest = (0..index.len() as EntryId)
            .filter_map(|id| index.modified(id))
            .max()
            .unwrap_or(0);
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
                let depth = parts.len() - 1;
                let score = score_slowly(&own, query, index.modified(id), newest, depth);
                hits.push((Reverse(score), id));
            }
        }
        hits.sort();
        hits.into_iter().map(|(_, id)| id).collect()
    }

    /// The words a name is made of, as byte ranges: runs of letters, runs
    /// of digits, and every other character on its own.
    fn words_of(name: &str) -> Vec<(usize, usize)> {
        #[derive(PartialEq, Clone, Copy)]
        enum Class {
            Letter,
            Digit,
            Other,
        }
        let class = |c: char| match c {
            c if c.is_ascii_digit() => Class::Digit,
            c if c.is_ascii_lowercase() || !c.is_ascii() => Class::Letter,
            _ => Class::Other,
        };
        let mut words: Vec<(usize, usize)> = Vec::new();
        let mut last = None;
        for (at, c) in name.char_indices() {
            let now = class(c);
            match words.last_mut() {
                Some(word) if last == Some(now) && now != Class::Other => {
                    word.1 = at + c.len_utf8()
                }
                _ => words.push((at, at + c.len_utf8())),
            }
            last = Some(now);
        }
        words
    }

    /// The ranking, worked out from the description in the README instead
    /// of from the code.
    fn score_slowly(
        own: &str,
        query: &Query,
        modified: Option<u32>,
        newest: u32,
        depth: usize,
    ) -> u32 {
        let words = words_of(own);
        let mut quality = 0u32;
        let mut covered = 0usize;
        for term in &query.terms {
            let places: Vec<usize> = (0..own.len())
                .filter(|&at| own.as_bytes()[at..].starts_with(term.as_bytes()))
                .collect();
            let best = places
                .iter()
                .map(|&at| {
                    let end = at + term.len();
                    let starts = words.iter().any(|word| word.0 == at);
                    let ends = words.iter().any(|word| word.1 == end);
                    match (at == 0, starts, ends) {
                        (true, _, true) => 12,
                        (false, true, true) => 10,
                        (true, _, false) => 8,
                        (false, true, false) => 6,
                        (false, false, true) => 4,
                        (false, false, false) => 3,
                    }
                })
                .max();
            if let Some(best) = best {
                quality += best;
                covered += term.len();
            }
        }
        let quality = quality.min(255);
        let covered = covered.min(255).min(own.len());
        let coverage = if own.is_empty() {
            0
        } else {
            covered * 200 / own.len()
        };
        let recency = match modified {
            None => 0,
            Some(modified) => match (newest - modified) / 86_400 {
                0 => 60,
                1..=7 => 48,
                8..=30 => 36,
                31..=365 => 20,
                366..=1095 => 8,
                _ => 0,
            },
        };
        let closeness = 60 - (depth * 4).min(60);
        quality * 1000 + coverage as u32 + recency + closeness as u32
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
                // Asking for fewer gives the best of them, not the first.
                let few = index.search(&query, 7);
                assert_eq!(few.total, slow.len(), "{text} {scope:?}");
                assert_eq!(few.hits, slow[..slow.len().min(7)], "{text} {scope:?}");
            }
        }
    }

    // ---- Ranking -----------------------------------------------------------

    const DAY: u32 = 86_400;
    const TODAY: u32 = 1_795_000_000;

    /// An index of files in one folder, given as (name, days since it was
    /// last changed). `None` means the time is not known.
    fn folder_of(files: &[(&str, Option<u32>)]) -> Index {
        let mut builder = IndexBuilder::new();
        let root = builder.add(None, "/", Kind::Dir, 0).unwrap();
        for &(name, age) in files {
            let modified = age.map_or(0, |days| TODAY - days * DAY);
            builder
                .add_dated(Some(root), name, Kind::File, 1, modified)
                .unwrap();
        }
        builder.finish()
    }

    fn ranked(index: &Index, text: &str) -> Vec<String> {
        names(index, &search(index, text))
    }

    #[test]
    fn a_better_place_in_the_name_ranks_higher() {
        // Listed worst first, so index order cannot be what sorts them.
        let index = folder_of(&[
            ("prereporting.pdf", Some(0)),
            ("prereport.pdf", Some(0)),
            ("annual-reporting.pdf", Some(0)),
            ("reporting.pdf", Some(0)),
            ("annual report.pdf", Some(0)),
            ("report-2026.pdf", Some(0)),
        ]);
        assert_eq!(
            ranked(&index, "report"),
            [
                "report-2026.pdf",
                "annual report.pdf",
                "reporting.pdf",
                "annual-reporting.pdf",
                "prereport.pdf",
                "prereporting.pdf",
            ]
        );
    }

    #[test]
    fn letters_next_to_digits_start_a_new_word() {
        let index = folder_of(&[("img2041.jpg", Some(0)), ("x2041y.jpg", Some(0))]);
        assert_eq!(ranked(&index, "img"), ["img2041.jpg"]);
        // "2041" is a whole word in both; the shorter name wins.
        assert_eq!(ranked(&index, "2041"), ["x2041y.jpg", "img2041.jpg"]);
        assert_eq!(fit(b"img2041.jpg", 3, 4), 10);
        assert_eq!(fit(b"img2041.jpg", 0, 3), 12);
        assert_eq!(fit(b"img2041.jpg", 4, 2), 3);
    }

    #[test]
    fn the_best_place_counts_when_a_word_is_there_twice() {
        let index = folder_of(&[
            ("preinvoice invoice.pdf", Some(0)),
            ("xinvoice.pdf", Some(0)),
        ]);
        // The second "invoice" in the first name is a whole word.
        assert_eq!(
            ranked(&index, "invoice"),
            ["preinvoice invoice.pdf", "xinvoice.pdf"]
        );
        // Overlapping places are looked at too: "aa" ends the word "aaa".
        assert_eq!(fit(b"baaa", 2, 2), 4);
        let overlapping = folder_of(&[("baaa", Some(0))]);
        assert_eq!(overlapping.search(&Query::parse("aa"), 1).total, 1);
    }

    #[test]
    fn among_equal_fits_the_name_closest_to_the_words_comes_first() {
        let index = folder_of(&[
            ("invoice-march-2026-final-v2.pdf", Some(0)),
            ("invoice-march.pdf", Some(0)),
            ("invoice.pdf", Some(0)),
            ("invoice", Some(0)),
        ]);
        assert_eq!(
            ranked(&index, "invoice"),
            [
                "invoice",
                "invoice.pdf",
                "invoice-march.pdf",
                "invoice-march-2026-final-v2.pdf",
            ]
        );
    }

    #[test]
    fn more_of_the_words_in_the_name_ranks_higher() {
        let mut builder = IndexBuilder::new();
        let root = builder.add(None, "/", Kind::Dir, 0).unwrap();
        let year = builder.add(Some(root), "2026", Kind::Dir, 0).unwrap();
        builder
            .add(Some(year), "invoice.pdf", Kind::File, 1)
            .unwrap();
        builder
            .add(Some(year), "invoice-2026-march.pdf", Kind::File, 1)
            .unwrap();
        let index = builder.finish();
        // Both match: the first has "2026" only in its folder.
        assert_eq!(
            ranked(&index, "invoice 2026"),
            ["invoice-2026-march.pdf", "invoice.pdf"]
        );
    }

    #[test]
    fn among_equal_names_the_one_changed_most_recently_comes_first() {
        let index = folder_of(&[
            ("notes-a.txt", None),
            ("notes-b.txt", Some(2000)),
            ("notes-c.txt", Some(400)),
            ("notes-d.txt", Some(90)),
            ("notes-e.txt", Some(20)),
            ("notes-f.txt", Some(3)),
            ("notes-g.txt", Some(0)),
        ]);
        assert_eq!(
            ranked(&index, "notes"),
            [
                "notes-g.txt",
                "notes-f.txt",
                "notes-e.txt",
                "notes-d.txt",
                "notes-c.txt",
                // Very old and unknown count the same; the earlier entry
                // comes first.
                "notes-a.txt",
                "notes-b.txt",
            ]
        );
    }

    #[test]
    fn a_better_fit_beats_a_newer_file() {
        let index = folder_of(&[
            ("budgeting-ideas.txt", Some(0)),
            ("budget.xlsx", Some(3000)),
        ]);
        assert_eq!(
            ranked(&index, "budget"),
            ["budget.xlsx", "budgeting-ideas.txt"]
        );
    }

    #[test]
    fn among_equal_names_the_one_nearer_the_top_comes_first() {
        let mut builder = IndexBuilder::new();
        let root = builder.add(None, "/", Kind::Dir, 0).unwrap();
        let mut folder = root;
        for level in 0..20 {
            folder = builder
                .add(Some(folder), &format!("level{level}"), Kind::Dir, 0)
                .unwrap();
            if level == 2 || level == 19 {
                builder
                    .add(Some(folder), "readme.md", Kind::File, 1)
                    .unwrap();
            }
        }
        builder.add(Some(root), "readme.md", Kind::File, 1).unwrap();
        let index = builder.finish();
        let depths: Vec<usize> = search(&index, "readme")
            .hits
            .iter()
            .map(|&id| index.path(id).unwrap().matches("level").count())
            .collect();
        assert_eq!(depths, [0, 3, 20]);
    }

    #[test]
    fn a_search_by_extension_alone_lists_the_newest_first() {
        let index = folder_of(&[
            ("old.pdf", Some(900)),
            ("new.pdf", Some(1)),
            ("other.txt", Some(0)),
            ("middle.pdf", Some(60)),
        ]);
        assert_eq!(
            ranked(&index, "ext:pdf"),
            ["new.pdf", "middle.pdf", "old.pdf"]
        );
    }

    #[test]
    fn a_limit_keeps_the_best_and_still_counts_everything() {
        let mut files: Vec<(String, Option<u32>)> = (0..500)
            .map(|i| (format!("holiday-photo-{i:03}.jpg"), Some(i + 10)))
            .collect();
        files.push(("photo.jpg".to_owned(), Some(4000)));
        let files: Vec<(&str, Option<u32>)> = files
            .iter()
            .map(|(name, age)| (name.as_str(), *age))
            .collect();
        let index = folder_of(&files);

        let three = index.search(&Query::parse("photo"), 3);
        assert_eq!(three.total, 501);
        // The exact name was added last and is the oldest, and still wins.
        assert_eq!(
            names(&index, &three),
            [
                "photo.jpg",
                "holiday-photo-000.jpg",
                "holiday-photo-001.jpg"
            ]
        );
        let none = index.search(&Query::parse("photo"), 0);
        assert_eq!((none.total, none.hits.len()), (501, 0));
    }

    #[test]
    fn remembers_when_an_entry_was_changed() {
        let index = folder_of(&[("known.txt", Some(5)), ("unknown.txt", None)]);
        assert_eq!(index.modified(0), None);
        assert_eq!(index.modified(1), Some(TODAY - 5 * DAY));
        assert_eq!(index.modified(2), None);
        assert_eq!(index.modified(99), None);
    }

    #[test]
    fn very_long_queries_and_very_deep_folders_do_not_overflow() {
        let mut builder = IndexBuilder::new();
        let mut folder = builder.add(None, "/", Kind::Dir, 0).unwrap();
        for _ in 0..300 {
            folder = builder.add(Some(folder), "deep", Kind::Dir, 0).unwrap();
        }
        let name = "word ".repeat(60);
        builder.add(Some(folder), &name, Kind::File, 1).unwrap();
        let index = builder.finish();
        // Thirty words, each scoring the most it can, on a name 300 bytes
        // long, 301 folders down.
        let text: String = (0..30)
            .map(|i| format!("wor{} ", "d".repeat(i % 2)))
            .collect();
        let result = index.search(&Query::parse(&(text + "word")), 5);
        assert_eq!(result.total, 1);
    }

    #[test]
    fn finds_a_match_in_the_very_last_entry() {
        let mut builder = IndexBuilder::new();
        let root = builder.add(None, "/", Kind::Dir, 0).unwrap();
        builder.add(Some(root), "first.txt", Kind::File, 1).unwrap();
        builder.add(Some(root), "last.txt", Kind::File, 1).unwrap();
        let index = builder.finish();
        assert_eq!(names(&index, &search(&index, "last")), ["last.txt"]);
        // The shorter name comes first: "txt" is more of it.
        assert_eq!(
            names(&index, &search(&index, "txt")),
            ["last.txt", "first.txt"]
        );
    }
}
