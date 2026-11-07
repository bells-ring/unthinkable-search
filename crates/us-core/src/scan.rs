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
    /// Recycle Bin metadata: local id of a `$I…` file → original item name.
    /// `build()` keeps the rows that really are in a Recycle Bin.
    pub recycle: Vec<(u32, String)>,
}

/// Longest `$I…` file: header + 32 767 UTF-16 units.
pub const RECYCLE_INFO_MAX: usize = 28 + 2 * 32_767;

/// Windows moves a deleted item to `<vol>\$Recycle.Bin\<SID>\$R<id><.ext>` and
/// keeps its original path in the sibling `$I<id><.ext>`.
#[inline]
pub fn is_recycle_info_name(name: &str) -> bool {
    name.starts_with("$I") && name.len() >= 8 && name.as_bytes().get(8).is_none_or(|&b| b == b'.')
}

/// Original file name from the contents of a `$I…` file (UNTRUSTED input).
/// Layouts: 8 version, 8 size, 8 deletion time, then v1 = fixed UTF-16 path
/// buffer, v2 = u32 length in units (with the NUL) + path.
pub fn recycle_original_name(data: &[u8]) -> Option<String> {
    let version = u64::from_le_bytes(data.get(..8)?.try_into().ok()?);
    let path = match version {
        1 => data.get(24..)?,
        2 => {
            let units = u32::from_le_bytes(data.get(24..28)?.try_into().ok()?) as usize;
            data.get(28..28 + units.checked_mul(2)?)?
        }
        _ => return None,
    };
    let units: Vec<u16> = path.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    let path = String::from_utf16(&units).ok()?;
    let name = path.rsplit('\\').next()?;
    valid_original_name(name).then(|| name.to_string())
}

/// A name we are willing to show instead of the on-disk one: a legal file name
/// without characters that could disguise it (bidi overrides turn `exe.pdf`
/// around) or break the NUL-separated search blob.
fn valid_original_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.len() <= 1024
        && !s.chars().any(|c| {
            (c as u32) < 32 || "\\/:*?\"<>|".contains(c) || matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
}

fn same_extension(a: &str, b: &str) -> bool {
    let ext = |s: &str| s.rfind('.').map(|k| s[k + 1..].to_lowercase());
    ext(a) == ext(b)
}

/// Recycle Bin pairs of one scan, resolved by `VolumeScan::recycle_links`.
#[derive(Default)]
pub(crate) struct RecycleLinks<'a> {
    /// `$R…` item (local id) → original name.
    pub items: std::collections::HashMap<u32, &'a str>,
    /// `$I…` rows that really live in a Recycle Bin; kept for later rebuilds,
    /// because the matching `$R…` may arrive in a later batch of live updates.
    pub infos: Vec<(u32, &'a str)>,
}

impl VolumeScan {
    /// Match `$I…` rows with their `$R…` items inside `<root>\$Recycle.Bin\<any>`.
    /// A file keeps its real extension in the bin, so an original name with
    /// another extension is rejected: a crafted volume must not be able to show
    /// `invoice.pdf` for an item that runs as `$R123456.exe`.
    pub(crate) fn recycle_links(&self) -> RecycleLinks<'_> {
        let mut out = RecycleLinks::default();
        if self.recycle.is_empty() {
            return out;
        }
        let bin_dir = |sid: u32| -> bool {
            let Some(sid) = self.entries.get(sid as usize) else { return false };
            let Some(bin) = self.entries.get(sid.parent as usize) else { return false };
            bin.parent == 0 && self.name(sid.parent as usize).eq_ignore_ascii_case("$recycle.bin")
        };
        let mut by_key = std::collections::HashMap::new();
        for (id, orig) in &self.recycle {
            let Some(e) = self.entries.get(*id as usize) else { continue };
            let name = self.name(*id as usize);
            if !e.is_dir() && is_recycle_info_name(name) && valid_original_name(orig) && bin_dir(e.parent) {
                out.infos.push((*id, orig));
                by_key.insert((e.parent, &name[2..]), orig.as_str());
            }
        }
        if by_key.is_empty() {
            return out;
        }
        for (i, e) in self.entries.iter().enumerate() {
            if !self.arena[e.name_off as usize..].starts_with(b"$R") || e.name_len < 2 {
                continue;
            }
            let name = self.name(i);
            if let Some(&orig) = by_key.get(&(e.parent, &name[2..]))
                && (e.is_dir() || same_extension(name, orig))
            {
                out.items.insert(i as u32, orig);
            }
        }
        out
    }
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
