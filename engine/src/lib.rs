//! The file index and search engine behind Seekvo.
//!
//! - [`IndexBuilder`] and [`Index`] hold every file and folder in flat
//!   arrays and answer searches over them.
//! - [`Query`] parses what a person typed.
//! - [`ntfs::read_volume`] fills an index from a whole NTFS drive at once by
//!   asking Windows for its file table. Windows only, administrators only.
//! - [`scan::scan_dir`] fills an index by walking a folder. It works on any
//!   system and needs no special rights, and is the slower path.
//! - [`synthetic`] builds a made-up drive of any size for benchmarks.

pub mod index;
pub mod mft;
pub mod ntfs;
pub mod query;
pub mod scan;
pub mod synthetic;

pub use index::{BuildError, EntryId, Index, IndexBuilder, Kind, SearchResult};
pub use query::{Query, Scope};
