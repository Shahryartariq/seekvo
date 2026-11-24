//! Indexing a whole NTFS drive in one go.
//!
//! Walking folders asks the file system about one folder at a time. NTFS
//! already keeps a table with one record per file, the Master File Table,
//! and Windows will hand those records over in bulk. Reading them is how a
//! drive with a million files gets indexed in seconds.
//!
//! The records arrive in table order, not folder order, and each one only
//! names its parent by number. [`build_index`] sorts that out. It is plain
//! Rust and runs anywhere; only [`read_volume`] needs Windows.
//!
//! There are two ways to get the records. [`read_file_table`] reads the
//! table directly (see [`crate::mft`]); it gives file sizes and every name
//! of a hard-linked file. [`read_records`] asks Windows for a summary of
//! each record instead; that has no sizes and one name per file, but it
//! comes out of the system's cache when the drive was read recently.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use crate::index::{BuildError, EntryId, Index, IndexBuilder, Kind};
use crate::live::LiveError;
use crate::mft::{FileTable, MftError};

/// The low 48 bits of a file reference are the record's number in the
/// table. The high 16 bits count how often that slot has been reused.
const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// The record number NTFS always gives the top folder of a drive.
const ROOT_RECORD: u64 = 5;
/// Records below this number are NTFS's own bookkeeping files.
const FIRST_ORDINARY_RECORD: u64 = 16;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// Bytes in a version 2 change-journal record before the file name.
const USN_V2_HEADER: usize = 60;

#[derive(Debug, Clone, Copy)]
struct Row {
    record: u64,
    parent: u64,
    name_start: usize,
    name_len: u16,
    is_dir: bool,
    /// Replaced or removed since it was added; skipped everywhere.
    dead: bool,
    size: u64,
    /// Seconds since 1970, or 0 when not known.
    modified: u32,
}

/// File records as they come off a drive: unordered, each naming its parent
/// by record number. All names share one buffer.
///
/// Once [`seal`](VolumeRecords::seal)ed, single records can be replaced or
/// removed cheaply, which is how changes on the drive are followed.
#[derive(Debug, Default)]
pub struct VolumeRecords {
    names: String,
    rows: Vec<Row>,
    /// After sealing: `rows[..sorted]` is in record-number order, so the
    /// rows of one record are a binary search away.
    sorted: usize,
    sealed: bool,
    /// After sealing: where the rows added since then are, by record number.
    added: HashMap<u64, Vec<usize>>,
    dead: usize,
}

/// One current row of a [`VolumeRecords`], for reading it back out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordRow<'a> {
    pub record: u64,
    /// Record number of the folder it is in.
    pub parent: u64,
    pub name: &'a str,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since 1970, or 0 when not known.
    pub modified: u32,
}

impl VolumeRecords {
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty, with room set aside for `rows` rows and `name_bytes` bytes
    /// of names.
    pub(crate) fn with_room(rows: usize, name_bytes: usize) -> Self {
        VolumeRecords {
            names: String::with_capacity(name_bytes),
            rows: Vec::with_capacity(rows),
            ..Self::default()
        }
    }

    /// Every current row. A file with two names appears twice.
    pub fn iter(&self) -> impl Iterator<Item = RecordRow<'_>> {
        self.rows
            .iter()
            .filter(|row| !row.dead)
            .map(|row| RecordRow {
                record: row.record,
                parent: row.parent,
                name: self.name(row),
                is_dir: row.is_dir,
                size: row.size,
                modified: row.modified,
            })
    }

    /// Number of rows that are still current. A file with two names has two.
    pub fn len(&self) -> usize {
        self.rows.len() - self.dead
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Add one record. `file_reference` and `parent_reference` are the
    /// 64-bit NTFS file references.
    pub fn push(&mut self, file_reference: u64, parent_reference: u64, name: &str, is_dir: bool) {
        self.push_sized(file_reference, parent_reference, name, is_dir, 0);
    }

    /// Add one record with a known size in bytes.
    pub fn push_sized(
        &mut self,
        file_reference: u64,
        parent_reference: u64,
        name: &str,
        is_dir: bool,
        size: u64,
    ) {
        self.push_dated(file_reference, parent_reference, name, is_dir, size, 0);
    }

    /// Add one record with a known size in bytes and the time it was last
    /// changed, in seconds since 1970 (0 when not known).
    pub fn push_dated(
        &mut self,
        file_reference: u64,
        parent_reference: u64,
        name: &str,
        is_dir: bool,
        size: u64,
        modified: u32,
    ) {
        let name_start = self.names.len();
        self.names.push_str(name);
        self.push_row(
            file_reference,
            parent_reference,
            name_start,
            is_dir,
            size,
            modified,
        );
    }

    pub(crate) fn push_utf16(
        &mut self,
        file_reference: u64,
        parent_reference: u64,
        units: impl Iterator<Item = u16>,
        is_dir: bool,
    ) {
        let name_start = self.names.len();
        // NTFS allows names that are not valid UTF-16. Those characters
        // become the replacement character instead of stopping the scan.
        self.names.extend(
            char::decode_utf16(units).map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER)),
        );
        self.push_row(file_reference, parent_reference, name_start, is_dir, 0, 0);
    }

    fn push_row(
        &mut self,
        file_reference: u64,
        parent_reference: u64,
        name_start: usize,
        is_dir: bool,
        size: u64,
        modified: u32,
    ) {
        // An NTFS name is at most 255 UTF-16 units, which is 765 bytes at
        // most. Anything longer than the length field can hold did not
        // come from a drive, and is cut at a character.
        let longest = usize::from(u16::MAX);
        if self.names.len() - name_start > longest {
            let mut cut = name_start + longest;
            while !self.names.is_char_boundary(cut) {
                cut -= 1;
            }
            self.names.truncate(cut);
        }
        let name_len = (self.names.len() - name_start) as u16;
        let record = file_reference & RECORD_NUMBER_MASK;
        if self.sealed {
            self.added.entry(record).or_default().push(self.rows.len());
        }
        self.rows.push(Row {
            record,
            parent: parent_reference & RECORD_NUMBER_MASK,
            name_start,
            name_len,
            is_dir,
            dead: false,
            size,
            modified,
        });
    }

    /// Fill in what the rows could not know when they were added. `lookup`
    /// gets a record number and returns whether it is a folder, its size
    /// and when it was last changed, or `None` to drop every row for that
    /// record.
    pub(crate) fn resolve(&mut self, lookup: impl Fn(u64) -> Option<(bool, u64, u32)>) {
        self.rows.retain_mut(|row| match lookup(row.record) {
            Some((is_dir, size, modified)) => {
                row.is_dir = is_dir;
                row.size = size;
                row.modified = modified;
                true
            }
            None => false,
        });
    }

    /// Get ready for single records to be replaced: drop what is no longer
    /// current, put the rest in record-number order and reclaim the space
    /// of dropped names. Call it once after loading, and again now and then
    /// when many records have been replaced.
    pub fn seal(&mut self) {
        let mut names = String::with_capacity(self.names.len());
        let mut rows: Vec<Row> = Vec::with_capacity(self.len());
        for row in self.rows.iter().filter(|row| !row.dead) {
            let name_start = names.len();
            names.push_str(self.name(row));
            rows.push(Row { name_start, ..*row });
        }
        // Stable, so a file's names stay in the order they were found.
        rows.sort_by_key(|row| row.record);
        self.names = names;
        self.sorted = rows.len();
        self.rows = rows;
        self.sealed = true;
        self.added.clear();
        self.dead = 0;
    }

    /// Rows replaced or removed since the last [`seal`](Self::seal). They
    /// still take up memory until the next one.
    pub fn stale_rows(&self) -> usize {
        self.dead
    }

    /// Positions of the current rows for one record number.
    fn rows_of(&self, record: u64) -> Vec<usize> {
        let record = record & RECORD_NUMBER_MASK;
        let sorted = &self.rows[..self.sorted];
        let first = sorted.partition_point(|row| row.record < record);
        let count = sorted[first..].partition_point(|row| row.record == record);
        let mut found: Vec<usize> = (first..first + count).collect();
        if let Some(added) = self.added.get(&record) {
            found.extend(added);
        }
        if !self.sealed {
            // Not sorted yet, so look everywhere.
            found = (0..self.rows.len())
                .filter(|&i| self.rows[i].record == record)
                .collect();
        }
        found.retain(|&i| !self.rows[i].dead);
        found
    }

    /// Forget everything known about one record number.
    pub fn remove_record(&mut self, record: u64) {
        for i in self.rows_of(record) {
            self.rows[i].dead = true;
            self.dead += 1;
        }
        self.added.remove(&(record & RECORD_NUMBER_MASK));
    }

    /// The size last recorded for a record number, if it is known at all.
    pub fn size_of(&self, record: u64) -> Option<u64> {
        self.rows_of(record).first().map(|&i| self.rows[i].size)
    }

    /// When a record number was last changed, as last recorded, if it is
    /// known at all.
    pub fn modified_of(&self, record: u64) -> Option<u32> {
        self.rows_of(record)
            .first()
            .map(|&i| self.rows[i].modified)
            .filter(|&modified| modified != 0)
    }

    fn name(&self, row: &Row) -> &str {
        &self.names[row.name_start..row.name_start + row.name_len as usize]
    }
}

fn bytes_at<const N: usize>(buffer: &[u8], at: usize) -> Option<[u8; N]> {
    buffer.get(at..at.checked_add(N)?)?.try_into().ok()
}

/// One version 2 change-journal record, as far as this module needs it.
struct UsnRecord<'a> {
    file: u64,
    parent: u64,
    usn: i64,
    /// When it happened, in 100-nanosecond steps since 1601 (a Windows
    /// `FILETIME`).
    time: i64,
    reason: u32,
    attributes: u32,
    /// The file name as UTF-16, little-endian.
    name: &'a [u8],
}

/// Walk the records that follow the 8-byte header of a change-journal
/// buffer. Damaged or unknown records are skipped; nothing outside the
/// buffer is ever read.
fn for_each_usn_record(buffer: &[u8], mut visit: impl FnMut(UsnRecord<'_>)) {
    let mut at = 8usize;
    while let Some(length) = bytes_at(buffer, at).map(u32::from_le_bytes) {
        let length = length as usize;
        let Some(end) = at.checked_add(length) else {
            break;
        };
        if length < USN_V2_HEADER || end > buffer.len() {
            break;
        }
        let record = &buffer[at..end];
        at = end;

        let major_version = bytes_at(record, 4).map(u16::from_le_bytes);
        if major_version != Some(2) {
            continue;
        }
        let (
            Some(file),
            Some(parent),
            Some(usn),
            Some(time),
            Some(reason),
            Some(attributes),
            Some(name_len),
            Some(name_offset),
        ) = (
            bytes_at(record, 8).map(u64::from_le_bytes),
            bytes_at(record, 16).map(u64::from_le_bytes),
            bytes_at(record, 24).map(i64::from_le_bytes),
            bytes_at(record, 32).map(i64::from_le_bytes),
            bytes_at(record, 40).map(u32::from_le_bytes),
            bytes_at(record, 52).map(u32::from_le_bytes),
            bytes_at(record, 56).map(u16::from_le_bytes),
            bytes_at(record, 58).map(u16::from_le_bytes),
        )
        else {
            continue;
        };
        let name_offset = name_offset as usize;
        let Some(name) = record.get(name_offset..name_offset + name_len as usize) else {
            continue;
        };
        visit(UsnRecord {
            file,
            parent,
            usn,
            time,
            reason,
            attributes,
            name,
        });
    }
}

/// Read one output buffer of `FSCTL_ENUM_USN_DATA` into `records`.
///
/// The buffer starts with the file reference to continue from, followed by
/// version 2 change-journal records. Returns that reference, or `None` if
/// the buffer is too short to hold one. Damaged or unknown records are
/// skipped; parsing never reads outside the buffer.
pub fn parse_usn_buffer(buffer: &[u8], records: &mut VolumeRecords) -> Option<u64> {
    let next = u64::from_le_bytes(bytes_at(buffer, 0)?);
    for_each_usn_record(buffer, |record| {
        let units = record
            .name
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
        let is_dir = record.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        records.push_utf16(record.file, record.parent, units, is_dir);
    });
    Some(next)
}

/// Something that happened to a file, as the drive's change journal
/// recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalEntry {
    /// Record number of the file or folder it happened to.
    pub record: u64,
    /// Record number of the folder it was in at the time.
    pub parent: u64,
    /// The entry's position in the journal.
    pub usn: i64,
    /// When it happened, in 100-nanosecond steps since 1601.
    pub time: i64,
    /// What happened, as NTFS's `USN_REASON_*` bits.
    pub reason: u32,
}

/// Read one output buffer of `FSCTL_READ_USN_JOURNAL`.
///
/// The buffer starts with the journal position to continue from, followed
/// by the entries. Returns that position and the entries, or `None` if the
/// buffer is too short to hold a position.
pub fn parse_journal_buffer(buffer: &[u8]) -> Option<(i64, Vec<JournalEntry>)> {
    let next = i64::from_le_bytes(bytes_at(buffer, 0)?);
    let mut entries = Vec::new();
    for_each_usn_record(buffer, |record| {
        entries.push(JournalEntry {
            record: record.file & RECORD_NUMBER_MASK,
            parent: record.parent & RECORD_NUMBER_MASK,
            usn: record.usn,
            time: record.time,
            reason: record.reason,
        });
    });
    Some((next, entries))
}

/// Numbers from turning a drive's records into an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VolumeStats {
    pub files: usize,
    /// Folders, counting the drive's top folder.
    pub dirs: usize,
    /// NTFS's own bookkeeping records, which are left out.
    pub system: usize,
    /// Records that could not be reached from the top folder, for example
    /// because their folder was being deleted during the scan.
    pub unreachable: usize,
}

/// Arrange unordered records into an index, parents before children.
///
/// `root_name` is how the top folder should be shown, such as `C:\`. Inside
/// each folder the entries are in name order, so the same records always
/// give the same index.
pub fn build_index(
    root_name: &str,
    records: &VolumeRecords,
) -> Result<(Index, VolumeStats), BuildError> {
    let rows = &records.rows;
    let mut order: Vec<usize> = (0..rows.len())
        .filter(|&i| !rows[i].dead && rows[i].record >= FIRST_ORDINARY_RECORD)
        .collect();
    let mut stats = VolumeStats {
        system: records.len() - order.len(),
        ..VolumeStats::default()
    };
    // Sorting by parent puts each folder's children next to each other, so
    // "the children of folder N" is a binary search away and needs no
    // per-folder lists.
    order.sort_unstable_by(|&a, &b| {
        (rows[a].parent, records.name(&rows[a]), rows[a].record).cmp(&(
            rows[b].parent,
            records.name(&rows[b]),
            rows[b].record,
        ))
    });

    let mut builder = IndexBuilder::new();
    let root = builder.add(None, root_name, Kind::Dir, 0)?;
    stats.dirs = 1;

    // A folder is entered once. Damaged data that names the same folder
    // twice, or forms a loop, cannot make the walk run forever.
    let mut entered: HashSet<u64> = HashSet::from([ROOT_RECORD]);
    let mut queue: VecDeque<(u64, EntryId)> = VecDeque::from([(ROOT_RECORD, root)]);
    let mut placed = 0usize;

    while let Some((folder, folder_id)) = queue.pop_front() {
        let first = order.partition_point(|&i| rows[i].parent < folder);
        let count = order[first..].partition_point(|&i| rows[i].parent == folder);
        for &i in &order[first..first + count] {
            let row = &rows[i];
            if row.is_dir {
                if !entered.insert(row.record) {
                    continue;
                }
                let id = builder.add_dated(
                    Some(folder_id),
                    records.name(row),
                    Kind::Dir,
                    0,
                    row.modified,
                )?;
                stats.dirs += 1;
                queue.push_back((row.record, id));
            } else {
                builder.add_dated(
                    Some(folder_id),
                    records.name(row),
                    Kind::File,
                    row.size,
                    row.modified,
                )?;
                stats.files += 1;
            }
            placed += 1;
        }
    }

    stats.unreachable = order.len() - placed;
    Ok((builder.finish(), stats))
}

/// Why a drive could not be read.
#[derive(Debug)]
pub enum VolumeError {
    /// Not a drive letter.
    BadDrive(char),
    /// Reading the file table directly only works on Windows.
    Unsupported,
    /// The drive uses another file system; its name is included.
    NotNtfs(String),
    /// Windows only lets administrators read a drive's file table.
    NeedsAdmin,
    Io(io::Error),
    /// The file table could not be read directly.
    Table(MftError),
    Build(BuildError),
    /// The drive was read, but its changes cannot be followed.
    Live(LiveError),
}

impl fmt::Display for VolumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VolumeError::BadDrive(drive) => write!(f, "{drive} is not a drive letter"),
            VolumeError::Unsupported => {
                write!(f, "reading a drive's file table only works on Windows")
            }
            VolumeError::NotNtfs(found) => {
                write!(f, "the drive is formatted as {found}, not NTFS")
            }
            VolumeError::NeedsAdmin => write!(
                f,
                "Windows only lets administrators read a drive's file table; \
                 run this from an administrator terminal"
            ),
            VolumeError::Io(error) => write!(f, "cannot read the drive: {error}"),
            VolumeError::Table(error) => write!(f, "{error}"),
            VolumeError::Build(error) => write!(f, "cannot build the index: {error}"),
            VolumeError::Live(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for VolumeError {}

/// Read an NTFS drive's file table directly, given the drive's letter.
///
/// This gives file sizes and every name of a hard-linked file. Needs
/// Windows and an administrator. Everywhere else it returns
/// [`VolumeError::Unsupported`].
///
/// The drive is read below the file system, so a file created in the last
/// few seconds may not have reached the table on disk yet.
pub fn read_file_table(drive: char) -> Result<FileTable, VolumeError> {
    if !drive.is_ascii_alphabetic() {
        return Err(VolumeError::BadDrive(drive));
    }
    #[cfg(windows)]
    {
        windows::read_file_table(drive.to_ascii_uppercase())
    }
    #[cfg(not(windows))]
    {
        Err(VolumeError::Unsupported)
    }
}

/// Ask Windows for a summary of every file record on an NTFS drive, given
/// its letter.
///
/// No sizes, and one name per file. Needs Windows and an administrator.
/// Everywhere else it returns [`VolumeError::Unsupported`].
pub fn read_records(drive: char) -> Result<VolumeRecords, VolumeError> {
    if !drive.is_ascii_alphabetic() {
        return Err(VolumeError::BadDrive(drive));
    }
    #[cfg(windows)]
    {
        windows::read_records(drive.to_ascii_uppercase())
    }
    #[cfg(not(windows))]
    {
        Err(VolumeError::Unsupported)
    }
}

/// Index every file on an NTFS drive, given its letter:
/// [`read_file_table`] followed by [`build_index`].
pub fn read_volume(drive: char) -> Result<(Index, VolumeStats), VolumeError> {
    let table = read_file_table(drive)?;
    let root = format!("{}:\\", drive.to_ascii_uppercase());
    build_index(&root, &table.records).map_err(VolumeError::Build)
}

/// An index of an NTFS drive that follows the drive as files change.
///
/// Opening it reads the whole file table once. After that,
/// [`poll`](LiveDrive::poll) asks the drive's change journal what happened
/// since the last call, which costs next to nothing when nothing did.
/// Needs Windows and an administrator.
pub struct LiveDrive {
    #[cfg(windows)]
    inner: crate::live::Live<windows::Followed>,
    #[cfg(not(windows))]
    never: std::convert::Infallible,
}

/// Where a [`LiveDrive`]'s index came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// Loaded from the saved file, then brought up to date from the
    /// drive's change journal. The drive itself was not read.
    FromSaved,
    /// Read from the drive in full, because the saved file could not be
    /// used. Says why.
    ReadInFull(String),
}

impl LiveDrive {
    /// Read a drive, given its letter, and start following it.
    pub fn open(drive: char) -> Result<Self, VolumeError> {
        if !drive.is_ascii_alphabetic() {
            return Err(VolumeError::BadDrive(drive));
        }
        #[cfg(windows)]
        {
            let inner = windows::follow(drive.to_ascii_uppercase())?;
            Ok(LiveDrive { inner })
        }
        #[cfg(not(windows))]
        {
            Err(VolumeError::Unsupported)
        }
    }

    /// Start following a drive from an index [`save`](Self::save)d
    /// earlier, without reading the drive.
    ///
    /// Everything that happened on the drive since the save is taken in
    /// from its change journal. When the saved file is missing, damaged,
    /// from another drive, or older than the journal goes back, the drive
    /// is read in full instead, exactly as [`open`](Self::open) does.
    pub fn open_saved(drive: char, saved: &Path) -> Result<(Self, Start), VolumeError> {
        if !drive.is_ascii_alphabetic() {
            return Err(VolumeError::BadDrive(drive));
        }
        #[cfg(windows)]
        {
            let drive = drive.to_ascii_uppercase();
            let why = match std::fs::File::open(saved) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    "nothing has been saved yet".to_owned()
                }
                Err(error) => format!("the saved index cannot be opened: {error}"),
                Ok(mut file) => match crate::snapshot::read(&mut file) {
                    Err(error) => error.to_string(),
                    Ok(saved) => match windows::resume(drive, saved)? {
                        Ok(inner) => return Ok((LiveDrive { inner }, Start::FromSaved)),
                        Err(why) => why.to_owned(),
                    },
                },
            };
            let inner = windows::follow(drive)?;
            Ok((LiveDrive { inner }, Start::ReadInFull(why)))
        }
        #[cfg(not(windows))]
        {
            let _ = saved;
            Err(VolumeError::Unsupported)
        }
    }

    /// Save the index as it is now, for [`open_saved`](Self::open_saved).
    ///
    /// The file is written under another name first and then moved into
    /// place, so a crash halfway leaves the previous save intact.
    pub fn save(&self, saved: &Path) -> io::Result<()> {
        #[cfg(windows)]
        {
            let origin = self.inner.volume().origin(self.inner.cursor());
            let mut partial = saved.as_os_str().to_owned();
            partial.push(".partial");
            let partial = std::path::PathBuf::from(partial);
            let mut file = std::fs::File::create(&partial)?;
            crate::snapshot::write(&mut file, self.inner.records(), origin)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&partial, saved)
        }
        #[cfg(not(windows))]
        {
            let _ = saved;
            match self.never {}
        }
    }

    /// How long catching up took while opening.
    pub fn caught_up_in(&self) -> Duration {
        #[cfg(windows)]
        {
            self.inner.caught_up_in()
        }
        #[cfg(not(windows))]
        {
            match self.never {}
        }
    }

    /// Take in what changed on the drive since the last call. Returns how
    /// many files and folders were looked at again; 0 means nothing
    /// changed.
    pub fn poll(&mut self) -> Result<usize, LiveError> {
        #[cfg(windows)]
        {
            self.inner.poll()
        }
        #[cfg(not(windows))]
        {
            match self.never {}
        }
    }

    /// The index, rebuilt first if a poll changed anything.
    pub fn index(&mut self) -> Result<&Index, LiveError> {
        #[cfg(windows)]
        {
            self.inner.index()
        }
        #[cfg(not(windows))]
        {
            match self.never {}
        }
    }

    /// Counts from the last time the index was built.
    pub fn stats(&self) -> VolumeStats {
        #[cfg(windows)]
        {
            self.inner.stats()
        }
        #[cfg(not(windows))]
        {
            match self.never {}
        }
    }

    /// How many records were looked at again while opening, to catch up
    /// with changes made around the time the drive was read.
    pub fn caught_up(&self) -> usize {
        #[cfg(windows)]
        {
            self.inner.caught_up()
        }
        #[cfg(not(windows))]
        {
            match self.never {}
        }
    }
}

// `seekvo_check_windows` lets the Windows code be type-checked on other
// systems with `RUSTFLAGS="--cfg seekvo_check_windows" cargo check`.
#[cfg(any(windows, seekvo_check_windows))]
#[cfg_attr(not(windows), allow(dead_code))]
mod windows {
    use std::ffi::c_void;
    use std::io;
    use std::ptr;

    use super::{
        RECORD_NUMBER_MASK, VolumeError, VolumeRecords, bytes_at, parse_journal_buffer,
        parse_usn_buffer,
    };
    use crate::live::{
        Journal, JournalBatch, JournalError, Live, LiveError, RecordSource, filetime_now,
    };
    use crate::mft::{self, BlockSource, FileTable};
    use crate::snapshot::{Origin, Saved};

    type Handle = *mut c_void;

    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const OPEN_EXISTING: u32 = 3;
    const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00B3;
    const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
    const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;
    const FSCTL_GET_NTFS_FILE_RECORD: u32 = 0x0009_0068;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_HANDLE_EOF: i32 = 38;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
    const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
    const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;
    /// Room for one file record and the 12 bytes Windows puts before it.
    /// Records are 1 KB on most drives and 4 KB on some.
    const RECORD_BUFFER_BYTES: usize = 1 << 16;
    /// Changes this long before the full read began are looked at again
    /// afterwards, in 100-nanosecond steps: two minutes.
    const REPLAY_MARGIN: i64 = 120 * 10_000_000;
    const FILE_BEGIN: u32 = 0;
    /// The most to ask `ReadFile` for at once; a whole number of sectors.
    const READ_LIMIT: usize = 1 << 30;
    /// Each call fills up to this much with records.
    const BUFFER_BYTES: usize = 1 << 20;

    /// `MFT_ENUM_DATA_V0`: where to start, and which change numbers to
    /// include.
    #[repr(C)]
    struct EnumData {
        start_file_reference: u64,
        low_usn: i64,
        high_usn: i64,
    }

    /// `USN_JOURNAL_DATA_V0`: what a drive says about its change journal.
    #[repr(C)]
    #[derive(Default)]
    struct JournalData {
        journal_id: u64,
        first_usn: i64,
        next_usn: i64,
        lowest_valid_usn: i64,
        max_usn: i64,
        maximum_size: u64,
        allocation_delta: u64,
    }

    /// `READ_USN_JOURNAL_DATA_V0`: where to read the journal from.
    #[repr(C)]
    struct ReadJournal {
        start_usn: i64,
        reason_mask: u32,
        return_only_on_close: u32,
        timeout: u64,
        bytes_to_wait_for: u64,
        journal_id: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: Handle,
        ) -> Handle;

        fn DeviceIoControl(
            device: Handle,
            control_code: u32,
            in_buffer: *const c_void,
            in_buffer_size: u32,
            out_buffer: *mut c_void,
            out_buffer_size: u32,
            bytes_returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;

        fn GetVolumeInformationW(
            root_path_name: *const u16,
            volume_name: *mut u16,
            volume_name_size: u32,
            serial_number: *mut u32,
            maximum_component_length: *mut u32,
            file_system_flags: *mut u32,
            file_system_name: *mut u16,
            file_system_name_size: u32,
        ) -> i32;

        fn SetFilePointerEx(
            file: Handle,
            distance_to_move: i64,
            new_file_pointer: *mut i64,
            move_method: u32,
        ) -> i32;

        fn ReadFile(
            file: Handle,
            buffer: *mut c_void,
            bytes_to_read: u32,
            bytes_read: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;

        fn CloseHandle(object: Handle) -> i32;
    }

    /// An open drive that closes itself, and its serial number.
    struct Volume(Handle, u32);

    impl Drop for Volume {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateFileW and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    impl BlockSource for Volume {
        fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
            let distance =
                i64::try_from(offset).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            // SAFETY: the handle is open, and the optional output is null.
            if unsafe { SetFilePointerEx(self.0, distance, ptr::null_mut(), FILE_BEGIN) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut done = 0usize;
            while done < buffer.len() {
                let rest = &mut buffer[done..];
                let want = rest.len().min(READ_LIMIT) as u32;
                let mut got = 0u32;
                // SAFETY: `rest` is writable for `want` bytes and lives
                // across the call; no overlapped I/O is used.
                let ok = unsafe {
                    ReadFile(
                        self.0,
                        rest.as_mut_ptr().cast(),
                        want,
                        &mut got,
                        ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                if got == 0 {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                done += got as usize;
            }
            Ok(())
        }
    }

    /// A NUL-terminated UTF-16 copy of `text`, as Windows wants its strings.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    /// The name of a drive's file system, and the drive's serial number.
    fn file_system_of(root: &str) -> io::Result<(String, u32)> {
        let root = wide(root);
        let mut name = [0u16; 64];
        let mut serial = 0u32;
        // SAFETY: `root` is NUL-terminated, `name` is writable for the
        // length passed, and the other outputs are allowed to be null.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                ptr::null_mut(),
                0,
                &mut serial,
                ptr::null_mut(),
                ptr::null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = name
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(name.len());
        Ok((String::from_utf16_lossy(&name[..length]), serial))
    }

    /// Open a drive for reading, after checking it is NTFS.
    fn open(drive: char) -> Result<Volume, VolumeError> {
        let root = format!("{drive}:\\");
        let (file_system, serial) = file_system_of(&root).map_err(VolumeError::Io)?;
        if file_system != "NTFS" {
            return Err(VolumeError::NotNtfs(file_system));
        }

        let device = wide(&format!("\\\\.\\{drive}:"));
        // SAFETY: `device` is NUL-terminated and outlives the call; the
        // optional pointers are null.
        let handle = unsafe {
            CreateFileW(
                device.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null_mut(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let error = io::Error::last_os_error();
            return Err(if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
                VolumeError::NeedsAdmin
            } else {
                VolumeError::Io(error)
            });
        }
        Ok(Volume(handle, serial))
    }

    pub(super) fn read_file_table(drive: char) -> Result<FileTable, VolumeError> {
        let mut volume = open(drive)?;
        mft::read_file_table(&mut volume).map_err(VolumeError::Table)
    }

    pub(super) fn read_records(drive: char) -> Result<VolumeRecords, VolumeError> {
        let volume = open(drive)?;

        let mut records = VolumeRecords::new();
        // u64 elements keep the buffer aligned for the records Windows
        // writes into it.
        let mut buffer = vec![0u64; BUFFER_BYTES / 8];
        let mut request = EnumData {
            start_file_reference: 0,
            low_usn: 0,
            high_usn: i64::MAX,
        };
        loop {
            let mut returned = 0u32;
            // SAFETY: `request` and `buffer` are valid for the sizes passed
            // and live across the call; no overlapped I/O is used.
            let ok = unsafe {
                DeviceIoControl(
                    volume.0,
                    FSCTL_ENUM_USN_DATA,
                    ptr::from_ref(&request).cast(),
                    size_of::<EnumData>() as u32,
                    buffer.as_mut_ptr().cast(),
                    BUFFER_BYTES as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_HANDLE_EOF) {
                    break;
                }
                return Err(VolumeError::Io(error));
            }
            let filled = (returned as usize).min(BUFFER_BYTES);
            // SAFETY: Windows wrote `filled` bytes into `buffer`, which is
            // at least that long, and u8 has no alignment requirement.
            let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), filled) };
            match parse_usn_buffer(bytes, &mut records) {
                // More than the 8-byte header means records came back and
                // there may be more after them.
                Some(next) if filled > 8 => request.start_file_reference = next,
                _ => break,
            }
        }
        Ok(records)
    }

    /// An open drive whose single records and change journal can be read.
    pub(super) struct Followed {
        volume: Volume,
        journal_id: u64,
        // u64 elements keep both buffers aligned for what Windows writes
        // into them.
        record: Vec<u64>,
        journal: Vec<u64>,
    }

    /// The bytes Windows wrote into the front of a buffer.
    fn filled(buffer: &[u64], returned: u32) -> &[u8] {
        let length = (returned as usize).min(buffer.len() * 8);
        // SAFETY: `length` is at most the buffer's size in bytes, and u8
        // has no alignment requirement.
        unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), length) }
    }

    impl RecordSource for Followed {
        fn file_record(&mut self, number: u64) -> io::Result<Option<Vec<u8>>> {
            let wanted = number & RECORD_NUMBER_MASK;
            let mut returned = 0u32;
            // SAFETY: the input is 8 readable bytes, the output buffer is
            // writable for the size passed, and both live across the call.
            let ok = unsafe {
                DeviceIoControl(
                    self.volume.0,
                    FSCTL_GET_NTFS_FILE_RECORD,
                    ptr::from_ref(&wanted).cast(),
                    size_of::<u64>() as u32,
                    self.record.as_mut_ptr().cast(),
                    RECORD_BUFFER_BYTES as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    // Past the end of the table: no such record.
                    Some(ERROR_INVALID_PARAMETER | ERROR_HANDLE_EOF) => Ok(None),
                    _ => Err(error),
                };
            }
            let bytes = filled(&self.record, returned);
            let (Some(found), Some(length)) = (
                bytes_at(bytes, 0).map(u64::from_le_bytes),
                bytes_at(bytes, 8).map(u32::from_le_bytes),
            ) else {
                return Ok(None);
            };
            // When the record asked for is not in use, Windows hands over
            // the nearest one below it that is.
            if found & RECORD_NUMBER_MASK != wanted {
                return Ok(None);
            }
            Ok(bytes.get(12..12 + length as usize).map(<[u8]>::to_vec))
        }
    }

    impl Journal for Followed {
        fn read_from(&mut self, from: i64) -> Result<JournalBatch, JournalError> {
            let request = ReadJournal {
                start_usn: from,
                reason_mask: u32::MAX,
                return_only_on_close: 0,
                // Return at once, also when there is nothing new.
                timeout: 0,
                bytes_to_wait_for: 0,
                journal_id: self.journal_id,
            };
            let mut returned = 0u32;
            // SAFETY: `request` and the output buffer are valid for the
            // sizes passed and live across the call.
            let ok = unsafe {
                DeviceIoControl(
                    self.volume.0,
                    FSCTL_READ_USN_JOURNAL,
                    ptr::from_ref(&request).cast(),
                    size_of::<ReadJournal>() as u32,
                    self.journal.as_mut_ptr().cast(),
                    BUFFER_BYTES as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(journal_error(io::Error::last_os_error()));
            }
            let (next, entries) = parse_journal_buffer(filled(&self.journal, returned))
                .ok_or_else(|| JournalError::Io(io::Error::from(io::ErrorKind::InvalidData)))?;
            Ok(JournalBatch { next, entries })
        }
    }

    fn journal_error(error: io::Error) -> JournalError {
        match error.raw_os_error() {
            Some(ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS) => {
                JournalError::NotActive
            }
            // The place to continue from is gone, or the journal was
            // deleted and made again under a new id.
            Some(ERROR_JOURNAL_ENTRY_DELETED | ERROR_INVALID_PARAMETER) => JournalError::Expired,
            _ => JournalError::Io(error),
        }
    }

    fn journal_of(volume: &Volume) -> Result<JournalData, JournalError> {
        let mut data = JournalData::default();
        let mut returned = 0u32;
        // SAFETY: there is no input, and `data` is writable for its size.
        let ok = unsafe {
            DeviceIoControl(
                volume.0,
                FSCTL_QUERY_USN_JOURNAL,
                ptr::null(),
                0,
                ptr::from_mut(&mut data).cast(),
                size_of::<JournalData>() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(journal_error(io::Error::last_os_error()));
        }
        Ok(data)
    }

    impl Followed {
        /// What to note in a saved index, so it is only ever used for this
        /// drive and this journal.
        pub(super) fn origin(&self, cursor: i64) -> Origin {
            Origin {
                volume_serial: self.volume.1,
                journal_id: self.journal_id,
                cursor,
            }
        }
    }

    fn followed(volume: Volume, journal: &JournalData) -> Followed {
        Followed {
            volume,
            journal_id: journal.journal_id,
            record: vec![0u64; RECORD_BUFFER_BYTES / 8],
            journal: vec![0u64; BUFFER_BYTES / 8],
        }
    }

    fn journal_or_error(volume: &Volume) -> Result<JournalData, VolumeError> {
        journal_of(volume).map_err(|error| VolumeError::Live(LiveError::Journal(error)))
    }

    /// Read a drive in full and start following its change journal.
    pub(super) fn follow(drive: char) -> Result<Live<Followed>, VolumeError> {
        let replay_since = filetime_now() - REPLAY_MARGIN;
        let mut volume = open(drive)?;
        let journal = journal_or_error(&volume)?;
        let table = mft::read_file_table(&mut volume).map_err(VolumeError::Table)?;
        let root = format!("{drive}:\\");
        Live::start(
            followed(volume, &journal),
            table.records,
            &root,
            replay_since,
        )
        .map_err(VolumeError::Live)
    }

    /// Carry on from a saved index. The inner error says why the saved
    /// index cannot be used for this drive as it is now.
    pub(super) fn resume(
        drive: char,
        saved: Saved,
    ) -> Result<Result<Live<Followed>, &'static str>, VolumeError> {
        const TOO_OLD: &str = "the drive's change journal no longer goes back to the last save";
        let volume = open(drive)?;
        let journal = journal_or_error(&volume)?;
        let origin = saved.origin;
        if origin.volume_serial != volume.1 {
            return Ok(Err("the saved index is from another drive"));
        }
        if origin.journal_id != journal.journal_id
            || origin.cursor < journal.first_usn
            || origin.cursor > journal.next_usn
        {
            return Ok(Err(TOO_OLD));
        }
        let root = format!("{drive}:\\");
        match Live::resume(
            followed(volume, &journal),
            saved.records,
            &root,
            origin.cursor,
        ) {
            Ok(live) => Ok(Ok(live)),
            Err(LiveError::Journal(JournalError::Expired)) => Ok(Err(TOO_OLD)),
            Err(other) => Err(VolumeError::Live(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;

    /// A file reference with a reuse count in the high bits, as real ones
    /// have.
    fn reference(record: u64) -> u64 {
        (7 << 48) | record
    }

    fn paths(index: &Index) -> Vec<String> {
        (0..index.len() as EntryId)
            .map(|id| index.path(id).unwrap().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn puts_parents_first_whatever_order_records_arrive_in() {
        let mut records = VolumeRecords::new();
        // Deepest first: the file, then its folder, then that folder's parent.
        records.push(reference(300), reference(200), "invoice.pdf", false);
        records.push(reference(200), reference(100), "2026", true);
        records.push(reference(100), reference(ROOT_RECORD), "Finance", true);
        records.push(
            reference(101),
            reference(ROOT_RECORD),
            "autoexec.bat",
            false,
        );

        let (index, stats) = build_index("C:\\", &records).unwrap();
        assert_eq!(
            paths(&index),
            [
                "C:/",
                "C:/Finance",
                "C:/autoexec.bat",
                "C:/Finance/2026",
                "C:/Finance/2026/invoice.pdf",
            ]
        );
        assert_eq!(
            stats,
            VolumeStats {
                files: 2,
                dirs: 3,
                system: 0,
                unreachable: 0
            }
        );
        let result = index.search(&Query::parse("invoice finance"), 10);
        assert_eq!(result.total, 1);
    }

    #[test]
    fn lists_each_folder_in_name_order() {
        let mut records = VolumeRecords::new();
        for (record, name) in [(30, "b.txt"), (31, "a.txt"), (32, "c.txt")] {
            records.push(reference(record), reference(ROOT_RECORD), name, false);
        }
        let (index, _) = build_index("D:\\", &records).unwrap();
        assert_eq!(paths(&index), ["D:/", "D:/a.txt", "D:/b.txt", "D:/c.txt"]);
    }

    #[test]
    fn leaves_out_the_drives_own_bookkeeping_files() {
        let mut records = VolumeRecords::new();
        records.push(reference(0), reference(ROOT_RECORD), "$MFT", false);
        records.push(reference(ROOT_RECORD), reference(ROOT_RECORD), ".", true);
        records.push(reference(11), reference(ROOT_RECORD), "$Extend", true);
        // Lives inside $Extend, which is left out, so it cannot be reached.
        records.push(reference(25), reference(11), "$UsnJrnl", false);
        records.push(reference(40), reference(ROOT_RECORD), "$Recycle.Bin", true);
        records.push(reference(41), reference(ROOT_RECORD), "notes.txt", false);

        let (index, stats) = build_index("C:\\", &records).unwrap();
        assert_eq!(paths(&index), ["C:/", "C:/$Recycle.Bin", "C:/notes.txt"]);
        assert_eq!(
            stats,
            VolumeStats {
                files: 1,
                dirs: 2,
                system: 3,
                unreachable: 1
            }
        );
    }

    #[test]
    fn counts_records_whose_folder_is_missing() {
        let mut records = VolumeRecords::new();
        records.push(reference(50), reference(999), "lost.txt", false);
        records.push(reference(51), reference(ROOT_RECORD), "kept.txt", false);
        let (index, stats) = build_index("C:\\", &records).unwrap();
        assert_eq!(paths(&index), ["C:/", "C:/kept.txt"]);
        assert_eq!(stats.unreachable, 1);
    }

    #[test]
    fn damaged_records_cannot_make_it_loop() {
        let mut records = VolumeRecords::new();
        // Folder 60 is inside 61, and a second record claims 61 is inside 60.
        records.push(reference(60), reference(ROOT_RECORD), "a", true);
        records.push(reference(61), reference(60), "b", true);
        records.push(reference(60), reference(61), "a-again", true);
        // Two folders that only contain each other.
        records.push(reference(70), reference(71), "x", true);
        records.push(reference(71), reference(70), "y", true);

        let (index, stats) = build_index("C:\\", &records).unwrap();
        assert_eq!(paths(&index), ["C:/", "C:/a", "C:/a/b"]);
        assert_eq!(stats.dirs, 3);
        assert_eq!(stats.unreachable, 3);
    }

    #[test]
    fn ignores_the_reuse_count_in_a_file_reference() {
        let mut records = VolumeRecords::new();
        records.push((3 << 48) | 80, (9 << 48) | ROOT_RECORD, "Docs", true);
        // The child names its parent with a different reuse count.
        records.push((1 << 48) | 81, (2 << 48) | 80, "a.txt", false);
        let (index, _) = build_index("C:\\", &records).unwrap();
        assert_eq!(paths(&index), ["C:/", "C:/Docs", "C:/Docs/a.txt"]);
    }

    /// One version 2 change-journal record, padded to 8 bytes as Windows
    /// writes them.
    fn usn_record(file: u64, parent: u64, attributes: u32, name: &[u16]) -> Vec<u8> {
        let name_bytes = name.len() * 2;
        let length = (USN_V2_HEADER + name_bytes).next_multiple_of(8);
        let mut record = vec![0u8; length];
        record[0..4].copy_from_slice(&(length as u32).to_le_bytes());
        record[4..6].copy_from_slice(&2u16.to_le_bytes());
        record[8..16].copy_from_slice(&file.to_le_bytes());
        record[16..24].copy_from_slice(&parent.to_le_bytes());
        record[52..56].copy_from_slice(&attributes.to_le_bytes());
        record[56..58].copy_from_slice(&(name_bytes as u16).to_le_bytes());
        record[58..60].copy_from_slice(&(USN_V2_HEADER as u16).to_le_bytes());
        for (i, unit) in name.iter().enumerate() {
            let at = USN_V2_HEADER + i * 2;
            record[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        record
    }

    fn names_of(records: &VolumeRecords, record: u64) -> Vec<String> {
        records
            .rows_of(record)
            .iter()
            .map(|&i| records.name(&records.rows[i]).to_owned())
            .collect()
    }

    #[test]
    fn replaces_and_removes_single_records() {
        let mut records = VolumeRecords::new();
        // Out of order on purpose; record 40 has two names.
        records.push_sized(40, ROOT_RECORD, "b.txt", false, 7);
        records.push_sized(30, ROOT_RECORD, "Docs", true, 0);
        records.push_sized(40, 30, "also-b.txt", false, 7);
        records.push_sized(35, 30, "a.txt", false, 3);
        // Before sealing, single records can be found, only slowly.
        assert_eq!(names_of(&records, 40), ["b.txt", "also-b.txt"]);

        records.seal();
        assert_eq!(records.len(), 4);
        assert_eq!(names_of(&records, 40), ["b.txt", "also-b.txt"]);
        assert_eq!(records.size_of(40), Some(7));
        assert_eq!(records.size_of(99), None);

        // Replace record 40 with one name, and add a new record.
        records.remove_record(40);
        assert_eq!(names_of(&records, 40), [] as [&str; 0]);
        records.push_sized(40, 30, "renamed.txt", false, 8);
        records.push_sized(50, ROOT_RECORD, "new.txt", false, 1);
        assert_eq!(names_of(&records, 40), ["renamed.txt"]);
        assert_eq!(records.size_of(40), Some(8));
        assert_eq!(records.len(), 4);
        assert_eq!(records.stale_rows(), 2);

        // Replace it again: the row added after sealing goes too.
        records.remove_record(40);
        records.push_sized(40, 30, "final.txt", false, 9);
        assert_eq!(names_of(&records, 40), ["final.txt"]);

        let (index, stats) = build_index("/", &records).unwrap();
        assert_eq!(stats.files, 3);
        assert_eq!(stats.dirs, 2);
        assert_eq!(
            paths(&index),
            ["/", "/Docs", "/new.txt", "/Docs/a.txt", "/Docs/final.txt"]
        );

        // Tidying up changes nothing that can be seen.
        records.seal();
        assert_eq!(records.stale_rows(), 0);
        assert_eq!(records.len(), 4);
        assert_eq!(names_of(&records, 40), ["final.txt"]);
        let (again, _) = build_index("/", &records).unwrap();
        assert_eq!(paths(&again), paths(&index));
    }

    #[test]
    fn a_name_too_long_for_any_drive_is_cut_at_a_character() {
        let mut records = VolumeRecords::new();
        let long = "é".repeat(40_000); // 80,000 bytes
        records.push_dated(30, ROOT_RECORD, &long, false, 1, 7);
        records.push_dated(31, ROOT_RECORD, "after.txt", false, 2, 8);
        let rows: Vec<RecordRow<'_>> = records.iter().collect();
        assert_eq!(rows[0].name.len(), 65_534);
        assert!(rows[0].name.chars().all(|c| c == 'é'));
        assert_eq!(
            (rows[1].name, rows[1].size, rows[1].modified),
            ("after.txt", 2, 8)
        );
        assert_eq!(records.modified_of(31), Some(8));
        assert_eq!(records.modified_of(99), None);
    }

    #[test]
    fn parses_a_buffer_of_journal_entries() {
        let mut first = usn_record(reference(40), reference(5), 0, &utf16("a.txt"));
        first[24..32].copy_from_slice(&1_000i64.to_le_bytes());
        first[32..40].copy_from_slice(&134_000_000_000_000_000i64.to_le_bytes());
        first[40..44].copy_from_slice(&0x100u32.to_le_bytes());
        let mut second = usn_record(reference(41), reference(40), 0x10, &utf16("Docs"));
        second[24..32].copy_from_slice(&1_080i64.to_le_bytes());
        second[40..44].copy_from_slice(&0x8000_0200u32.to_le_bytes());

        let mut buffer = 1_160i64.to_le_bytes().to_vec();
        buffer.extend(&first);
        buffer.extend(&second);

        let (next, entries) = parse_journal_buffer(&buffer).unwrap();
        assert_eq!(next, 1_160);
        assert_eq!(
            entries,
            [
                JournalEntry {
                    record: 40,
                    parent: 5,
                    usn: 1_000,
                    time: 134_000_000_000_000_000,
                    reason: 0x100,
                },
                JournalEntry {
                    record: 41,
                    parent: 40,
                    usn: 1_080,
                    time: 0,
                    reason: 0x8000_0200,
                },
            ]
        );

        // Nothing new: only the position comes back.
        assert_eq!(
            parse_journal_buffer(&1_160i64.to_le_bytes()),
            Some((1_160, Vec::new()))
        );
        assert_eq!(parse_journal_buffer(&[1, 2, 3]), None);
        // Cut off in the middle of an entry: what came before still counts.
        let (_, entries) = parse_journal_buffer(&buffer[..buffer.len() - 10]).unwrap();
        assert_eq!(entries.len(), 1);
    }

    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn parses_a_buffer_of_records() {
        let mut buffer = 0xABCDu64.to_le_bytes().to_vec();
        buffer.extend(usn_record(
            reference(100),
            reference(ROOT_RECORD),
            0x10,
            &utf16("Fotos"),
        ));
        buffer.extend(usn_record(
            reference(101),
            reference(100),
            0x20,
            &utf16("Über 📷.jpg"),
        ));

        let mut records = VolumeRecords::new();
        assert_eq!(parse_usn_buffer(&buffer, &mut records), Some(0xABCD));
        assert_eq!(records.len(), 2);

        let (index, stats) = build_index("C:\\", &records).unwrap();
        assert_eq!(paths(&index), ["C:/", "C:/Fotos", "C:/Fotos/Über 📷.jpg"]);
        assert_eq!((stats.files, stats.dirs), (1, 2));
    }

    #[test]
    fn replaces_broken_utf16_instead_of_stopping() {
        let mut name = utf16("bad");
        name.push(0xD800); // half of a surrogate pair
        name.extend(utf16(".txt"));
        let mut buffer = 0u64.to_le_bytes().to_vec();
        buffer.extend(usn_record(reference(100), reference(ROOT_RECORD), 0, &name));

        let mut records = VolumeRecords::new();
        parse_usn_buffer(&buffer, &mut records);
        let (index, _) = build_index("C:\\", &records).unwrap();
        assert_eq!(index.name(1), Some("bad\u{FFFD}.txt"));
    }

    #[test]
    fn skips_other_record_versions() {
        let mut odd = usn_record(reference(100), reference(ROOT_RECORD), 0, &utf16("v3.txt"));
        odd[4..6].copy_from_slice(&3u16.to_le_bytes());
        let mut buffer = 0u64.to_le_bytes().to_vec();
        buffer.extend(odd);
        buffer.extend(usn_record(
            reference(101),
            reference(ROOT_RECORD),
            0,
            &utf16("v2.txt"),
        ));

        let mut records = VolumeRecords::new();
        parse_usn_buffer(&buffer, &mut records);
        assert_eq!(records.len(), 1);
        assert_eq!(records.name(&records.rows[0]), "v2.txt");
    }

    #[test]
    fn survives_damaged_buffers() {
        let good = usn_record(reference(100), reference(ROOT_RECORD), 0, &utf16("ok.txt"));
        let mut records = VolumeRecords::new();

        // Too short to hold even the header.
        assert_eq!(parse_usn_buffer(&[1, 2, 3], &mut records), None);
        // A header and nothing else.
        assert_eq!(parse_usn_buffer(&9u64.to_le_bytes(), &mut records), Some(9));

        // A record cut off in the middle.
        let mut cut = 0u64.to_le_bytes().to_vec();
        cut.extend(&good[..good.len() - 10]);
        parse_usn_buffer(&cut, &mut records);

        // A record that claims a length of zero, which must not spin.
        let mut zero = 0u64.to_le_bytes().to_vec();
        zero.extend([0u8; 64]);
        parse_usn_buffer(&zero, &mut records);

        // A record whose length runs far past the buffer.
        let mut huge = 0u64.to_le_bytes().to_vec();
        let mut liar = good.clone();
        liar[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        huge.extend(liar);
        parse_usn_buffer(&huge, &mut records);

        // A name that points outside its record.
        let mut outside = 0u64.to_le_bytes().to_vec();
        let mut wild = good.clone();
        wild[56..58].copy_from_slice(&4000u16.to_le_bytes());
        outside.extend(wild);
        parse_usn_buffer(&outside, &mut records);

        assert!(records.is_empty());

        // The good record on its own still parses.
        let mut fine = 0u64.to_le_bytes().to_vec();
        fine.extend(&good);
        parse_usn_buffer(&fine, &mut records);
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn rejects_something_that_is_not_a_drive_letter() {
        assert!(matches!(read_volume('1'), Err(VolumeError::BadDrive('1'))));
        assert!(matches!(read_volume('é'), Err(VolumeError::BadDrive('é'))));
    }

    #[cfg(not(windows))]
    #[test]
    fn says_so_when_not_on_windows() {
        assert!(matches!(read_volume('C'), Err(VolumeError::Unsupported)));
    }

    #[cfg(windows)]
    fn system_drive() -> char {
        std::env::var("SystemDrive")
            .ok()
            .and_then(|value| value.chars().next())
            .unwrap_or('C')
    }

    /// A path as the index shows it: no `\\?\` prefix, lowercase.
    #[cfg(windows)]
    fn plain(path: &std::path::Path) -> String {
        path.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_lowercase()
    }

    #[cfg(windows)]
    fn assert_looks_like_windows(index: &Index, stats: &VolumeStats, drive: char) {
        use crate::index::Kind;

        assert!(stats.files > 10_000, "{stats:?}");
        assert!(stats.dirs > 1_000, "{stats:?}");
        let system32 = format!("{drive}:\\windows\\system32").to_lowercase();
        let folders = index.search(&Query::parse("system32"), 10_000);
        assert!(folders.hits.iter().any(|&id| {
            index.kind(id) == Some(Kind::Dir) && index.path(id).unwrap().to_lowercase() == system32
        }));
    }

    /// The real thing: read the file table of the drive Windows is
    /// installed on, and compare sizes with what Windows itself reports.
    /// Needs an administrator, so it steps aside when it is not one.
    #[cfg(windows)]
    #[test]
    fn reads_the_system_drives_file_table() {
        use std::fs;
        use std::path::PathBuf;

        let drive = system_drive();
        let table = match read_file_table(drive) {
            Ok(table) => table,
            Err(VolumeError::NeedsAdmin) => {
                eprintln!("skipped: reading the file table needs an administrator");
                return;
            }
            Err(other) => panic!("{other}"),
        };
        let (index, stats) = build_index(&format!("{drive}:\\"), &table.records).unwrap();
        assert_looks_like_windows(&index, &stats, drive);

        // Files that have been on the drive since Windows was installed,
        // so they are certainly in the table on disk.
        let windows = PathBuf::from(format!("{drive}:\\Windows"));
        let known = [
            windows.join("explorer.exe"),
            windows.join("System32").join("kernel32.dll"),
            windows.join("System32").join("ntdll.dll"),
            windows
                .join("System32")
                .join("drivers")
                .join("etc")
                .join("services"),
            windows.join("win.ini"),
        ];
        let mut compared = 0;
        for path in known {
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            let name = path.file_name().unwrap().to_string_lossy().to_lowercase();
            let wanted = plain(&path);
            let hits = index.search(&Query::parse(&name), usize::MAX);
            let found = hits
                .hits
                .iter()
                .find(|&&id| index.path(id).unwrap().to_lowercase() == wanted)
                .unwrap_or_else(|| panic!("{wanted} is missing from the index"));
            assert_eq!(index.size(*found), Some(metadata.len()), "{wanted}");
            // The time read off the drive is the one Windows reports.
            let changed = metadata
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            assert_eq!(
                index.modified(*found).map(u64::from),
                Some(changed),
                "{wanted}"
            );
            compared += 1;
        }
        assert!(compared >= 3, "only {compared} known files exist here");
    }

    /// The other way in: ask Windows for the records. This goes through
    /// the file system, so a file made a moment ago is already there.
    #[cfg(windows)]
    #[test]
    fn asks_windows_for_the_system_drives_records() {
        use std::fs;

        let drive = system_drive();

        // A fresh file with a name nothing else has.
        let marker_name = format!("seekvo-ntfs-test-{}.marker", std::process::id());
        let marker = std::env::temp_dir().join(&marker_name);
        fs::write(&marker, "x").unwrap();
        let marker_path = plain(&fs::canonicalize(&marker).unwrap());
        let on_system_drive = marker_path.starts_with(drive.to_ascii_lowercase());

        let outcome = read_records(drive);
        let _ = fs::remove_file(&marker);
        let records = match outcome {
            Ok(records) => records,
            Err(VolumeError::NeedsAdmin) => {
                eprintln!("skipped: reading the file table needs an administrator");
                return;
            }
            Err(other) => panic!("{other}"),
        };
        let (index, stats) = build_index(&format!("{drive}:\\"), &records).unwrap();
        assert_looks_like_windows(&index, &stats, drive);

        if on_system_drive {
            let found = index.search(&Query::parse(&marker_name), 10);
            assert_eq!(found.total, 1);
            assert_eq!(
                index.path(found.hits[0]).unwrap().to_lowercase(),
                marker_path
            );
        }
    }

    /// Everything the index has under `folder`, as (path below the folder,
    /// size), sorted. `folder` is a plain lowercase path.
    #[cfg(windows)]
    fn inside(live: &mut LiveDrive, folder: &str) -> Vec<(String, u64)> {
        use crate::query::{Query, Scope};
        let marker = folder.rsplit('\\').next().unwrap();
        let index = live.index().unwrap();
        let query = Query::parse(marker).with_scope(Scope::Path);
        let prefix = format!("{folder}\\");
        let mut found: Vec<(String, u64)> = index
            .search(&query, 1000)
            .hits
            .iter()
            .filter_map(|&id| {
                let path = index.path(id)?.to_lowercase();
                let below = path.strip_prefix(&prefix)?.to_owned();
                Some((below, index.size(id)?))
            })
            .collect();
        found.sort();
        found
    }

    /// Poll until the index shows exactly `expected` under `folder`. The
    /// journal is written as part of each change, so the first poll is
    /// normally enough; the retries allow for a slow machine.
    #[cfg(windows)]
    fn expect_inside(live: &mut LiveDrive, folder: &str, expected: &[(&str, u64)]) {
        let expected: Vec<(String, u64)> = expected
            .iter()
            .map(|&(path, size)| (path.to_owned(), size))
            .collect();
        let mut found = Vec::new();
        for _ in 0..50 {
            live.poll().unwrap();
            found = inside(live, folder);
            if found == expected {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(found, expected);
    }

    /// A new, empty folder on `drive` for a test to change files in.
    #[cfg(windows)]
    fn scratch_folder(drive: char, name: &str) -> std::path::PathBuf {
        use std::fs;
        // The temporary folder is usually on the system drive; if not, a
        // folder everyone can write to on that drive will do.
        let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
        let base = if plain(&temp).starts_with(&format!("{drive}:").to_lowercase()) {
            temp
        } else {
            fs::canonicalize(format!("{drive}:\\Users\\Public")).unwrap()
        };
        let folder = base.join(format!("{name}-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    /// Stop watching, change files, start again from the saved index: the
    /// changes made in between have to be there, without reading the drive.
    #[test]
    #[cfg(windows)]
    fn picks_up_what_changed_while_nothing_was_watching() {
        use std::fs;

        let drive = system_drive();
        let folder = scratch_folder(drive, "seekvo-saved-test");
        let shown = plain(&folder);
        let saved = folder.with_extension("saved");
        fs::write(folder.join("stays.txt"), b"1").unwrap();
        fs::write(folder.join("goes.txt"), b"22").unwrap();

        let (mut live, start) = match LiveDrive::open_saved(drive, &saved) {
            Ok(opened) => opened,
            Err(VolumeError::NeedsAdmin) => {
                eprintln!("skipped: needs an administrator");
                let _ = fs::remove_dir_all(&folder);
                return;
            }
            Err(other) => panic!("{other}"),
        };
        assert_eq!(
            start,
            Start::ReadInFull("nothing has been saved yet".to_owned())
        );
        expect_inside(&mut live, &shown, &[("goes.txt", 2), ("stays.txt", 1)]);
        live.save(&saved).unwrap();
        drop(live);

        // Nothing is watching now.
        fs::remove_file(folder.join("goes.txt")).unwrap();
        fs::rename(folder.join("stays.txt"), folder.join("kept.txt")).unwrap();
        fs::write(folder.join("arrived.bin"), vec![1u8; 5_000]).unwrap();

        let (mut live, start) = LiveDrive::open_saved(drive, &saved).unwrap();
        assert_eq!(start, Start::FromSaved);
        // Up to date as soon as it opens, before any poll.
        assert_eq!(
            inside(&mut live, &shown),
            [
                ("arrived.bin".to_owned(), 5_000),
                ("kept.txt".to_owned(), 1)
            ]
        );
        // And it keeps following from there.
        fs::remove_file(folder.join("arrived.bin")).unwrap();
        expect_inside(&mut live, &shown, &[("kept.txt", 1)]);
        drop(live);

        // A saved file that has been damaged is not trusted: the drive is
        // read again instead.
        let mut bytes = fs::read(&saved).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xFF;
        fs::write(&saved, bytes).unwrap();
        let (mut live, start) = LiveDrive::open_saved(drive, &saved).unwrap();
        assert_eq!(
            start,
            Start::ReadInFull("the saved index is damaged".to_owned())
        );
        expect_inside(&mut live, &shown, &[("kept.txt", 1)]);

        fs::remove_file(&saved).unwrap();
        fs::remove_dir_all(&folder).unwrap();
    }

    /// The real thing: change files on the machine's own system drive and
    /// check the index follows. Needs an administrator, like the test above.
    #[test]
    #[cfg(windows)]
    fn follows_changes_on_the_system_drive() {
        use std::fs;
        use std::io::Write;

        let drive = system_drive();
        let folder = scratch_folder(drive, "seekvo-live-test");
        let shown = plain(&folder);

        // Written a moment before the drive is read, so the table on disk
        // may not have it yet. Catching up from the journal has to find it.
        fs::write(folder.join("early.txt"), b"12345").unwrap();

        let mut live = match LiveDrive::open(drive) {
            Ok(live) => live,
            Err(VolumeError::NeedsAdmin) => {
                eprintln!("skipped: needs an administrator");
                let _ = fs::remove_dir_all(&folder);
                return;
            }
            Err(other) => panic!("{other}"),
        };
        expect_inside(&mut live, &shown, &[("early.txt", 5)]);

        // A new file, big enough that its data is stored outside its record.
        fs::write(folder.join("report.bin"), vec![7u8; 3_000]).unwrap();
        expect_inside(
            &mut live,
            &shown,
            &[("early.txt", 5), ("report.bin", 3_000)],
        );

        // It grows.
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(folder.join("report.bin"))
            .unwrap();
        file.write_all(&vec![7u8; 67_000]).unwrap();
        drop(file);
        expect_inside(
            &mut live,
            &shown,
            &[("early.txt", 5), ("report.bin", 70_000)],
        );

        // It is renamed.
        fs::rename(folder.join("report.bin"), folder.join("final.bin")).unwrap();
        expect_inside(
            &mut live,
            &shown,
            &[("early.txt", 5), ("final.bin", 70_000)],
        );

        // It moves into a new folder.
        fs::create_dir(folder.join("archive")).unwrap();
        fs::rename(
            folder.join("final.bin"),
            folder.join("archive").join("final.bin"),
        )
        .unwrap();
        expect_inside(
            &mut live,
            &shown,
            &[
                ("archive", 0),
                ("archive\\final.bin", 70_000),
                ("early.txt", 5),
            ],
        );

        // The folder is renamed; the file inside follows.
        fs::rename(folder.join("archive"), folder.join("2026")).unwrap();
        expect_inside(
            &mut live,
            &shown,
            &[("2026", 0), ("2026\\final.bin", 70_000), ("early.txt", 5)],
        );

        // It gets a second name.
        fs::hard_link(
            folder.join("2026").join("final.bin"),
            folder.join("also.bin"),
        )
        .unwrap();
        expect_inside(
            &mut live,
            &shown,
            &[
                ("2026", 0),
                ("2026\\final.bin", 70_000),
                ("also.bin", 70_000),
                ("early.txt", 5),
            ],
        );

        // Everything but the first file is deleted.
        fs::remove_file(folder.join("also.bin")).unwrap();
        fs::remove_dir_all(folder.join("2026")).unwrap();
        expect_inside(&mut live, &shown, &[("early.txt", 5)]);

        fs::remove_dir_all(&folder).unwrap();
        expect_inside(&mut live, &shown, &[]);
    }

    #[test]
    fn following_a_drive_says_so_when_not_on_windows() {
        assert!(matches!(
            LiveDrive::open('1'),
            Err(VolumeError::BadDrive('1'))
        ));
        #[cfg(not(windows))]
        {
            assert!(matches!(
                LiveDrive::open('C'),
                Err(VolumeError::Unsupported)
            ));
            assert!(matches!(
                LiveDrive::open_saved('C', Path::new("saved")),
                Err(VolumeError::Unsupported)
            ));
        }
        assert!(matches!(
            LiveDrive::open_saved('?', Path::new("saved")),
            Err(VolumeError::BadDrive('?'))
        ));
    }
}
