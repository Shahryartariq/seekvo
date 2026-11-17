//! Reading the NTFS Master File Table straight off the drive.
//!
//! The table is itself a file, `$MFT`, made of fixed-size records, one per
//! file or folder. This module finds it from the drive's first sector,
//! follows the list of places it is stored, and reads every record. Unlike
//! the records Windows hands out through `FSCTL_ENUM_USN_DATA`, these carry
//! file sizes and every name of a hard-linked file.
//!
//! Nothing here is Windows-specific. The drive is anything that can read
//! bytes at an offset ([`BlockSource`]), so the same code runs against a
//! real volume on Windows and against an image file in the tests.
//!
//! Everything read from the drive is treated as untrusted: a record that
//! does not check out is skipped and counted, never trusted.

use std::fmt;
use std::io;

use crate::ntfs::VolumeRecords;

/// Something that can read `buffer.len()` bytes starting at `offset`.
///
/// Reads are always a whole number of sectors at a sector boundary, which
/// is what a raw Windows volume requires.
pub trait BlockSource {
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> io::Result<()>;
}

/// An image of a volume saved as an ordinary file.
impl BlockSource for std::fs::File {
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        self.seek(SeekFrom::Start(offset))?;
        self.read_exact(buffer)
    }
}

/// Why the file table could not be read.
#[derive(Debug)]
pub enum MftError {
    Io(io::Error),
    /// The first sector does not belong to an NTFS volume.
    NotNtfs,
    /// The first sector says NTFS but its numbers make no sense.
    BadBootSector(&'static str),
    /// The table's own first record is unreadable, so there is nowhere to
    /// start.
    BadTable(&'static str),
}

impl fmt::Display for MftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MftError::Io(error) => write!(f, "cannot read the drive: {error}"),
            MftError::NotNtfs => write!(f, "the drive is not formatted as NTFS"),
            MftError::BadBootSector(what) => {
                write!(f, "the drive's first sector is damaged: {what}")
            }
            MftError::BadTable(what) => write!(f, "the drive's file table is damaged: {what}"),
        }
    }
}

impl std::error::Error for MftError {}

impl From<io::Error> for MftError {
    fn from(error: io::Error) -> Self {
        MftError::Io(error)
    }
}

/// What came out of reading a file table.
#[derive(Debug)]
pub struct FileTable {
    pub records: VolumeRecords,
    /// Records in use, before counting extra names of hard-linked files.
    pub in_use: usize,
    /// Records that failed their integrity check and were skipped, usually
    /// because they were being written while the scan ran.
    pub damaged: usize,
}

const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// NTFS protects each 512 bytes of a record with a check value, whatever
/// the drive's real sector size is.
const FIXUP_STRIDE: usize = 512;
const ATTRIBUTE_FILE_NAME: u32 = 0x30;
const ATTRIBUTE_DATA: u32 = 0x80;
const ATTRIBUTE_END: u32 = 0xFFFF_FFFF;
const RECORD_IN_USE: u16 = 0x1;
const RECORD_IS_DIRECTORY: u16 = 0x2;
/// The short 8.3 name kept for old programs; the long name is listed too.
const NAMESPACE_DOS: u8 = 2;
/// How much of the table to read per request.
const CHUNK_BYTES: usize = 4 << 20;
const FLAG_IN_USE: u8 = 1;
const FLAG_DIRECTORY: u8 = 2;

fn bytes_at<const N: usize>(buffer: &[u8], at: usize) -> Option<[u8; N]> {
    buffer.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn u16_at(buffer: &[u8], at: usize) -> Option<u16> {
    bytes_at(buffer, at).map(u16::from_le_bytes)
}

fn u32_at(buffer: &[u8], at: usize) -> Option<u32> {
    bytes_at(buffer, at).map(u32::from_le_bytes)
}

fn u64_at(buffer: &[u8], at: usize) -> Option<u64> {
    bytes_at(buffer, at).map(u64::from_le_bytes)
}

/// The numbers from the first sector that say where the table is and how
/// big its pieces are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    bytes_per_sector: u64,
    cluster_size: u64,
    record_size: usize,
    mft_offset: u64,
    volume_bytes: u64,
}

fn parse_boot_sector(sector: &[u8]) -> Result<Geometry, MftError> {
    if sector.get(3..11) != Some(b"NTFS    ") {
        return Err(MftError::NotNtfs);
    }
    let bad = MftError::BadBootSector;

    let bytes_per_sector = u64::from(u16_at(sector, 0x0B).ok_or(bad("too short"))?);
    if !(512..=4096).contains(&bytes_per_sector) || !bytes_per_sector.is_power_of_two() {
        return Err(bad("sector size"));
    }

    // Up to 128 this is a plain count. Larger values are a negative power
    // of two, used for clusters bigger than 64 KB.
    let raw = *sector.get(0x0D).ok_or(bad("too short"))?;
    let sectors_per_cluster: u64 = if raw <= 0x80 {
        u64::from(raw)
    } else {
        let shift = 256 - u32::from(raw);
        if shift > 16 {
            return Err(bad("cluster size"));
        }
        1 << shift
    };
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
        return Err(bad("cluster size"));
    }
    let cluster_size = bytes_per_sector * sectors_per_cluster;

    // Positive: a count of clusters. Negative: a power of two in bytes.
    let raw = *sector.get(0x40).ok_or(bad("too short"))? as i8;
    let record_size: u64 = if raw > 0 {
        u64::from(raw.unsigned_abs()) * cluster_size
    } else {
        let shift = u32::from(raw.unsigned_abs());
        if shift > 16 {
            return Err(bad("record size"));
        }
        1 << shift
    };
    if !(512..=65_536).contains(&record_size) || !record_size.is_power_of_two() {
        return Err(bad("record size"));
    }

    let mft_cluster = u64_at(sector, 0x30).ok_or(bad("too short"))?;
    let mft_offset = mft_cluster
        .checked_mul(cluster_size)
        .ok_or(bad("table position"))?;

    let volume_bytes = u64_at(sector, 0x28)
        .and_then(|sectors| sectors.checked_mul(bytes_per_sector))
        .ok_or(bad("volume size"))?;
    if mft_offset >= volume_bytes {
        return Err(bad("table position"));
    }

    Ok(Geometry {
        bytes_per_sector,
        cluster_size,
        record_size: record_size as usize,
        mft_offset,
        volume_bytes,
    })
}

/// A stretch of clusters holding part of a file. `cluster` is `None` for a
/// hole that takes no space on the drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    cluster: Option<u64>,
    clusters: u64,
}

/// Decode a file's list of runs.
///
/// Each run is a header byte giving the sizes of two numbers, then a length
/// in clusters and a signed distance from the previous run's start. A zero
/// header ends the list. Returns `None` if the list is malformed.
fn decode_runs(bytes: &[u8]) -> Option<Vec<Run>> {
    let mut runs = Vec::new();
    let mut at = 0usize;
    let mut previous: i64 = 0;
    loop {
        let header = *bytes.get(at)?;
        if header == 0 {
            return Some(runs);
        }
        at += 1;
        let length_bytes = usize::from(header & 0x0F);
        let offset_bytes = usize::from(header >> 4);
        if length_bytes == 0 || length_bytes > 8 || offset_bytes > 8 {
            return None;
        }

        let mut clusters: u64 = 0;
        for (i, &byte) in bytes.get(at..at + length_bytes)?.iter().enumerate() {
            clusters |= u64::from(byte) << (8 * i);
        }
        at += length_bytes;
        if clusters == 0 {
            return None;
        }

        let cluster = if offset_bytes == 0 {
            None
        } else {
            let field = bytes.get(at..at + offset_bytes)?;
            let mut delta: i64 = 0;
            for (i, &byte) in field.iter().enumerate() {
                delta |= i64::from(byte) << (8 * i);
            }
            // Sign-extend from the field's top bit.
            let unused_bits = 64 - 8 * offset_bytes as u32;
            delta = (delta << unused_bits) >> unused_bits;
            at += offset_bytes;
            previous = previous.checked_add(delta)?;
            Some(u64::try_from(previous).ok()?)
        };
        runs.push(Run { cluster, clusters });
    }
}

/// Undo the per-sector protection on a record, checking it on the way.
///
/// NTFS overwrites the last two bytes of every 512 with a counter and keeps
/// the real bytes in a small array near the start. If a write was cut off
/// half-way, some sector carries the wrong counter. Returns false then.
fn apply_fixups(record: &mut [u8]) -> bool {
    let (Some(array_at), Some(count)) = (u16_at(record, 4), u16_at(record, 6)) else {
        return false;
    };
    let (array_at, count) = (usize::from(array_at), usize::from(count));
    if count < 2 || (count - 1) * FIXUP_STRIDE > record.len() {
        return false;
    }
    let Some(counter) = bytes_at::<2>(record, array_at) else {
        return false;
    };
    for sector in 1..count {
        let Some(saved) = bytes_at::<2>(record, array_at + 2 * sector) else {
            return false;
        };
        let end = sector * FIXUP_STRIDE;
        if record[end - 2..end] != counter {
            return false;
        }
        record[end - 2..end].copy_from_slice(&saved);
    }
    true
}

/// The unnamed data attribute of a record, as far as this module needs it.
enum Data {
    /// Small enough to sit inside the record.
    Inside { size: u64 },
    /// Stored elsewhere. `size` is only known in the piece that starts at
    /// the beginning of the file.
    Outside {
        first_cluster_index: u64,
        size: u64,
        runs: Option<Vec<Run>>,
    },
}

fn parse_data(attribute: &[u8]) -> Option<Data> {
    if attribute.get(8) == Some(&0) {
        return Some(Data::Inside {
            size: u64::from(u32_at(attribute, 0x10)?),
        });
    }
    let first_cluster_index = u64_at(attribute, 0x10)?;
    let runs_at = usize::from(u16_at(attribute, 0x20)?);
    let real_size = u64_at(attribute, 0x30)?;
    Some(Data::Outside {
        first_cluster_index,
        size: real_size,
        runs: attribute.get(runs_at..).and_then(decode_runs),
    })
}

/// Walk the attributes of a checked record, calling `visit` with each
/// attribute's type and bytes.
fn for_each_attribute(record: &[u8], mut visit: impl FnMut(u32, &[u8])) {
    let used = u32_at(record, 0x18).map_or(record.len(), |used| (used as usize).min(record.len()));
    let Some(mut at) = u16_at(record, 0x14).map(usize::from) else {
        return;
    };
    while let Some(kind) = u32_at(record, at) {
        if kind == ATTRIBUTE_END {
            break;
        }
        let Some(length) = u32_at(record, at + 4).map(|length| length as usize) else {
            break;
        };
        let Some(end) = at.checked_add(length) else {
            break;
        };
        if length < 24 || end > used {
            break;
        }
        visit(kind, &record[at..end]);
        at = end;
    }
}

/// Everything gathered while reading the table.
struct Scan {
    records: VolumeRecords,
    /// How many records the table says it has. Nothing beyond is believed.
    total_records: usize,
    /// Per record number: in use, and whether it is a folder. Grows as
    /// records are seen, so a damaged size field cannot make it huge.
    flags: Vec<u8>,
    /// Per record number: size of the unnamed data, in bytes.
    sizes: Vec<u64>,
    /// Further pieces of the table itself, found in its overflow records:
    /// the cluster index each piece starts at, and its runs.
    table_pieces: Vec<(u64, Vec<Run>)>,
    in_use: usize,
    damaged: usize,
}

impl Scan {
    fn new(total_records: usize) -> Self {
        Scan {
            records: VolumeRecords::new(),
            total_records,
            flags: Vec::new(),
            sizes: Vec::new(),
            table_pieces: Vec::new(),
            in_use: 0,
            damaged: 0,
        }
    }

    /// The place in the per-record tables for record `number`, making room
    /// if needed. `None` if the number is outside the table or there is no
    /// memory left for it.
    fn slot(&mut self, number: u64) -> Option<usize> {
        let slot = usize::try_from(number)
            .ok()
            .filter(|&slot| slot < self.total_records)?;
        if slot >= self.flags.len() {
            let more = slot + 1 - self.flags.len();
            self.flags.try_reserve(more).ok()?;
            self.sizes.try_reserve(more).ok()?;
            self.flags.resize(slot + 1, 0);
            self.sizes.resize(slot + 1, 0);
        }
        Some(slot)
    }

    /// Take in one record. `number` is its position in the table.
    fn record(&mut self, record: &mut [u8], number: u64) {
        if record.get(..4) != Some(b"FILE") {
            // Never used, or marked bad by NTFS itself.
            return;
        }
        if !apply_fixups(record) {
            self.damaged += 1;
            return;
        }
        let record = &*record;
        let Some(flags) = u16_at(record, 0x16) else {
            return;
        };
        if flags & RECORD_IN_USE == 0 {
            return;
        }

        // A file with too many attributes for one record spills into
        // overflow records, each pointing back at the main one.
        let base = u64_at(record, 0x20).unwrap_or(0);
        let is_overflow = base != 0;
        let owner = if is_overflow {
            base & RECORD_NUMBER_MASK
        } else {
            number
        };
        let Some(slot) = self.slot(owner) else {
            return;
        };
        if !is_overflow {
            self.in_use += 1;
            self.flags[slot] = FLAG_IN_USE
                | if flags & RECORD_IS_DIRECTORY != 0 {
                    FLAG_DIRECTORY
                } else {
                    0
                };
        }

        for_each_attribute(record, |kind, attribute| {
            let is_resident = attribute.get(8) == Some(&0);
            let name_length = attribute.get(9).copied().unwrap_or(0);
            match kind {
                ATTRIBUTE_FILE_NAME if is_resident => self.file_name(attribute, owner),
                ATTRIBUTE_DATA if name_length == 0 => match parse_data(attribute) {
                    Some(Data::Inside { size }) => self.sizes[slot] = size,
                    Some(Data::Outside {
                        first_cluster_index: 0,
                        size,
                        ..
                    }) => self.sizes[slot] = size,
                    Some(Data::Outside {
                        first_cluster_index,
                        runs: Some(runs),
                        ..
                    }) if owner == 0 => self.table_pieces.push((first_cluster_index, runs)),
                    _ => {}
                },
                _ => {}
            }
        });
    }

    fn file_name(&mut self, attribute: &[u8], owner: u64) {
        let (Some(length), Some(at)) = (u32_at(attribute, 0x10), u16_at(attribute, 0x14)) else {
            return;
        };
        let at = usize::from(at);
        let Some(value) = at
            .checked_add(length as usize)
            .and_then(|end| attribute.get(at..end))
        else {
            return;
        };
        let (Some(parent), Some(&characters), Some(&namespace)) =
            (u64_at(value, 0), value.get(0x40), value.get(0x41))
        else {
            return;
        };
        if namespace == NAMESPACE_DOS {
            return;
        }
        let Some(name) = value.get(0x42..0x42 + 2 * usize::from(characters)) else {
            return;
        };
        let units = name
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
        // Folder or file, and the size, are filled in once the whole table
        // has been read.
        self.records.push_utf16(owner, parent, units, false);
    }

    fn finish(mut self) -> FileTable {
        let (flags, sizes) = (&self.flags, &self.sizes);
        self.records.resolve(|record| {
            let slot = usize::try_from(record).ok()?;
            let flag = *flags.get(slot)?;
            if flag & FLAG_IN_USE == 0 {
                return None;
            }
            Some((flag & FLAG_DIRECTORY != 0, sizes[slot]))
        });
        FileTable {
            records: self.records,
            in_use: self.in_use,
            damaged: self.damaged,
        }
    }
}

fn round_up(value: usize, multiple: usize) -> usize {
    value.div_ceil(multiple) * multiple
}

/// Read the whole file table of an NTFS volume.
pub fn read_file_table(source: &mut dyn BlockSource) -> Result<FileTable, MftError> {
    // 4096 bytes is a whole number of sectors for every sector size NTFS
    // supports, and no NTFS volume is smaller.
    let mut boot = vec![0u8; 4096];
    source.read_at(0, &mut boot)?;
    let geometry = parse_boot_sector(&boot)?;
    let record_size = geometry.record_size;
    let sector = geometry.bytes_per_sector as usize;

    // The table's first record describes the table itself.
    let mut first = vec![0u8; round_up(record_size, sector)];
    source.read_at(geometry.mft_offset, &mut first)?;
    let first = &mut first[..record_size];
    if first.get(..4) != Some(b"FILE") || !apply_fixups(first) {
        return Err(MftError::BadTable("its first record does not check out"));
    }
    let mut own_data = None;
    for_each_attribute(first, |kind, attribute| {
        if kind == ATTRIBUTE_DATA && attribute.get(9) == Some(&0) && own_data.is_none() {
            own_data = parse_data(attribute);
        }
    });
    let Some(Data::Outside {
        first_cluster_index: 0,
        size: table_bytes,
        runs: Some(first_runs),
    }) = own_data
    else {
        return Err(MftError::BadTable("it does not say where it is stored"));
    };

    // The table cannot be bigger than the volume it is on, and NTFS itself
    // stops at four billion records.
    if table_bytes > geometry.volume_bytes {
        return Err(MftError::BadTable("it claims to be bigger than the volume"));
    }
    let total_records = usize::try_from(table_bytes / record_size as u64)
        .ok()
        .filter(|&records| records <= u32::MAX as usize)
        .ok_or(MftError::BadTable("it claims an impossible size"))?;
    let table_bytes = total_records as u64 * record_size as u64;

    let mut scan = Scan::new(total_records);
    // Room for one full request plus a record cut in two by the end of the
    // previous one.
    let mut buffer = vec![0u8; CHUNK_BYTES + record_size];
    let mut filled = 0usize;
    // Position in the table of `buffer[0]`.
    let mut position = 0u64;
    let mut next_cluster_index = 0u64;
    let mut piece = Some(first_runs);

    'table: while let Some(runs) = piece.take() {
        for run in runs {
            let run_bytes = run
                .clusters
                .checked_mul(geometry.cluster_size)
                .ok_or(MftError::BadTable("a piece of it has an impossible size"))?;
            next_cluster_index += run.clusters;

            let Some(cluster) = run.cluster else {
                // A hole: nothing stored, so no records. Whatever was
                // waiting for its second half will never get it.
                position += filled as u64 + run_bytes;
                filled = 0;
                continue;
            };
            let mut offset =
                cluster
                    .checked_mul(geometry.cluster_size)
                    .ok_or(MftError::BadTable(
                        "a piece of it is at an impossible place",
                    ))?;
            let mut left = run_bytes;
            while left > 0 {
                if position >= table_bytes {
                    break 'table;
                }
                let want = left.min(CHUNK_BYTES as u64) as usize;
                source.read_at(offset, &mut buffer[filled..filled + want])?;
                filled += want;
                offset += want as u64;
                left -= want as u64;

                let whole = filled / record_size;
                for i in 0..whole {
                    let start = position + (i * record_size) as u64;
                    if start >= table_bytes {
                        break;
                    }
                    let record = &mut buffer[i * record_size..(i + 1) * record_size];
                    scan.record(record, start / record_size as u64);
                }
                let consumed = whole * record_size;
                buffer.copy_within(consumed..filled, 0);
                filled -= consumed;
                position += consumed as u64;
            }
        }

        // A badly fragmented table lists its later pieces in overflow
        // records, which were among the records just read.
        if let Some(found) = scan
            .table_pieces
            .iter()
            .position(|(starts_at, _)| *starts_at == next_cluster_index)
        {
            piece = Some(scan.table_pieces.swap_remove(found).1);
        }
    }

    Ok(scan.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{EntryId, Index, Kind};
    use crate::ntfs::{VolumeStats, build_index};
    use crate::query::Query;

    // ---- A real volume --------------------------------------------------

    /// The parts of a real NTFS volume the reader touches. See
    /// tests/fixtures/README.md for what is on it.
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/small-ntfs.bin");

    /// A volume of which only some byte ranges are known. Reading anywhere
    /// else is an error, so a reader that wanders off fails the test.
    #[derive(Clone)]
    struct Pieces(Vec<(u64, Vec<u8>)>);

    impl Pieces {
        fn fixture() -> Self {
            let mut rest = FIXTURE.strip_prefix(b"SEEKVO-IMAGE-1\n").unwrap();
            let mut pieces = Vec::new();
            while !rest.is_empty() {
                let offset = u64_at(rest, 0).unwrap();
                let length = u32_at(rest, 8).unwrap() as usize;
                pieces.push((offset, rest[12..12 + length].to_vec()));
                rest = &rest[12 + length..];
            }
            Pieces(pieces)
        }
    }

    impl BlockSource for Pieces {
        fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
            for (start, bytes) in &self.0 {
                let Some(skip) = offset.checked_sub(*start) else {
                    continue;
                };
                let skip = skip as usize;
                if let Some(found) = bytes.get(skip..skip + buffer.len()) {
                    buffer.copy_from_slice(found);
                    return Ok(());
                }
            }
            Err(io::Error::other(
                "read outside the known parts of the volume",
            ))
        }
    }

    fn index_of(source: &mut dyn BlockSource) -> (Index, VolumeStats, FileTable) {
        let table = read_file_table(source).unwrap();
        let (index, stats) = build_index("/", &table.records).unwrap();
        (index, stats, table)
    }

    /// Path and size of every entry, with `/` between parts on any system.
    fn listing(index: &Index) -> Vec<(String, u64)> {
        (0..index.len() as EntryId)
            .map(|id| {
                let path = index.path(id).unwrap().replace('\\', "/");
                (path, index.size(id).unwrap())
            })
            .collect()
    }

    fn size_of(index: &Index, path: &str) -> Option<u64> {
        listing(index)
            .into_iter()
            .find(|(found, _)| found == path)
            .map(|(_, size)| size)
    }

    #[test]
    fn reads_a_real_volume() {
        let (index, stats, table) = index_of(&mut Pieces::fixture());
        assert_eq!(table.damaged, 0);
        // 90 numbered files, a.txt under both its names, and four others.
        assert_eq!(stats.files, 96);
        // The top folder, Docs, Reports, 2026 and Many.
        assert_eq!(stats.dirs, 5);
        // NTFS keeps three bookkeeping files inside its own $Extend folder.
        // The folder is left out, so they have nowhere to go.
        assert_eq!(stats.unreachable, 3);

        assert_eq!(size_of(&index, "/Docs/Reports/2026/report-q3.pdf"), Some(2));
        assert_eq!(size_of(&index, "/Docs/Über 📷.jpg"), Some(1));
        assert_eq!(size_of(&index, "/Docs"), Some(0));
        let report = index.search(&Query::parse("report 2026 ext:pdf"), 10);
        assert_eq!(report.total, 1);
        assert_eq!(index.kind(report.hits[0]), Some(Kind::File));
    }

    #[test]
    fn reads_sizes_stored_inside_and_outside_the_record() {
        let (index, _, _) = index_of(&mut Pieces::fixture());
        // Five bytes fit inside the record itself.
        assert_eq!(size_of(&index, "/Docs/a.txt"), Some(5));
        // These do not, and the second takes no space on the volume at all.
        assert_eq!(size_of(&index, "/big.bin"), Some(300_000));
        assert_eq!(size_of(&index, "/sparse.bin"), Some(5_000_000));
    }

    #[test]
    fn lists_a_hard_linked_file_under_every_name() {
        let (index, _, _) = index_of(&mut Pieces::fixture());
        assert_eq!(size_of(&index, "/Docs/a.txt"), Some(5));
        assert_eq!(size_of(&index, "/link-to-a.txt"), Some(5));
    }

    #[test]
    fn follows_the_table_through_all_its_pieces() {
        let (index, _, _) = index_of(&mut Pieces::fixture());
        // The table on this volume is in three pieces; the last numbered
        // files were created after it had to grow into the later ones.
        for number in 1..=90 {
            let path = format!("/Many/file-{number}.dat");
            let digits = number.to_string().len() as u64;
            assert_eq!(size_of(&index, &path), Some(digits), "{path}");
        }
    }

    #[test]
    fn leaves_out_deleted_files_and_the_volumes_own_files() {
        let (index, _, _) = index_of(&mut Pieces::fixture());
        let names: Vec<String> = listing(&index).into_iter().map(|(path, _)| path).collect();
        assert!(!names.iter().any(|path| path.contains("deleted")));
        assert!(!names.iter().any(|path| path.contains("Pads")));
        assert!(!names.iter().any(|path| path.contains('$')));
    }

    #[test]
    fn never_panics_on_a_damaged_volume() {
        // Flip a few bytes somewhere in the volume, many times over. The
        // reader may give an error or a smaller listing; it must not crash
        // or hang.
        let original = Pieces::fixture();
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..400 {
            let mut damaged = original.clone();
            for _ in 0..(1 + next() % 8) {
                let piece = (next() % damaged.0.len() as u64) as usize;
                let bytes = &mut damaged.0[piece].1;
                let at = (next() % bytes.len() as u64) as usize;
                bytes[at] = next() as u8;
            }
            if let Ok(table) = read_file_table(&mut damaged) {
                let _ = build_index("/", &table.records);
            }
        }
    }

    // ---- A hand-made volume, for the cases a healthy one does not have --

    const SECTOR: usize = 512;
    const CLUSTER: usize = 4096;
    const RECORD: usize = 1024;
    const TABLE_CLUSTER: usize = 4;
    const ROOT: u64 = 5;

    fn boot_sector() -> Vec<u8> {
        let mut sector = vec![0u8; SECTOR];
        sector[3..11].copy_from_slice(b"NTFS    ");
        sector[0x0B..0x0D].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        sector[0x0D] = (CLUSTER / SECTOR) as u8;
        sector[0x28..0x30].copy_from_slice(&((24 * CLUSTER / SECTOR) as u64).to_le_bytes());
        sector[0x30..0x38].copy_from_slice(&(TABLE_CLUSTER as u64).to_le_bytes());
        sector[0x40] = 0xF6; // -10: records of 2^10 bytes
        sector
    }

    fn padded(mut bytes: Vec<u8>) -> Vec<u8> {
        bytes.resize(bytes.len().next_multiple_of(8), 0);
        let length = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        bytes
    }

    fn name_attribute(parent: u64, name: &str, namespace: u8) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut value = vec![0u8; 0x42];
        value[0..8].copy_from_slice(&((1u64 << 48) | parent).to_le_bytes());
        value[0x40] = units.len() as u8;
        value[0x41] = namespace;
        for unit in units {
            value.extend_from_slice(&unit.to_le_bytes());
        }
        let mut attribute = vec![0u8; 24];
        attribute[0..4].copy_from_slice(&ATTRIBUTE_FILE_NAME.to_le_bytes());
        attribute[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        attribute[0x14..0x16].copy_from_slice(&24u16.to_le_bytes());
        attribute.extend(value);
        padded(attribute)
    }

    fn data_inside(size: u32) -> Vec<u8> {
        let mut attribute = vec![0u8; 24];
        attribute[0..4].copy_from_slice(&ATTRIBUTE_DATA.to_le_bytes());
        attribute[0x10..0x14].copy_from_slice(&size.to_le_bytes());
        attribute[0x14..0x16].copy_from_slice(&24u16.to_le_bytes());
        attribute.resize(24 + size as usize, 0xAA);
        padded(attribute)
    }

    fn data_outside(first_cluster_index: u64, size: u64, runs: &[u8]) -> Vec<u8> {
        let mut attribute = vec![0u8; 0x40];
        attribute[0..4].copy_from_slice(&ATTRIBUTE_DATA.to_le_bytes());
        attribute[8] = 1;
        attribute[0x10..0x18].copy_from_slice(&first_cluster_index.to_le_bytes());
        attribute[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        attribute[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        attribute.extend_from_slice(runs);
        attribute.push(0);
        padded(attribute)
    }

    /// One run: `clusters` long, starting `delta` clusters after the
    /// previous run in the same list.
    fn run(clusters: u8, delta: i16) -> Vec<u8> {
        let mut bytes = vec![0x21, clusters];
        bytes.extend_from_slice(&delta.to_le_bytes());
        bytes
    }

    const IN_USE: u16 = RECORD_IN_USE;
    const FOLDER: u16 = RECORD_IN_USE | RECORD_IS_DIRECTORY;

    /// A record with its per-sector protection applied, as it sits on disk.
    fn record(flags: u16, main_record: Option<u64>, attributes: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = vec![0u8; RECORD];
        bytes[0..4].copy_from_slice(b"FILE");
        bytes[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        bytes[6..8].copy_from_slice(&3u16.to_le_bytes());
        bytes[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        bytes[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        if let Some(main) = main_record {
            bytes[0x20..0x28].copy_from_slice(&((1u64 << 48) | main).to_le_bytes());
        }
        let mut at = 0x38;
        for attribute in attributes {
            bytes[at..at + attribute.len()].copy_from_slice(attribute);
            at += attribute.len();
        }
        bytes[at..at + 4].copy_from_slice(&ATTRIBUTE_END.to_le_bytes());
        bytes[0x18..0x1C].copy_from_slice(&((at + 8) as u32).to_le_bytes());

        let counter = [0x07, 0x00];
        bytes[0x30..0x32].copy_from_slice(&counter);
        for sector in 1..=2 {
            let end = sector * SECTOR;
            let saved = [bytes[end - 2], bytes[end - 1]];
            bytes[0x30 + 2 * sector..0x32 + 2 * sector].copy_from_slice(&saved);
            bytes[end - 2..end].copy_from_slice(&counter);
        }
        bytes
    }

    /// A whole volume held in memory.
    struct Memory(Vec<u8>);

    impl BlockSource for Memory {
        fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
            let start = offset as usize;
            let found = self
                .0
                .get(start..start + buffer.len())
                .ok_or_else(|| io::Error::other("read past the end of the volume"))?;
            buffer.copy_from_slice(found);
            Ok(())
        }
    }

    /// A volume whose table is in two pieces: records 0-15 at cluster 4 and
    /// records 16-23 at cluster 20. The table's own record only knows the
    /// first piece; the second is listed in an overflow record, as on a
    /// badly fragmented drive.
    fn hand_made() -> Vec<u8> {
        let mut volume = vec![0u8; 24 * CLUSTER];
        volume[..SECTOR].copy_from_slice(&boot_sector());
        let mut put = |number: usize, bytes: Vec<u8>| {
            let at = if number < 16 {
                TABLE_CLUSTER * CLUSTER + number * RECORD
            } else {
                20 * CLUSTER + (number - 16) * RECORD
            };
            volume[at..at + RECORD].copy_from_slice(&bytes);
        };

        let table_bytes = 24 * RECORD as u64;
        put(
            0,
            record(
                IN_USE,
                None,
                &[
                    name_attribute(ROOT, "$MFT", 3),
                    data_outside(0, table_bytes, &run(4, TABLE_CLUSTER as i16)),
                ],
            ),
        );
        put(5, record(FOLDER, None, &[name_attribute(ROOT, ".", 3)]));
        // The table's second piece starts at its cluster index 4.
        put(
            15,
            record(IN_USE, Some(0), &[data_outside(4, 0, &run(2, 20))]),
        );

        put(16, record(FOLDER, None, &[name_attribute(ROOT, "Docs", 1)]));
        put(
            17,
            record(
                IN_USE,
                None,
                &[name_attribute(16, "a.txt", 1), data_inside(5)],
            ),
        );
        // A file whose data attribute did not fit and lives in record 19.
        put(
            18,
            record(IN_USE, None, &[name_attribute(ROOT, "split.bin", 1)]),
        );
        put(
            19,
            record(IN_USE, Some(18), &[data_outside(0, 777, &run(1, 9))]),
        );
        put(
            20,
            record(
                IN_USE,
                None,
                &[
                    name_attribute(16, "LONGFI~1.TXT", NAMESPACE_DOS),
                    name_attribute(16, "Long File Name.txt", 1),
                    data_inside(9),
                ],
            ),
        );
        let mut torn = record(IN_USE, None, &[name_attribute(ROOT, "torn.txt", 1)]);
        torn[SECTOR - 1] ^= 0xFF;
        put(21, torn);
        put(22, record(0, None, &[name_attribute(ROOT, "gone.txt", 1)]));
        put(
            23,
            record(
                IN_USE,
                None,
                &[
                    name_attribute(ROOT, "big.bin", 1),
                    // Two runs, the second one earlier on the volume.
                    data_outside(0, 1_000_000, &[run(100, 300), run(145, -200)].concat()),
                ],
            ),
        );
        volume
    }

    #[test]
    fn reads_the_hand_made_volume() {
        let (index, stats, table) = index_of(&mut Memory(hand_made()));
        assert_eq!(
            listing(&index),
            [
                ("/".to_owned(), 0),
                ("/Docs".to_owned(), 0),
                ("/big.bin".to_owned(), 1_000_000),
                ("/split.bin".to_owned(), 777),
                ("/Docs/Long File Name.txt".to_owned(), 9),
                ("/Docs/a.txt".to_owned(), 5),
            ]
        );
        assert_eq!(table.damaged, 1);
        // $MFT, the top folder, and the five listed above minus the top
        // folder. Overflow records are not counted.
        assert_eq!(table.in_use, 7);
        assert_eq!(stats.files, 4);
        assert_eq!(stats.dirs, 2);
    }

    #[test]
    fn stops_at_the_first_piece_when_no_more_is_listed() {
        let mut volume = hand_made();
        // Wipe the overflow record that lists the second piece.
        let at = TABLE_CLUSTER * CLUSTER + 15 * RECORD;
        volume[at..at + RECORD].fill(0);
        let (index, _, _) = index_of(&mut Memory(volume));
        assert_eq!(listing(&index), [("/".to_owned(), 0)]);
    }

    #[test]
    fn refuses_a_volume_that_is_not_ntfs() {
        let mut volume = hand_made();
        volume[3..11].copy_from_slice(b"EXFAT   ");
        assert!(matches!(
            read_file_table(&mut Memory(volume)),
            Err(MftError::NotNtfs)
        ));
        assert!(matches!(
            read_file_table(&mut Memory(vec![0u8; 8192])),
            Err(MftError::NotNtfs)
        ));
    }

    #[test]
    fn refuses_a_first_sector_with_impossible_numbers() {
        for (at, value) in [
            (0x0B, 0x33u8),
            (0x0D, 0),
            (0x0D, 3),
            (0x40, 0x80),
            (0x40, 0xFF),
        ] {
            let mut sector = boot_sector();
            sector[at] = value;
            assert!(
                matches!(parse_boot_sector(&sector), Err(MftError::BadBootSector(_))),
                "byte {at:#x} = {value:#x}"
            );
        }
        // A table placed past the end of the volume, or beyond all reason.
        for cluster in [24u64, u64::MAX] {
            let mut far = boot_sector();
            far[0x30..0x38].copy_from_slice(&cluster.to_le_bytes());
            assert!(matches!(
                parse_boot_sector(&far),
                Err(MftError::BadBootSector(_))
            ));
        }
    }

    #[test]
    fn reads_the_sizes_from_the_first_sector() {
        assert_eq!(
            parse_boot_sector(&boot_sector()).unwrap(),
            Geometry {
                bytes_per_sector: 512,
                cluster_size: 4096,
                record_size: 1024,
                mft_offset: 16_384,
                volume_bytes: 98_304,
            }
        );
        // Large clusters are written as a negative power of two, and
        // record sizes can be a count of clusters.
        let mut sector = boot_sector();
        sector[0x0D] = 0xF8; // 2^8 sectors
        sector[0x28..0x30].copy_from_slice(&1_000_000u64.to_le_bytes());
        assert_eq!(parse_boot_sector(&sector).unwrap().cluster_size, 131_072);
        let mut sector = boot_sector();
        sector[0x0D] = 1;
        sector[0x40] = 2; // two 512-byte clusters
        assert_eq!(parse_boot_sector(&sector).unwrap().record_size, 1024);
    }

    #[test]
    fn refuses_a_table_whose_own_record_is_unreadable() {
        let at = TABLE_CLUSTER * CLUSTER;

        let mut blank = hand_made();
        blank[at..at + RECORD].fill(0);
        assert!(matches!(
            read_file_table(&mut Memory(blank)),
            Err(MftError::BadTable(_))
        ));

        let mut torn = hand_made();
        torn[at + SECTOR - 1] ^= 0xFF;
        assert!(matches!(
            read_file_table(&mut Memory(torn)),
            Err(MftError::BadTable(_))
        ));

        // A table that claims to have no data attribute.
        let mut lost = hand_made();
        let no_data = record(IN_USE, None, &[name_attribute(ROOT, "$MFT", 3)]);
        lost[at..at + RECORD].copy_from_slice(&no_data);
        assert!(matches!(
            read_file_table(&mut Memory(lost)),
            Err(MftError::BadTable(_))
        ));
    }

    #[test]
    fn refuses_a_table_that_claims_to_be_bigger_than_the_volume() {
        let mut volume = hand_made();
        let at = TABLE_CLUSTER * CLUSTER;
        let huge = record(
            IN_USE,
            None,
            &[
                name_attribute(ROOT, "$MFT", 3),
                data_outside(0, u64::MAX / 2, &run(4, TABLE_CLUSTER as i16)),
            ],
        );
        volume[at..at + RECORD].copy_from_slice(&huge);
        assert!(matches!(
            read_file_table(&mut Memory(volume)),
            Err(MftError::BadTable(_))
        ));
    }

    #[test]
    fn ignores_an_overflow_record_that_points_outside_the_table() {
        let mut volume = hand_made();
        // Record 19 now claims to belong to record four million.
        let stray = record(IN_USE, Some(4_000_000), &[data_outside(0, 777, &run(1, 9))]);
        let at = 20 * CLUSTER + 3 * RECORD;
        volume[at..at + RECORD].copy_from_slice(&stray);
        let (index, _, _) = index_of(&mut Memory(volume));
        assert_eq!(size_of(&index, "/split.bin"), Some(0));
        assert_eq!(size_of(&index, "/big.bin"), Some(1_000_000));
    }

    #[test]
    fn decodes_run_lists() {
        // 0x30 clusters at cluster 0x20, then 0x10 clusters 0x100 further on.
        assert_eq!(
            decode_runs(&[0x11, 0x30, 0x20, 0x21, 0x10, 0x00, 0x01, 0x00]),
            Some(vec![
                Run {
                    cluster: Some(0x20),
                    clusters: 0x30
                },
                Run {
                    cluster: Some(0x120),
                    clusters: 0x10
                },
            ])
        );
        // A negative distance steps back toward the start of the volume.
        assert_eq!(
            decode_runs(&[0x11, 0x05, 0x40, 0x11, 0x05, 0xF0, 0x00]),
            Some(vec![
                Run {
                    cluster: Some(0x40),
                    clusters: 5
                },
                Run {
                    cluster: Some(0x30),
                    clusters: 5
                },
            ])
        );
        // No distance at all marks a hole.
        assert_eq!(
            decode_runs(&[0x01, 0x08, 0x11, 0x02, 0x10, 0x00]),
            Some(vec![
                Run {
                    cluster: None,
                    clusters: 8
                },
                Run {
                    cluster: Some(0x10),
                    clusters: 2
                },
            ])
        );
        assert_eq!(decode_runs(&[0x00]), Some(vec![]));
    }

    #[test]
    fn rejects_malformed_run_lists() {
        // No end marker.
        assert_eq!(decode_runs(&[0x11, 0x30, 0x20]), None);
        assert_eq!(decode_runs(&[]), None);
        // Cut off in the middle of a run.
        assert_eq!(decode_runs(&[0x21, 0x30, 0x20]), None);
        // A length of zero clusters, or no length field.
        assert_eq!(decode_runs(&[0x11, 0x00, 0x20, 0x00]), None);
        assert_eq!(decode_runs(&[0x10, 0x20, 0x00]), None);
        // Fields wider than eight bytes.
        assert_eq!(decode_runs(&[0x19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), None);
        // A run that would start before the volume does.
        assert_eq!(decode_runs(&[0x11, 0x05, 0xF0, 0x00]), None);
    }

    #[test]
    fn checks_and_undoes_the_per_sector_protection() {
        let on_disk = record(IN_USE, None, &[name_attribute(ROOT, "a.txt", 1)]);
        let mut good = on_disk.clone();
        assert!(apply_fixups(&mut good));
        // The counter is gone from the sector ends again.
        assert_ne!(good[SECTOR - 2..SECTOR], on_disk[SECTOR - 2..SECTOR]);

        let mut torn = on_disk.clone();
        torn[2 * SECTOR - 2] ^= 1;
        assert!(!apply_fixups(&mut torn));

        // Headers that point outside the record.
        let mut wild = on_disk.clone();
        wild[4..6].copy_from_slice(&2000u16.to_le_bytes());
        assert!(!apply_fixups(&mut wild));
        let mut many = on_disk.clone();
        many[6..8].copy_from_slice(&200u16.to_le_bytes());
        assert!(!apply_fixups(&mut many));
        let mut none = on_disk;
        none[6..8].copy_from_slice(&1u16.to_le_bytes());
        assert!(!apply_fixups(&mut none));
    }
}
