//! Saving a drive's records to a file, so the next start needs no drive
//! read.
//!
//! The file holds every record's name, folder, kind and size, plus the
//! place in the drive's change journal that was current when it was
//! written. On the next start the records are loaded and the journal is
//! read from that place, which brings them up to date with whatever
//! happened in between.
//!
//! The file is only ever read by the engine that wrote it, on the same
//! machine, but it is still checked like any other input: a file cut short
//! or damaged is refused, never trusted and never a crash.

use std::fmt;
use std::io::{self, Read, Write};

use crate::ntfs::VolumeRecords;

/// The first bytes of every saved file. The number changes whenever the
/// layout does, so an old file is refused instead of misread.
const MAGIC: &[u8; 16] = b"SEEKVO-RECORDS-1";
const HEADER_BYTES: usize = 16 + 8 + 8 + 8 + 8;
/// Record number, folder's record number, size, name length.
const ROW_BYTES: usize = 8 + 8 + 8 + 2;
const IS_FOLDER: u64 = 1 << 63;

/// Which drive and which moment a saved file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Origin {
    /// The drive's serial number, to tell drives apart.
    pub volume_serial: u32,
    /// Which change journal the position below is in. A drive gets a new
    /// one when its journal is deleted and made again.
    pub journal_id: u64,
    /// Where in that journal to continue from.
    pub cursor: i64,
}

/// What a saved file holds.
#[derive(Debug)]
pub struct Saved {
    pub origin: Origin,
    pub records: VolumeRecords,
}

/// Why a saved file could not be used.
#[derive(Debug)]
pub enum SnapshotError {
    Io(io::Error),
    /// Not a file this version of the engine wrote.
    NotOurs,
    /// Cut short or changed since it was written.
    Damaged,
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Io(error) => write!(f, "cannot read the saved index: {error}"),
            SnapshotError::NotOurs => write!(f, "the saved index is from another version"),
            SnapshotError::Damaged => write!(f, "the saved index is damaged"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<io::Error> for SnapshotError {
    fn from(error: io::Error) -> Self {
        SnapshotError::Io(error)
    }
}

/// A quick check value over the file's bytes. It is there to notice a file
/// that was cut short or damaged, not to resist tampering.
fn check_value(bytes: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    let mut value = 0xCBF2_9CE4_8422_2325u64 ^ bytes.len() as u64;
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        let word = u64::from_le_bytes(word.try_into().expect("chunks of 8"));
        value = (value ^ word).wrapping_mul(PRIME).rotate_left(29);
    }
    for &byte in words.remainder() {
        value = (value ^ u64::from(byte)).wrapping_mul(PRIME);
    }
    value
}

/// Write `records` and where they came from to `out`.
pub fn write(out: &mut impl Write, records: &VolumeRecords, origin: Origin) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES + records.len() * (ROW_BYTES + 24) + 8);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&u64::from(origin.volume_serial).to_le_bytes());
    bytes.extend_from_slice(&origin.journal_id.to_le_bytes());
    bytes.extend_from_slice(&origin.cursor.to_le_bytes());
    bytes.extend_from_slice(&(records.len() as u64).to_le_bytes());
    for row in records.iter() {
        let kind = if row.is_dir { IS_FOLDER } else { 0 };
        bytes.extend_from_slice(&(row.record | kind).to_le_bytes());
        bytes.extend_from_slice(&row.parent.to_le_bytes());
        bytes.extend_from_slice(&row.size.to_le_bytes());
        // An NTFS name is at most 255 UTF-16 units, which is at most 765
        // bytes of UTF-8.
        bytes.extend_from_slice(&(row.name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(row.name.as_bytes());
    }
    let check = check_value(&bytes);
    bytes.extend_from_slice(&check.to_le_bytes());
    out.write_all(&bytes)
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    let slice = bytes.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes(slice.try_into().ok()?))
}

/// Read back what [`write`] wrote.
pub fn read(input: &mut impl Read) -> Result<Saved, SnapshotError> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    if bytes.get(..MAGIC.len()) != Some(MAGIC) {
        return Err(SnapshotError::NotOurs);
    }
    parse(&bytes).ok_or(SnapshotError::Damaged)
}

fn parse(bytes: &[u8]) -> Option<Saved> {
    let body_length = bytes.len().checked_sub(8)?;
    if body_length < HEADER_BYTES
        || u64_at(bytes, body_length)? != check_value(&bytes[..body_length])
    {
        return None;
    }
    let body = &bytes[..body_length];

    let origin = Origin {
        volume_serial: u32::try_from(u64_at(body, 16)?).ok()?,
        journal_id: u64_at(body, 24)?,
        cursor: u64_at(body, 32)? as i64,
    };
    let rows = usize::try_from(u64_at(body, 40)?).ok()?;
    // Every row takes at least ROW_BYTES, so a count that could not fit in
    // the file is refused before anything is set aside for it.
    if rows > (body.len() - HEADER_BYTES) / ROW_BYTES {
        return None;
    }

    let mut records = VolumeRecords::with_room(rows, body.len() - HEADER_BYTES - rows * ROW_BYTES);
    let mut at = HEADER_BYTES;
    for _ in 0..rows {
        let record = u64_at(body, at)?;
        let parent = u64_at(body, at + 8)?;
        let size = u64_at(body, at + 16)?;
        let name_length = usize::from(u16::from_le_bytes(
            body.get(at + 24..at + 26)?.try_into().ok()?,
        ));
        at += ROW_BYTES;
        let name = std::str::from_utf8(body.get(at..at + name_length)?).ok()?;
        at += name_length;
        records.push_sized(
            record & !IS_FOLDER,
            parent,
            name,
            record & IS_FOLDER != 0,
            size,
        );
    }
    (at == body.len()).then_some(Saved { origin, records })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntfs::build_index;

    const ORIGIN: Origin = Origin {
        volume_serial: 0xDEAD_BEEF,
        journal_id: 0x01DC_1234_5678_9ABC,
        cursor: 987_654_321,
    };

    fn sample() -> VolumeRecords {
        let mut records = VolumeRecords::new();
        records.push_sized(30, 5, "Docs", true, 0);
        records.push_sized(31, 30, "résumé – final.pdf", false, 123_456);
        records.push_sized(32, 5, "", false, 0);
        records.push_sized(33, 30, "big.bin", false, u64::MAX / 3);
        records.push_sized(31, 5, "second name.pdf", false, 123_456);
        records
    }

    fn rows(records: &VolumeRecords) -> Vec<(u64, u64, String, bool, u64)> {
        records
            .iter()
            .map(|row| {
                (
                    row.record,
                    row.parent,
                    row.name.to_owned(),
                    row.is_dir,
                    row.size,
                )
            })
            .collect()
    }

    fn saved(records: &VolumeRecords) -> Vec<u8> {
        let mut bytes = Vec::new();
        write(&mut bytes, records, ORIGIN).unwrap();
        bytes
    }

    #[test]
    fn reads_back_what_it_wrote() {
        let records = sample();
        let back = read(&mut saved(&records).as_slice()).unwrap();
        assert_eq!(back.origin, ORIGIN);
        assert_eq!(rows(&back.records), rows(&records));
    }

    #[test]
    fn saves_an_empty_drive() {
        let back = read(&mut saved(&VolumeRecords::new()).as_slice()).unwrap();
        assert!(back.records.is_empty());
    }

    #[test]
    fn leaves_out_records_that_were_replaced() {
        let mut records = sample();
        records.seal();
        records.remove_record(31);
        records.push_sized(31, 30, "renamed.pdf", false, 5);
        records.remove_record(33);

        let back = read(&mut saved(&records).as_slice()).unwrap();
        assert_eq!(back.records.len(), 3);
        let (before, _) = build_index("/", &records).unwrap();
        let (after, _) = build_index("/", &back.records).unwrap();
        let paths = |index: &crate::Index| -> Vec<String> {
            (0..index.len() as u32)
                .map(|id| index.path(id).unwrap())
                .collect()
        };
        assert_eq!(paths(&after), paths(&before));
    }

    #[test]
    fn refuses_a_file_that_is_not_a_saved_index() {
        assert!(matches!(
            read(&mut b"hello".as_slice()),
            Err(SnapshotError::NotOurs)
        ));
        assert!(matches!(
            read(&mut b"".as_slice()),
            Err(SnapshotError::NotOurs)
        ));
        // The next version's files are not this version's.
        let mut newer = saved(&sample());
        newer[15] = b'2';
        assert!(matches!(
            read(&mut newer.as_slice()),
            Err(SnapshotError::NotOurs)
        ));
    }

    #[test]
    fn refuses_a_file_that_was_cut_short() {
        let bytes = saved(&sample());
        for length in MAGIC.len()..bytes.len() {
            assert!(
                matches!(read(&mut &bytes[..length]), Err(SnapshotError::Damaged)),
                "{length}"
            );
        }
    }

    #[test]
    fn refuses_a_file_with_any_byte_changed() {
        let bytes = saved(&sample());
        for at in 0..bytes.len() {
            let mut changed = bytes.clone();
            changed[at] ^= 0x40;
            assert!(read(&mut changed.as_slice()).is_err(), "{at}");
        }
    }

    #[test]
    fn a_damaged_file_with_a_matching_check_value_is_still_refused_or_read_safely() {
        // Damage the body, then fix the check value up, as if the damage
        // had been there when the file was written. Reading must not panic
        // or set aside absurd amounts of memory, whatever it finds.
        let bytes = saved(&sample());
        let mut state = 0x1234_5678_9ABC_DEF1u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3000 {
            let mut changed = bytes.clone();
            let body = changed.len() - 8;
            for _ in 0..(1 + next() % 4) {
                let at = MAGIC.len() + (next() % (body - MAGIC.len()) as u64) as usize;
                changed[at] = next() as u8;
            }
            let check = check_value(&changed[..body]);
            changed[body..].copy_from_slice(&check.to_le_bytes());
            if let Ok(back) = read(&mut changed.as_slice()) {
                let _ = build_index("/", &back.records);
            }
        }
    }
}
