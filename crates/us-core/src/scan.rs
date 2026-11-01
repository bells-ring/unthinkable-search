//! Raw per-volume scan result. Both the MFT reader and the directory walker
//! produce this format; `index::build` merges any number of scans.

pub const NONE: u32 = u32::MAX;
pub const ATTR_DIRECTORY: u32 = 0x10;
pub const ATTR_REPARSE_POINT: u32 = 0x400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ScanKind {
    #[default]
    Walk = 0,
    Mft = 1,
}

/// Information needed to resume a volume later (USN catch-up).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VolumeMeta {
    pub kind: ScanKind,
    pub serial: u32,
    pub journal_id: u64,
    pub next_usn: i64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanEntry {
    /// Local id of the parent directory inside the same scan (`NONE` for the root).
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u32,
    pub attrs: u32,
    pub size: u64,
    /// FILETIME (100 ns since 1601-01-01 UTC).
    pub mtime: u64,
    /// Creation time, FILETIME (0 = unknown).
    pub ctime: u64,
    /// MFT record number (NONE for walk scans). Used to apply USN updates.
    pub rec: u32,
}

impl ScanEntry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.attrs & ATTR_DIRECTORY != 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct VolumeScan {
    /// Display name of the root, e.g. `C:`.
    pub root: String,
    pub meta: VolumeMeta,
    /// Entry 0 is always the root directory.
    pub entries: Vec<ScanEntry>,
    pub arena: Vec<u8>,
}

impl VolumeScan {
    pub fn new(root: &str) -> Self {
        let mut s = VolumeScan {
            root: root.to_string(),
            ..Default::default()
        };
        s.push(NONE, root, ATTR_DIRECTORY, 0, 0);
        s
    }

    #[inline]
    pub fn push(&mut self, parent: u32, name: &str, attrs: u32, size: u64, mtime: u64) -> u32 {
        self.push_rec(parent, name, attrs, size, mtime, 0, NONE)
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn push_rec(&mut self, parent: u32, name: &str, attrs: u32, size: u64, mtime: u64, ctime: u64, rec: u32) -> u32 {
        let id = self.entries.len() as u32;
        let name_off = self.arena.len() as u32;
        self.arena.extend_from_slice(name.as_bytes());
        self.entries.push(ScanEntry {
            parent,
            name_off,
            name_len: name.len() as u32,
            attrs,
            size,
            mtime,
            ctime,
            rec,
        });
        id
    }

    #[inline]
    pub fn name(&self, i: usize) -> &str {
        let e = &self.entries[i];
        let b = &self.arena[e.name_off as usize..(e.name_off + e.name_len) as usize];
        // Arena is only ever filled from &str.
        unsafe { std::str::from_utf8_unchecked(b) }
    }
}
