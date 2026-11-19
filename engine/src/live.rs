//! Keeping an index current while files change.
//!
//! NTFS writes a line to its change journal every time a file is created,
//! renamed, moved, written to or deleted. Reading the journal from where
//! the last read stopped gives the record numbers of everything that
//! changed, however long ago the drive was first read.
//!
//! The journal says *that* a record changed, and for some changes what kind
//! it was. This module does not try to replay those kinds. For every record
//! the journal mentions it asks for the record as it is now and replaces
//! what was known about it. A create, a rename, a move, a second name, a
//! new size and a delete all come out of that one step, and a missed or
//! repeated journal line cannot leave the index wrong.
//!
//! Nothing here is Windows-specific: the drive is anything that can hand
//! over one file record ([`RecordSource`]) and read its journal
//! ([`Journal`]).

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::index::{BuildError, Index, IndexBuilder};
use crate::mft::{FileRecord, parse_file_record};
use crate::ntfs::{JournalEntry, VolumeRecords, VolumeStats, build_index};

/// Hands over single file records as the file system has them right now.
pub trait RecordSource {
    /// The record with exactly this number, or `None` if no file is using
    /// that number at the moment.
    fn file_record(&mut self, number: u64) -> io::Result<Option<Vec<u8>>>;
}

/// A stretch of the change journal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JournalBatch {
    /// Where to read from next time.
    pub next: i64,
    /// What happened, oldest first. Empty when there is nothing new.
    pub entries: Vec<JournalEntry>,
}

/// Why the change journal could not be read.
#[derive(Debug)]
pub enum JournalError {
    /// The drive keeps no change journal.
    NotActive,
    /// The journal has been restarted, or the place to continue from has
    /// been overwritten. Changes were missed; the drive has to be read
    /// again from scratch.
    Expired,
    Io(io::Error),
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::NotActive => write!(f, "the drive keeps no change journal"),
            JournalError::Expired => write!(
                f,
                "the change journal no longer goes back far enough; the drive must be read again"
            ),
            JournalError::Io(error) => write!(f, "cannot read the change journal: {error}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// Reads a drive's change journal.
pub trait Journal {
    /// The entries at and after position `from`. Position 0 means the
    /// oldest entry the journal still has.
    fn read_from(&mut self, from: i64) -> Result<JournalBatch, JournalError>;
}

/// Why following a drive stopped.
#[derive(Debug)]
pub enum LiveError {
    Journal(JournalError),
    Io(io::Error),
    Build(BuildError),
}

impl fmt::Display for LiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiveError::Journal(error) => write!(f, "{error}"),
            LiveError::Io(error) => write!(f, "cannot read the drive: {error}"),
            LiveError::Build(error) => write!(f, "cannot build the index: {error}"),
        }
    }
}

impl std::error::Error for LiveError {}

impl From<JournalError> for LiveError {
    fn from(error: JournalError) -> Self {
        LiveError::Journal(error)
    }
}

impl From<io::Error> for LiveError {
    fn from(error: io::Error) -> Self {
        LiveError::Io(error)
    }
}

impl From<BuildError> for LiveError {
    fn from(error: BuildError) -> Self {
        LiveError::Build(error)
    }
}

/// The current time as Windows counts it: 100-nanosecond steps since the
/// start of 1601. Journal entries carry their time in the same form.
pub fn filetime_now() -> i64 {
    /// Seconds from 1601 to 1970.
    const TO_UNIX_EPOCH: i64 = 11_644_473_600;
    let since_1970 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (since_1970.as_secs() as i64 + TO_UNIX_EPOCH) * 10_000_000
        + i64::from(since_1970.subsec_nanos() / 100)
}

/// One file as it is now, pieced together from its record and any overflow
/// records.
enum Lookup {
    /// No file uses this record number any more.
    Gone,
    /// This is an overflow record; the file is this other record.
    PartOf(u64),
    File(FileRecord),
}

fn look_up(number: u64, source: &mut dyn RecordSource) -> io::Result<Lookup> {
    let Some(bytes) = source.file_record(number)? else {
        return Ok(Lookup::Gone);
    };
    let Some(mut file) = parse_file_record(&bytes).filter(|file| file.in_use) else {
        return Ok(Lookup::Gone);
    };
    if let Some(main) = file.belongs_to.filter(|&main| main != number) {
        return Ok(Lookup::PartOf(main));
    }

    // Names, or the start of the data, may sit in overflow records.
    for other in std::mem::take(&mut file.continued_in) {
        if other == number {
            continue;
        }
        let Some(bytes) = source.file_record(other)? else {
            continue;
        };
        let Some(part) = parse_file_record(&bytes) else {
            continue;
        };
        if !part.in_use || part.belongs_to != Some(number) {
            continue;
        }
        file.names.extend(part.names);
        file.size = file.size.or(part.size);
    }
    Ok(Lookup::File(file))
}

/// Look at one record again and replace what `records` knows about it.
///
/// Works the same whether the file is new, renamed, moved, resized, has a
/// name more or less, or is gone.
pub fn refresh(
    records: &mut VolumeRecords,
    number: u64,
    source: &mut dyn RecordSource,
) -> io::Result<()> {
    let (number, file) = match look_up(number, source)? {
        Lookup::Gone => {
            records.remove_record(number);
            return Ok(());
        }
        Lookup::File(file) => (number, file),
        // The journal named an overflow record. Look at the file it is
        // part of instead, once; a second redirect would mean damage.
        Lookup::PartOf(main) => match look_up(main, source)? {
            Lookup::File(file) => (main, file),
            Lookup::Gone | Lookup::PartOf(_) => {
                records.remove_record(main);
                return Ok(());
            }
        },
    };

    // When the size sits somewhere this code does not read (a list of
    // overflow records stored outside the record), the last known size is
    // better than zero.
    let size = if file.is_dir {
        0
    } else {
        file.size.or_else(|| records.size_of(number)).unwrap_or(0)
    };
    records.remove_record(number);
    for (parent, name) in &file.names {
        records.push_sized(number, *parent, name, file.is_dir, size);
    }
    Ok(())
}

/// An index of a drive that can be brought up to date at any time.
pub struct Live<V> {
    volume: V,
    records: VolumeRecords,
    root_name: String,
    /// Where in the journal to continue from.
    cursor: i64,
    index: Index,
    stats: VolumeStats,
    /// Records changed since `index` was built.
    dirty: bool,
    /// Tidy the records up once this many rows have been replaced.
    tidy_after: usize,
    /// How many records were looked at again while catching up at the
    /// start, and how long that took.
    caught_up: usize,
    caught_up_in: Duration,
}

impl<V: RecordSource + Journal> Live<V> {
    /// Start following a drive whose records have just been read in full.
    ///
    /// `root_name` is how the top folder should be shown, such as `C:\`.
    ///
    /// A full read of the table off the disk can miss the last few seconds
    /// of changes, because Windows had not written them out yet. So the
    /// journal is read from its start, and every record with an entry at or
    /// after `replay_since` (a time as [`filetime_now`] gives it) is looked
    /// at again. Pass a time shortly before the full read began.
    pub fn start(
        volume: V,
        records: VolumeRecords,
        root_name: &str,
        replay_since: i64,
    ) -> Result<Self, LiveError> {
        Self::begin(volume, records, root_name, 0, Some(replay_since))
    }

    /// Carry on following a drive from records saved earlier.
    ///
    /// `cursor` is the journal position that was current when the records
    /// were saved, as [`cursor`](Self::cursor) gave it. Everything the
    /// journal has noted since is taken in before this returns. Fails with
    /// [`JournalError::Expired`] when the journal no longer goes back that
    /// far; the drive then has to be read in full again.
    pub fn resume(
        volume: V,
        records: VolumeRecords,
        root_name: &str,
        cursor: i64,
    ) -> Result<Self, LiveError> {
        Self::begin(volume, records, root_name, cursor, None)
    }

    fn begin(
        volume: V,
        mut records: VolumeRecords,
        root_name: &str,
        cursor: i64,
        only_since: Option<i64>,
    ) -> Result<Self, LiveError> {
        records.seal();
        let mut live = Live {
            volume,
            records,
            root_name: root_name.to_owned(),
            cursor,
            // Built below, once, after catching up.
            index: IndexBuilder::new().finish(),
            stats: VolumeStats::default(),
            dirty: true,
            tidy_after: 50_000,
            caught_up: 0,
            caught_up_in: Duration::ZERO,
        };
        let started = Instant::now();
        live.caught_up = live.apply_journal(only_since)?;
        live.caught_up_in = started.elapsed();
        live.index()?;
        Ok(live)
    }

    /// Read what the journal has added since last time and bring the
    /// records up to date. Returns how many files were looked at again.
    ///
    /// The index itself is rebuilt by the next call to [`index`](Self::index),
    /// so several polls in a row cost one rebuild.
    pub fn poll(&mut self) -> Result<usize, LiveError> {
        self.apply_journal(None)
    }

    fn apply_journal(&mut self, only_since: Option<i64>) -> Result<usize, LiveError> {
        let mut touched: BTreeSet<u64> = BTreeSet::new();
        loop {
            let batch = self.volume.read_from(self.cursor)?;
            let moved_on = batch.next > self.cursor;
            self.cursor = self.cursor.max(batch.next);
            if batch.entries.is_empty() {
                break;
            }
            for entry in &batch.entries {
                if only_since.is_none_or(|since| entry.time >= since) {
                    touched.insert(entry.record);
                    // A file that moved or was deleted also changed its
                    // folder; the folder's own record does not need
                    // another look, but a folder the index has never seen
                    // does, and looking twice is harmless.
                    touched.insert(entry.parent);
                }
            }
            if !moved_on {
                // A journal that hands out entries without moving forward
                // would loop forever.
                break;
            }
        }

        for &number in &touched {
            refresh(&mut self.records, number, &mut self.volume)?;
        }
        if !touched.is_empty() {
            self.dirty = true;
        }
        if self.records.stale_rows() >= self.tidy_after {
            self.records.seal();
        }
        Ok(touched.len())
    }

    /// The index, rebuilt first if anything changed since it was built.
    pub fn index(&mut self) -> Result<&Index, LiveError> {
        if self.dirty {
            let (index, stats) = build_index(&self.root_name, &self.records)?;
            self.index = index;
            self.stats = stats;
            self.dirty = false;
        }
        Ok(&self.index)
    }

    /// True when a poll changed something and the index has not been
    /// rebuilt since.
    pub fn needs_rebuild(&self) -> bool {
        self.dirty
    }

    /// Counts from the last time the index was built.
    pub fn stats(&self) -> VolumeStats {
        self.stats
    }

    /// How many records were looked at again at the start, to catch up
    /// with changes the records it was given did not have yet.
    pub fn caught_up(&self) -> usize {
        self.caught_up
    }

    /// How long catching up at the start took.
    pub fn caught_up_in(&self) -> Duration {
        self.caught_up_in
    }

    /// Where in the journal the next poll continues from. Save it together
    /// with the records to [`resume`](Self::resume) later.
    pub fn cursor(&self) -> i64 {
        self.cursor
    }

    /// The records as they are now.
    pub fn records(&self) -> &VolumeRecords {
        &self.records
    }

    /// The drive being followed.
    pub fn volume(&self) -> &V {
        &self.volume
    }

    #[cfg(test)]
    fn tidy_after(&mut self, rows: usize) {
        self.tidy_after = rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::EntryId;
    use crate::mft::testing::*;
    use std::collections::HashMap;

    /// A drive made of a few file records and a journal, all in memory.
    #[derive(Default)]
    struct FakeDrive {
        records: HashMap<u64, Vec<u8>>,
        journal: Vec<JournalEntry>,
        /// Set to make the next journal read fail.
        expired: bool,
        /// How many single records have been asked for.
        lookups: usize,
    }

    impl FakeDrive {
        /// Put a record on the drive and note it in the journal.
        fn write(&mut self, number: u64, bytes: Vec<u8>, time: i64) {
            self.records.insert(number, bytes);
            self.note(number, time);
        }

        fn delete(&mut self, number: u64, time: i64) {
            self.records.remove(&number);
            self.note(number, time);
        }

        fn note(&mut self, number: u64, time: i64) {
            let usn = (self.journal.len() as i64 + 1) * 100;
            self.journal.push(JournalEntry {
                record: number,
                parent: ROOT,
                usn,
                time,
                reason: 0,
            });
        }
    }

    impl RecordSource for &mut FakeDrive {
        fn file_record(&mut self, number: u64) -> io::Result<Option<Vec<u8>>> {
            self.lookups += 1;
            Ok(self.records.get(&number).cloned())
        }
    }

    impl Journal for &mut FakeDrive {
        fn read_from(&mut self, from: i64) -> Result<JournalBatch, JournalError> {
            if self.expired {
                return Err(JournalError::Expired);
            }
            // Two entries at a time, so reading has to go round more than
            // once.
            let entries: Vec<JournalEntry> = self
                .journal
                .iter()
                .filter(|entry| entry.usn >= from)
                .take(2)
                .copied()
                .collect();
            let next = entries.last().map_or(from, |last| last.usn + 100);
            Ok(JournalBatch { next, entries })
        }
    }

    const DOCS: u64 = 30;
    const LONG_AGO: i64 = 1_000;
    const NOW: i64 = 5_000;

    fn file(parent: u64, name: &str, size: u32) -> Vec<u8> {
        record(
            IN_USE,
            None,
            &[name_attribute(parent, name, 1), data_inside(size)],
        )
    }

    fn folder(parent: u64, name: &str) -> Vec<u8> {
        record(FOLDER, None, &[name_attribute(parent, name, 1)])
    }

    /// A drive with `Docs\a.txt` (5 bytes) and `notes.txt` (9 bytes), and
    /// the records as a full read would have found them.
    fn drive() -> (FakeDrive, VolumeRecords) {
        let mut drive = FakeDrive::default();
        drive.records.insert(DOCS, folder(ROOT, "Docs"));
        drive.records.insert(31, file(DOCS, "a.txt", 5));
        drive.records.insert(32, file(ROOT, "notes.txt", 9));

        let mut records = VolumeRecords::new();
        records.push_sized(DOCS, ROOT, "Docs", true, 0);
        records.push_sized(31, DOCS, "a.txt", false, 5);
        records.push_sized(32, ROOT, "notes.txt", false, 9);
        (drive, records)
    }

    fn follow(drive: &mut FakeDrive, records: VolumeRecords) -> Live<&mut FakeDrive> {
        Live::start(drive, records, "/", NOW).unwrap()
    }

    /// Every path with its size, sorted, with `/` between parts.
    fn listing<V: RecordSource + Journal>(live: &mut Live<V>) -> Vec<(String, u64)> {
        let index = live.index().unwrap();
        let mut found: Vec<(String, u64)> = (1..index.len() as EntryId)
            .map(|id| {
                let path = index.path(id).unwrap().replace('\\', "/");
                (path, index.size(id).unwrap())
            })
            .collect();
        found.sort();
        found
    }

    fn entry(path: &str, size: u64) -> (String, u64) {
        (path.to_owned(), size)
    }

    #[test]
    fn starts_from_the_records_it_was_given() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        assert_eq!(
            listing(&mut live),
            [
                entry("/Docs", 0),
                entry("/Docs/a.txt", 5),
                entry("/notes.txt", 9)
            ]
        );
        assert!(!live.needs_rebuild());
        assert_eq!(live.poll().unwrap(), 0);
        assert!(!live.needs_rebuild());
    }

    #[test]
    fn picks_up_a_new_file() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        live.volume.write(40, file(DOCS, "new.txt", 7), NOW);
        assert!(live.poll().unwrap() >= 1);
        assert!(live.needs_rebuild());
        assert!(listing(&mut live).contains(&entry("/Docs/new.txt", 7)));
        assert_eq!(live.stats().files, 3);
    }

    #[test]
    fn picks_up_a_new_size() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        let grown = record(
            IN_USE,
            None,
            &[
                name_attribute(DOCS, "a.txt", 1),
                data_outside(0, 2_000_000, &run(100, 300)),
            ],
        );
        live.volume.write(31, grown, NOW);
        live.poll().unwrap();
        assert!(listing(&mut live).contains(&entry("/Docs/a.txt", 2_000_000)));
    }

    #[test]
    fn follows_a_rename_and_a_move() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        // notes.txt becomes Docs\ideas.txt.
        live.volume.write(32, file(DOCS, "ideas.txt", 9), NOW);
        live.poll().unwrap();
        assert_eq!(
            listing(&mut live),
            [
                entry("/Docs", 0),
                entry("/Docs/a.txt", 5),
                entry("/Docs/ideas.txt", 9)
            ]
        );
    }

    #[test]
    fn renaming_a_folder_moves_everything_inside_it() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        live.volume.write(DOCS, folder(ROOT, "Papers"), NOW);
        let lookups_before = live.volume.lookups;
        live.poll().unwrap();
        // Only the folder itself (and the folder it sits in) was looked at
        // again; the file inside follows through its parent number.
        assert!(live.volume.lookups - lookups_before <= 2);
        assert_eq!(
            listing(&mut live),
            [
                entry("/Papers", 0),
                entry("/Papers/a.txt", 5),
                entry("/notes.txt", 9)
            ]
        );
    }

    #[test]
    fn drops_a_deleted_file() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        live.volume.delete(31, NOW);
        live.poll().unwrap();
        assert_eq!(
            listing(&mut live),
            [entry("/Docs", 0), entry("/notes.txt", 9)]
        );
    }

    #[test]
    fn a_reused_record_number_shows_the_new_file_only() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        // a.txt is deleted and its record number goes to a new folder.
        live.volume.write(31, folder(ROOT, "Music"), NOW);
        live.poll().unwrap();
        assert_eq!(
            listing(&mut live),
            [
                entry("/Docs", 0),
                entry("/Music", 0),
                entry("/notes.txt", 9)
            ]
        );
    }

    #[test]
    fn follows_names_being_added_and_taken_away() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        let two_names = record(
            IN_USE,
            None,
            &[
                name_attribute(DOCS, "a.txt", 1),
                name_attribute(ROOT, "shortcut-to-a.txt", 1),
                data_inside(5),
            ],
        );
        live.volume.write(31, two_names, NOW);
        live.poll().unwrap();
        assert!(listing(&mut live).contains(&entry("/shortcut-to-a.txt", 5)));
        assert!(listing(&mut live).contains(&entry("/Docs/a.txt", 5)));

        live.volume
            .write(31, file(ROOT, "shortcut-to-a.txt", 5), NOW);
        live.poll().unwrap();
        assert_eq!(
            listing(&mut live),
            [
                entry("/Docs", 0),
                entry("/notes.txt", 9),
                entry("/shortcut-to-a.txt", 5)
            ]
        );
    }

    #[test]
    fn reads_the_size_from_an_overflow_record() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        // The file's data attribute did not fit and lives in record 50.
        let main = record(
            IN_USE,
            None,
            &[
                attribute_list(&[(0x30, 0, 0, 33), (0x80, 0, 0, 50)]),
                name_attribute(ROOT, "huge.bin", 1),
            ],
        );
        let overflow = record(IN_USE, Some(33), &[data_outside(0, 9_000_000, &run(1, 9))]);
        live.volume.records.insert(50, overflow);
        live.volume.write(33, main, NOW);
        live.poll().unwrap();
        assert!(listing(&mut live).contains(&entry("/huge.bin", 9_000_000)));
    }

    #[test]
    fn a_change_noted_against_an_overflow_record_updates_the_file() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        let main = record(
            IN_USE,
            None,
            &[
                attribute_list(&[(0x80, 0, 0, 50)]),
                name_attribute(ROOT, "huge.bin", 1),
            ],
        );
        live.volume.records.insert(33, main);
        let overflow = record(IN_USE, Some(33), &[data_outside(0, 4_444, &run(1, 9))]);
        // Only the overflow record is named in the journal.
        live.volume.write(50, overflow, NOW);
        live.poll().unwrap();
        assert!(listing(&mut live).contains(&entry("/huge.bin", 4_444)));
    }

    #[test]
    fn keeps_the_last_known_size_when_it_cannot_be_read() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        // notes.txt (9 bytes) now keeps its list of overflow records
        // outside the record, where this code does not follow it.
        let main = record(
            IN_USE,
            None,
            &[
                attribute_list_elsewhere(),
                name_attribute(ROOT, "notes.txt", 1),
            ],
        );
        live.volume.write(32, main, NOW);
        live.poll().unwrap();
        assert!(listing(&mut live).contains(&entry("/notes.txt", 9)));
    }

    #[test]
    fn catches_up_on_changes_the_full_read_was_too_early_for() {
        let (mut drive, records) = drive();
        // Written just before the full read began, so the read missed it.
        drive.write(40, file(ROOT, "just-saved.txt", 3), NOW);
        // Changed long before; the full read already has the new state, and
        // the drive here pretends otherwise to prove it is left alone.
        drive.write(32, file(ROOT, "renamed-long-ago.txt", 9), LONG_AGO);

        let mut live = follow(&mut drive, records);
        let found = listing(&mut live);
        assert!(found.contains(&entry("/just-saved.txt", 3)));
        assert!(found.contains(&entry("/notes.txt", 9)));

        // Having caught up, the next poll has nothing left to do.
        assert_eq!(live.poll().unwrap(), 0);
    }

    #[test]
    fn reads_the_journal_once_only() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        for number in 40..45 {
            live.volume
                .write(number, file(ROOT, &format!("f{number}.txt"), 1), NOW);
        }
        assert_eq!(live.stats().files, 2);
        assert!(live.poll().unwrap() >= 5);
        assert_eq!(live.poll().unwrap(), 0);
        // Counts are from the last build, which has not happened yet.
        assert_eq!(live.stats().files, 2);
        listing(&mut live);
        assert_eq!(live.stats().files, 7);
    }

    #[test]
    fn carries_on_from_saved_records() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        live.volume
            .write(40, file(ROOT, "before-saving.txt", 1), NOW);
        live.poll().unwrap();
        let cursor = live.cursor();

        // Save, as a file would be written, and stop watching.
        let origin = crate::snapshot::Origin {
            volume_serial: 1,
            journal_id: 2,
            cursor,
        };
        let mut on_disk = Vec::new();
        crate::snapshot::write(&mut on_disk, live.records(), origin).unwrap();
        drop(live);
        let saved = crate::snapshot::read(&mut on_disk.as_slice()).unwrap();
        assert_eq!(saved.origin, origin);
        let saved = saved.records;

        // While nothing was watching: one file renamed, one deleted, one
        // new. All of it long ago by the clock, which must not matter.
        drive.write(31, file(DOCS, "b.txt", 5), LONG_AGO);
        drive.delete(32, LONG_AGO);
        drive.write(41, file(ROOT, "while-away.txt", 2), LONG_AGO);

        let lookups_before = drive.lookups;
        let mut live = Live::resume(&mut drive, saved, "/", cursor).unwrap();
        assert_eq!(live.caught_up(), 4); // three files and the top folder
        assert_eq!(
            listing(&mut live),
            [
                entry("/Docs", 0),
                entry("/Docs/b.txt", 5),
                entry("/before-saving.txt", 1),
                entry("/while-away.txt", 2)
            ]
        );
        // Only what changed was looked at; before-saving.txt was not.
        assert_eq!(live.volume.lookups - lookups_before, 4);
        assert_eq!(live.poll().unwrap(), 0);
    }

    #[test]
    fn says_so_when_the_journal_no_longer_goes_back_far_enough() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        live.volume.expired = true;
        assert!(matches!(
            live.poll(),
            Err(LiveError::Journal(JournalError::Expired))
        ));
    }

    #[test]
    fn stays_right_through_many_changes_and_a_tidy_up() {
        let (mut drive, records) = drive();
        let mut live = follow(&mut drive, records);
        live.tidy_after(8);
        // The same file is renamed fifty times, with a second file coming
        // and going, so replaced rows pile up and get tidied away.
        for round in 0..50 {
            live.volume
                .write(31, file(DOCS, &format!("draft-{round}.txt"), round), NOW);
            if round % 2 == 0 {
                live.volume.write(60, file(ROOT, "temp.bin", 1), NOW);
            } else {
                live.volume.delete(60, NOW);
            }
            live.poll().unwrap();
        }
        assert_eq!(
            listing(&mut live),
            [
                entry("/Docs", 0),
                entry("/Docs/draft-49.txt", 49),
                entry("/notes.txt", 9)
            ]
        );
        assert!(live.records.stale_rows() < 8);
    }

    #[test]
    fn counts_time_from_1601() {
        // 2026 is about 425 years after 1601.
        let years = filetime_now() / 10_000_000 / 86_400 / 365;
        assert!((424..=430).contains(&years), "{years}");
    }
}
