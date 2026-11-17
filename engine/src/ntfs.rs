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
//! Known gaps: file sizes are not part of these records, so every size is
//! zero for now, and a file with several hard links is listed under one of
//! its names only.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::io;

use crate::index::{BuildError, EntryId, Index, IndexBuilder, Kind};

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
    name_len: u32,
    is_dir: bool,
}

/// File records as they come off a drive: unordered, each naming its parent
/// by record number. All names share one buffer.
#[derive(Debug, Default)]
pub struct VolumeRecords {
    names: String,
    rows: Vec<Row>,
}

impl VolumeRecords {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Add one record. `file_reference` and `parent_reference` are the
    /// 64-bit NTFS file references.
    pub fn push(&mut self, file_reference: u64, parent_reference: u64, name: &str, is_dir: bool) {
        let name_start = self.names.len();
        self.names.push_str(name);
        self.push_row(file_reference, parent_reference, name_start, is_dir);
    }

    fn push_utf16(
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
        self.push_row(file_reference, parent_reference, name_start, is_dir);
    }

    fn push_row(
        &mut self,
        file_reference: u64,
        parent_reference: u64,
        name_start: usize,
        is_dir: bool,
    ) {
        // An NTFS name is at most 255 UTF-16 units, far below u32.
        let name_len = (self.names.len() - name_start) as u32;
        self.rows.push(Row {
            record: file_reference & RECORD_NUMBER_MASK,
            parent: parent_reference & RECORD_NUMBER_MASK,
            name_start,
            name_len,
            is_dir,
        });
    }

    fn name(&self, row: &Row) -> &str {
        &self.names[row.name_start..row.name_start + row.name_len as usize]
    }
}

fn bytes_at<const N: usize>(buffer: &[u8], at: usize) -> Option<[u8; N]> {
    buffer.get(at..at.checked_add(N)?)?.try_into().ok()
}

/// Read one output buffer of `FSCTL_ENUM_USN_DATA` into `records`.
///
/// The buffer starts with the file reference to continue from, followed by
/// version 2 change-journal records. Returns that reference, or `None` if
/// the buffer is too short to hold one. Damaged or unknown records are
/// skipped; parsing never reads outside the buffer.
pub fn parse_usn_buffer(buffer: &[u8], records: &mut VolumeRecords) -> Option<u64> {
    let next = u64::from_le_bytes(bytes_at(buffer, 0)?);
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
        let (Some(file), Some(parent), Some(attributes), Some(name_len), Some(name_offset)) = (
            bytes_at(record, 8).map(u64::from_le_bytes),
            bytes_at(record, 16).map(u64::from_le_bytes),
            bytes_at(record, 52).map(u32::from_le_bytes),
            bytes_at(record, 56).map(u16::from_le_bytes),
            bytes_at(record, 58).map(u16::from_le_bytes),
        ) else {
            continue;
        };
        let name_offset = name_offset as usize;
        let Some(name) = record.get(name_offset..name_offset + name_len as usize) else {
            continue;
        };
        let units = name
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
        let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        records.push_utf16(file, parent, units, is_dir);
    }
    Some(next)
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
        .filter(|&i| rows[i].record >= FIRST_ORDINARY_RECORD)
        .collect();
    let mut stats = VolumeStats {
        system: rows.len() - order.len(),
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
                let id = builder.add(Some(folder_id), records.name(row), Kind::Dir, 0)?;
                stats.dirs += 1;
                queue.push_back((row.record, id));
            } else {
                builder.add(Some(folder_id), records.name(row), Kind::File, 0)?;
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
    Build(BuildError),
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
            VolumeError::Build(error) => write!(f, "cannot build the index: {error}"),
        }
    }
}

impl std::error::Error for VolumeError {}

/// Read every file record from an NTFS drive, given its letter.
///
/// This is the part that talks to the disk. Needs Windows and an
/// administrator. Everywhere else it returns [`VolumeError::Unsupported`].
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

/// Index every file on an NTFS drive, given its letter: [`read_records`]
/// followed by [`build_index`].
pub fn read_volume(drive: char) -> Result<(Index, VolumeStats), VolumeError> {
    let records = read_records(drive)?;
    let root = format!("{}:\\", drive.to_ascii_uppercase());
    build_index(&root, &records).map_err(VolumeError::Build)
}

// `seekvo_check_windows` lets the Windows code be type-checked on other
// systems with `RUSTFLAGS="--cfg seekvo_check_windows" cargo check`.
#[cfg(any(windows, seekvo_check_windows))]
#[cfg_attr(not(windows), allow(dead_code))]
mod windows {
    use std::ffi::c_void;
    use std::io;
    use std::ptr;

    use super::{VolumeError, VolumeRecords, parse_usn_buffer};

    type Handle = *mut c_void;

    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const OPEN_EXISTING: u32 = 3;
    const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00B3;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_HANDLE_EOF: i32 = 38;
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

        fn CloseHandle(object: Handle) -> i32;
    }

    /// An open drive that closes itself.
    struct Volume(Handle);

    impl Drop for Volume {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateFileW and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// A NUL-terminated UTF-16 copy of `text`, as Windows wants its strings.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn file_system_of(root: &str) -> io::Result<String> {
        let root = wide(root);
        let mut name = [0u16; 64];
        // SAFETY: `root` is NUL-terminated, `name` is writable for the
        // length passed, and the other outputs are allowed to be null.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
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
        Ok(String::from_utf16_lossy(&name[..length]))
    }

    pub(super) fn read_records(drive: char) -> Result<VolumeRecords, VolumeError> {
        let root = format!("{drive}:\\");
        let file_system = file_system_of(&root).map_err(VolumeError::Io)?;
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
        let volume = Volume(handle);

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

    /// The real thing: read the drive Windows is installed on. Needs an
    /// administrator, so it steps aside when it is not one.
    #[cfg(windows)]
    #[test]
    fn reads_the_system_drive() {
        use crate::index::Kind;
        use std::fs;

        let drive = std::env::var("SystemDrive")
            .ok()
            .and_then(|value| value.chars().next())
            .unwrap_or('C');

        // A fresh file with a name nothing else has.
        let marker_name = format!("seekvo-ntfs-test-{}.marker", std::process::id());
        let marker = std::env::temp_dir().join(&marker_name);
        fs::write(&marker, "x").unwrap();
        let marker_path = fs::canonicalize(&marker).unwrap();
        let on_system_drive = marker_path
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_uppercase()
            .starts_with(drive.to_ascii_uppercase());

        let outcome = read_volume(drive);
        let _ = fs::remove_file(&marker);
        let (index, stats) = match outcome {
            Ok(found) => found,
            Err(VolumeError::NeedsAdmin) => {
                eprintln!("skipped: reading the file table needs an administrator");
                return;
            }
            Err(other) => panic!("{other}"),
        };

        assert!(stats.files > 10_000, "{stats:?}");
        assert!(stats.dirs > 1_000, "{stats:?}");

        let system32 = format!("{drive}:\\windows\\system32").to_lowercase();
        let folders = index.search(&Query::parse("system32"), 10_000);
        assert!(folders.hits.iter().any(|&id| {
            index.kind(id) == Some(Kind::Dir) && index.path(id).unwrap().to_lowercase() == system32
        }));

        if on_system_drive {
            let wanted = marker_path
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_lowercase();
            let found = index.search(&Query::parse(&marker_name), 10);
            assert_eq!(found.total, 1);
            assert_eq!(index.path(found.hits[0]).unwrap().to_lowercase(), wanted);
        }
    }
}
