//! Fill an index by walking a folder with the ordinary file system calls.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::index::{BuildError, EntryId, Index, IndexBuilder, Kind};

/// Numbers from a finished scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanStats {
    pub files: usize,
    pub dirs: usize,
    /// Folders or entries that could not be read, usually for lack of
    /// permission. They are skipped.
    pub skipped: usize,
}

/// Why a scan could not finish.
#[derive(Debug)]
pub enum ScanError {
    /// The starting folder itself could not be read.
    Root(io::Error),
    Build(BuildError),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Root(error) => write!(f, "cannot read the folder: {error}"),
            ScanError::Build(error) => write!(f, "cannot build the index: {error}"),
        }
    }
}

impl std::error::Error for ScanError {}

impl From<BuildError> for ScanError {
    fn from(error: BuildError) -> Self {
        ScanError::Build(error)
    }
}

/// Walk `root` and everything under it.
///
/// Folders are visited level by level and each folder's entries are added in
/// name order, so the same tree always produces the same index. Symbolic
/// links are recorded as files and not followed, which also rules out loops.
pub fn scan_dir(root: &Path) -> Result<(Index, ScanStats), ScanError> {
    let metadata = fs::metadata(root).map_err(ScanError::Root)?;
    if !metadata.is_dir() {
        return Err(ScanError::Root(io::Error::new(
            io::ErrorKind::NotADirectory,
            "not a folder",
        )));
    }

    let mut builder = IndexBuilder::new();
    let mut stats = ScanStats::default();
    let root_id = builder.add(None, &root.to_string_lossy(), Kind::Dir, 0)?;
    stats.dirs += 1;

    let mut queue: VecDeque<(PathBuf, EntryId)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), root_id));

    while let Some((dir, dir_id)) = queue.pop_front() {
        let reader = match fs::read_dir(&dir) {
            Ok(reader) => reader,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };

        let mut children: Vec<(String, Kind, u64)> = Vec::new();
        for entry in reader {
            let Ok(entry) = entry else {
                stats.skipped += 1;
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                stats.skipped += 1;
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if file_type.is_dir() {
                children.push((name, Kind::Dir, 0));
            } else {
                // A file whose size cannot be read is still worth listing.
                let size = if file_type.is_file() {
                    entry.metadata().map(|m| m.len()).unwrap_or(0)
                } else {
                    0
                };
                children.push((name, Kind::File, size));
            }
        }
        children.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        for (name, kind, size) in children {
            let id = builder.add(Some(dir_id), &name, kind, size)?;
            match kind {
                Kind::Dir => {
                    stats.dirs += 1;
                    queue.push_back((dir.join(&name), id));
                }
                Kind::File => stats.files += 1,
            }
        }
    }

    Ok((builder.finish(), stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A folder under the system temp folder that removes itself.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let unique = format!(
                "seekvo-scan-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(unique);
            fs::create_dir_all(&path).unwrap();
            TempTree(path)
        }

        fn file(&self, relative: &str, contents: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn scans_a_tree_and_finds_its_files() {
        let tree = TempTree::new();
        tree.file("Finance/2026/invoice-march.pdf", "12345");
        tree.file("Finance/2026/notes.txt", "hi");
        tree.file("Photos/IMG_0001.jpg", "");
        fs::create_dir_all(tree.0.join("Empty")).unwrap();

        let (index, stats) = scan_dir(&tree.0).unwrap();
        assert_eq!(stats.files, 3);
        // The root, Empty, Finance, Finance/2026 and Photos.
        assert_eq!(stats.dirs, 5);
        assert_eq!(stats.skipped, 0);
        assert_eq!(index.len(), 8);

        let result = index.search(&Query::parse("invoice 2026"), 10);
        assert_eq!(result.total, 1);
        let hit = result.hits[0];
        assert_eq!(index.name(hit), Some("invoice-march.pdf"));
        assert_eq!(index.size(hit), Some(5));
        assert_eq!(
            PathBuf::from(index.path(hit).unwrap()),
            tree.0
                .join("Finance")
                .join("2026")
                .join("invoice-march.pdf")
        );
    }

    #[test]
    fn the_same_tree_gives_the_same_order() {
        let tree = TempTree::new();
        for name in ["b.txt", "a.txt", "c.txt"] {
            tree.file(name, "");
        }
        let (index, _) = scan_dir(&tree.0).unwrap();
        let names: Vec<_> = (1..4).map(|id| index.name(id).unwrap()).collect();
        assert_eq!(names, ["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn refuses_a_missing_folder_or_a_file() {
        let tree = TempTree::new();
        tree.file("plain.txt", "");
        assert!(matches!(
            scan_dir(&tree.0.join("nope")),
            Err(ScanError::Root(_))
        ));
        assert!(matches!(
            scan_dir(&tree.0.join("plain.txt")),
            Err(ScanError::Root(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_symbolic_links() {
        let tree = TempTree::new();
        tree.file("real/file.txt", "x");
        // A link back to the top would loop forever if it were followed.
        std::os::unix::fs::symlink(&tree.0, tree.0.join("real").join("loop")).unwrap();

        let (index, stats) = scan_dir(&tree.0).unwrap();
        assert_eq!(stats.dirs, 2);
        assert_eq!(stats.files, 2);
        let link = index.search(&Query::parse("loop"), 10);
        assert_eq!(link.total, 1);
        assert_eq!(index.kind(link.hits[0]), Some(Kind::File));
    }
}
