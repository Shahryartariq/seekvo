//! The file index and search engine behind Seekvo.
//!
//! - [`IndexBuilder`] and [`Index`] hold every file and folder in flat
//!   arrays and answer searches over them.
//! - [`Query`] parses what a person typed.
//! - [`scan::scan_dir`] fills an index by walking a folder. It works on any
//!   system and is the slow path: on Windows the plan is to read the NTFS
//!   Master File Table instead.
//! - [`synthetic`] builds a made-up drive of any size for benchmarks.

pub mod index;
pub mod query;
pub mod scan;
pub mod synthetic;

pub use index::{BuildError, EntryId, Index, IndexBuilder, Kind, SearchResult};
pub use query::{Query, Scope};
