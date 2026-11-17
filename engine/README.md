# Seekvo engine

The file index and search engine behind Seekvo, written in Rust. This is the first piece of the
app itself.

## What works today

- **An in-memory index** of files and folders: name, parent folder, size and extension.
- **Search** across that index: several words in any order, any letter case, and an `ext:` filter.
- **A folder scanner** that fills the index by walking a folder. It runs on Windows, macOS and
  Linux.
- **A command-line tool** to try it on your own folders and to run the benchmark.

Not built yet: reading the NTFS Master File Table (the fast way to index a whole Windows drive),
following changes live, saving the index to disk, and ranking results.

## Try it

You need Rust 1.88 or newer.

```bash
cd engine

# Index a folder, then search it
cargo run --release -- scan C:\Users\you\Documents invoice 2026

# Only PDF files with "report" in the path
cargo run --release -- scan C:\Users\you\Documents report ext:pdf

# Time searches on a made-up drive with a million files
cargo run --release -- bench
```

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

## Benchmark

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

These numbers time the search over an index that is already in memory. They do not cover reading
a real drive, which is the next piece of work. Your machine will give different numbers.

## Checks

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## What comes next

1. Read the NTFS Master File Table on Windows, so a whole drive is indexed in seconds.
2. Follow the NTFS change journal, so new and renamed files show up without a rescan.
3. Save the index to disk and load it at startup.
4. Rank results, so the most likely file comes first.
