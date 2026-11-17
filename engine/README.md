# Seekvo engine

The file index and search engine behind Seekvo, written in Rust. This is the first piece of the
app itself.

## What works today

- **An in-memory index** of files and folders: name, parent folder, size and extension.
- **Search** across that index: several words in any order, any letter case, and an `ext:` filter.
- **A whole-drive reader for NTFS.** It reads the drive's file table directly instead of walking
  folders, and gets every file's size and every name of a hard-linked file. On a real Windows
  drive with 1.4 million files and folders the first read took about 10 seconds.
- **An image reader.** The same code indexes an NTFS volume saved as an image file, on any
  system.
- **A folder scanner** that fills the index by walking a folder. It runs on Windows, macOS and
  Linux and needs no special rights.
- **A command-line tool** to try all of this and to run the benchmark.

Not built yet: following changes live, saving the index to disk, and ranking results.

## Try it

You need Rust 1.88 or newer.

```bash
cd engine

# Index the whole C: drive, then search it.
# Windows only, from an administrator terminal.
cargo run --release -- drive C invoice 2026

# Index one folder, then search it. Works anywhere, no administrator needed.
cargo run --release -- scan C:\Users\you\Documents report ext:pdf

# Index an NTFS volume saved as an image file. Works anywhere.
cargo run --release -- image disk.img report ext:pdf

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

NTFS keeps one fixed-size record per file in its Master File Table, and the table is itself a
file on the drive. `src/mft.rs` reads it without going through the file system:

1. The drive's first sector says where the table starts and how big a record is.
2. The table's own first record lists the stretches of the drive it is stored in. On a
   fragmented drive that list continues in further records, which the reader follows.
3. Each record is checked (NTFS marks the end of every 512 bytes so a half-written record can be
   told apart) and then read for its names, its parent folder and the size of its data.

The records come out in table order, each naming its parent by number. `src/ntfs.rs` sorts them
by parent and walks down from the top folder, so parents land in the index before their
children.

Everything read from the drive is treated as untrusted. A record that fails its check is
skipped and counted. The tests run the reader on the file table of a real volume (formatted
with `mkntfs`, filled through `ntfs-3g`; see `tests/fixtures/`), on a hand-made volume for cases
a healthy one does not have, and on the real table with random bytes flipped, 400 times over,
to check that damage produces an error or a shorter listing and never a crash. On Windows a
further test reads the real system drive and compares file sizes with what Windows reports.

Two things to know:

- The drive is read below the file system, so a file created in the last few seconds may not
  have reached the table on disk yet. Following the change journal, which is next, closes that
  gap.
- `--reader records` uses the earlier method, which asks Windows for a summary of each record
  (`FSCTL_ENUM_USN_DATA`). It has no sizes and one name per file.

## Benchmark

### A real drive

Measured in CI on GitHub's hosted Windows runner (`drive C`, release build), on its C: drive:
NTFS, 1,201,656 file names and 208,530 folders. Three runs on separate machines:

| | First read | Read again |
| --- | ---: | ---: |
| Reading the file table off the drive | 8.7, 9.0 and 11.0 s | 3.2, 5.8 and 3.2 s |
| Arranging the records into the index | 0.8 s | 0.8 s |
| **Total** | **9.5, 9.8 and 11.9 s** | **4.0, 6.5 and 4.0 s** |

The index holds 102 MB of memory, and a search for `system32` (723 matches) took about 5 ms.

The runner's drive is a cloud virtual disk, and the table on it is about 1.4 GB. It has not been
measured on a PC with a local SSD yet. The earlier reader (`--reader records`) took 36 to 42
seconds when it was the first thing to read the same drive, and about 3.5 seconds once Windows
had the table in memory.

Every pull request repeats these reads, and the result lines are attached to the CI run as
notices.

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

1. Follow the NTFS change journal, so new and renamed files show up without a rescan.
2. Save the index to disk and load it at startup.
3. Rank results, so the most likely file comes first.
4. Add up folder sizes, which the disk map needs.
