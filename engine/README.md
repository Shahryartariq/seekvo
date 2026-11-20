# Seekvo engine

The file index and search engine behind Seekvo, written in Rust. This is the first piece of the
app itself.

## What works today

- **An in-memory index** of files and folders: name, parent folder, size and extension.
- **Search** across that index: several words in any order, any letter case, and an `ext:` filter.
- **A whole-drive reader for NTFS.** It reads the drive's file table directly instead of walking
  folders, and gets every file's size and every name of a hard-linked file. On a real Windows
  drive with 1.4 million files and folders the first read took 9 to 14 seconds in eight runs
  out of nine.
- **Following changes.** After the first read the index follows the drive's change journal. A
  file that is created, renamed, moved, resized or deleted is in the index at the next poll,
  with no rescan.
- **A saved index.** The index can be written to a file. The next start loads it and catches up
  from the change journal instead of reading the drive: about a second on the same drive.
- **An image reader.** The same code indexes an NTFS volume saved as an image file, on any
  system.
- **A folder scanner** that fills the index by walking a folder. It runs on Windows, macOS and
  Linux and needs no special rights.
- **A command-line tool** to try all of this and to run the benchmark.

Not built yet: ranking results, and updating the index in place when a file changes (today it
is rebuilt, which takes most of a second on a big drive).

## Try it

You need Rust 1.88 or newer.

```bash
cd engine

# Index the whole C: drive, then search it.
# Windows only, from an administrator terminal.
cargo run --release -- drive C invoice 2026

# Index C:, then keep the index current and repeat the search whenever a file changes.
# With --saved, the next start loads the index from that file instead of reading the drive.
cargo run --release -- watch C invoice 2026 --saved c-drive.seekvo

# Index one folder, then search it. Works anywhere, no administrator needed.
cargo run --release -- scan C:\Users\you\Documents report ext:pdf

# Index an NTFS volume saved as an image file. Works anywhere.
cargo run --release -- image disk.img report ext:pdf

# Time searches on a made-up drive with a million files
cargo run --release -- bench
```

Reading a drive's file table is something Windows only allows administrators to do. The tool
only reads the drive. The one thing it writes is the saved index, and only to the file you name.

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

`--reader records` uses the earlier method, which asks Windows for a summary of each record
(`FSCTL_ENUM_USN_DATA`). It has no sizes and one name per file.

### Staying current

NTFS writes a line to its change journal every time a file is created, renamed, moved, written
to or deleted. `src/live.rs` reads the journal from where it last stopped. That costs almost
nothing when nothing happened, however large the drive is.

The journal says which records changed. For each one the engine asks Windows for the record as
it is now (`FSCTL_GET_NTFS_FILE_RECORD`) and replaces what it knew about that file. It does not
try to replay "this was a rename, that was a delete". One step covers a new file, a new name, a
move, a second name, a new size and a delete, and a journal line that is read twice does no
harm. Renaming a folder touches one record; everything inside follows because children point
to their folder by number.

The first read has a gap of its own: the table is read below the file system, so a file saved
a few seconds earlier may not be in the table on disk yet. To close it, every record the
journal mentions from two minutes before the read began is looked at again once the read is
done.

### Starting without reading the drive

`src/snapshot.rs` writes the records to a file together with the place in the journal they are
current to. The next start loads the file and reads the journal from that place, which brings
in everything that happened while nothing was watching.

The saved file is used only when it can be trusted. It carries a check value, the drive's serial
number and the journal's id. If the file is missing, cut short, changed, from another drive, or
older than the journal goes back, the drive is read in full instead. The tests cut the file at
every length and change every byte to make sure each case is refused.

The tests for all of this run on any system against a made-up drive and journal. On Windows, CI
does it for real: it changes files on the runner's system drive (create, grow, rename, move,
rename the folder, add a second name, delete) and checks the index after each step, then stops
watching, changes more files, and starts again from the saved index.

Things to know:

- A change is searchable after the index is rebuilt, which took 0.7 to 1.0 seconds on the test
  drive. That is fine as a background step, but updating the index in place is the better
  design and comes next.
- A drive changed by another system that does not write the journal (Linux writing to an NTFS
  drive, for example) will have changes the saved index does not know about.
- Windows keeps a limited amount of journal. After a long time away on a busy drive the journal
  no longer goes back far enough, and the start falls back to a full read.

## Benchmark

### A real drive

Measured in CI on GitHub's hosted Windows runner (release build), on its C: drive: NTFS, about
1.2 million file names and 208,500 folders. Each run is on a separate machine.

Reading the whole drive (`drive C`), nine runs:

| | First read | Read again |
| --- | ---: | ---: |
| Reading the file table off the drive | 8.3 to 13.3 s in eight runs, 3.4 s in one | 3.2 to 5.8 s |
| Arranging the records into the index | 0.7 to 1.0 s | 0.7 to 1.0 s |
| **Total** | **9.0 to 14.2 s** in eight runs, 4.3 s in one | **4.0 to 6.5 s** |

The one fast first read was as quick as a second read, so that runner had most likely read
the table before the test began. The index holds 102 MB of memory, and a search for `system32` (629 to 723 matches) took 4 to
5 ms.

Following changes and starting from a saved index (`watch C --saved`), four or five runs each. The
machines differ in speed, so these are ranges:

| | |
| --- | ---: |
| Taking in a handful of changed files | 0.1 to 0.3 ms |
| Rebuilding the index so they can be searched | 0.7 to 1.0 s |
| Saving the index (a 69 MB file) | 0.30 to 0.31 s |
| Starting from the saved index, catching up included | 0.8 to 1.2 s over eight starts |

The start from the saved index was timed right after the file was written, so Windows still had
it in memory, and only a few files had changed in between. A start after a restart, or after a
day of changes, has not been measured.

The runner's drive is a cloud virtual disk, and the table on it is about 1.4 GB. None of this
has been measured on a PC with a local SSD yet. The earlier reader (`--reader records`) took 36
to 42 seconds when it was the first thing to read the same drive, and 2.9 to 3.5 seconds once
Windows had the table in memory.

Every pull request repeats these steps, and the result lines are attached to the CI run as
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

1. Update the index in place when a file changes, instead of rebuilding it.
2. Rank results, so the most likely file comes first.
3. Add up folder sizes, which the disk map needs.
4. Measure all of it on a PC with a local SSD.
