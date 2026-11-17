# Seekvo engine

The file index and search engine behind Seekvo, written in Rust. This is the first piece of the
app itself.

## What works today

- **An in-memory index** of files and folders: name, parent folder, size and extension.
- **Search** across that index: several words in any order, any letter case, and an `ext:` filter.
- **A whole-drive reader for NTFS.** It asks Windows for the drive's file table in bulk instead of
  walking folders. On a real Windows drive with 1.35 million files and folders it built the index
  in 3.7 seconds.
- **A folder scanner** that fills the index by walking a folder. It runs on Windows, macOS and
  Linux and needs no special rights.
- **A command-line tool** to try it on your own drive and folders and to run the benchmark.

Not built yet: file sizes when reading a whole drive, following changes live, saving the index to
disk, and ranking results.

## Try it

You need Rust 1.88 or newer.

```bash
cd engine

# Index the whole C: drive, then search it.
# Windows only, from an administrator terminal.
cargo run --release -- drive C invoice 2026

# Index one folder, then search it. Works anywhere, no administrator needed.
cargo run --release -- scan C:\Users\you\Documents report ext:pdf

# Time searches on a made-up drive with a million files
cargo run --release -- bench
```

Reading a drive's file table is something Windows only allows administrators to do. The tool
only reads; it never writes to the drive.

## How a search matches

- Every word must appear somewhere in the path. `invoice 2026` finds `invoice-march.pdf` inside a
  folder called `2026`.
- At least one word must appear in the entry's own name. Searching `photos` returns the `Photos`
  folder, not the 40,000 pictures inside it. Pass `--path` to get everything inside as well.
- `ext:pdf` keeps only files with that extension.

## How it is built

A million files stored as a million small objects means a million places in memory to visit.
Seekvo stores them the other way round:

- **One array per field.** Parents, sizes and extensions each sit in their own flat array, so a
  search reads memory front to back.
- **All names in one buffer.** Every lowercase name sits end to end with a zero byte between
  names. Finding a word is one substring search over that buffer, using the SIMD search from the
  `memchr` crate. After a hit the search jumps to the next name.
- **Parents before children.** A folder is always stored before what it contains. So one forward
  pass is enough to carry "this folder's name matched" down to everything inside it, with no
  recursion and no path strings built during the search.

The code is in `src/index.rs`. A test checks the fast search against a slow, obvious one on a
few thousand files and many queries.

### Reading a whole drive

NTFS keeps one record per file in its Master File Table, and Windows will hand those records
over a megabyte at a time (`FSCTL_ENUM_USN_DATA`). Each record has the file's name and the
number of its parent folder, in table order rather than folder order. `src/ntfs.rs` sorts the
records by parent and walks down from the top folder, so parents land in the index before their
children. The parsing and the ordering are plain Rust and are tested on every system, including
with damaged buffers and records that point at each other in a loop. Only four Windows calls
are platform-specific.

Two limits for now: these records carry no file sizes, so sizes read as zero, and a file with
several hard links is listed under one of its names.

## Benchmark

### A real drive

Measured in CI on GitHub's hosted Windows runner (`drive C`, release build):

| | |
| --- | --- |
| Drive | C:, NTFS, 1,136,635 files and 208,527 folders |
| Time to index the whole drive | 3.7 seconds |
| Memory held by the index | 97 MB |
| Search for `system32` (723 matches) | 5.7 ms |

Every pull request repeats this run, and the result line is attached to the CI run as a notice.

### A made-up drive

`cargo run --release -- bench` on a 2-core cloud VM (Intel Xeon, 2.3 GHz), one thread:

A made-up drive with 1,117,955 entries, 53 MB of memory, built in 115 ms.

| Search | Matches | Median | Slowest of 30 |
| --- | ---: | ---: | ---: |
| `invoice` | 40,404 | 3.2 ms | 6.6 ms |
| `invoice march 2026` | 125 | 7.7 ms | 11.3 ms |
| `react index` | 3,368 | 5.5 ms | 5.6 ms |
| `node_modules react --path` | 10,104 | 4.9 ms | 5.0 ms |
| `ext:jpg` | 673,400 | 1.1 ms | 1.4 ms |
| `img_10 hunza` | 1,450 | 5.1 ms | 7.9 ms |
| `statement ext:pdf` | 40,404 | 3.2 ms | 3.6 ms |
| `e` | 368,437 | 4.7 ms | 4.9 ms |
| `zzzz` | 0 | 2.4 ms | 2.7 ms |

Your machine will give different numbers.

## Checks

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## What comes next

1. File sizes for whole-drive indexing, read from the file table itself.
2. Follow the NTFS change journal, so new and renamed files show up without a rescan.
3. Save the index to disk and load it at startup.
4. Rank results, so the most likely file comes first.
