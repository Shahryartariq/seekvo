# Test fixtures

`small-ntfs.bin` holds the parts of a real 4 MB NTFS volume that the file-table reader
touches: the first sector and the Master File Table. The volume was formatted with `mkntfs` and
filled through the `ntfs-3g` driver, so the records are what a real NTFS implementation wrote,
not something built by this project's own code.

What is on the volume:

| Path | Why it is there |
| --- | --- |
| `Docs/a.txt` and `link-to-a.txt` | One file with two names (a hard link), 5 bytes |
| `Docs/Über 📷.jpg` | A name outside ASCII, with a character that needs two UTF-16 units |
| `Docs/Reports/2026/report-q3.pdf` | Nested folders |
| `big.bin` | 300,000 bytes, stored outside its record |
| `sparse.bin` | 5,000,000 bytes that take no space on the volume |
| `Many/file-1.dat` … `file-90.dat` | Enough files to grow the table |
| `deleted.txt`, `Pads/` | Created and deleted again; their records are still in the table, marked unused |

The table itself is in three pieces.

To rebuild it (Linux, root, `ntfs-3g` installed):

```bash
sudo tests/fixtures/make-image.sh /tmp/small-ntfs.img
cargo run --example make_fixture -- /tmp/small-ntfs.img tests/fixtures/small-ntfs.bin
```

File format: the line `SEEKVO-IMAGE-1`, then one piece after another. Each piece is an offset
(u64), a length (u32) and that many bytes. Numbers are little-endian.
