//! Pure NTFS on-disk structure parsing (no I/O). All code here treats input
//! as UNTRUSTED (a malicious USB stick can contain any bytes): every offset and
//! length is bounds-checked; corrupted records are skipped, never panic.

use crate::scan::{NONE, ScanEntry, VolumeMeta, VolumeScan, ATTR_DIRECTORY};

/// NTFS applies the update-sequence fixup every 512 bytes regardless of the
/// physical sector size.
pub const NTFS_BLOCK: usize = 512;
pub const ROOT_RECORD: usize = 5;
/// Records 0..24 are metafiles / reserved ($MFT, $LogFile, $Extend, ...).
pub const FIRST_USER_RECORD: usize = 24;

pub const AT_STANDARD_INFORMATION: u32 = 0x10;
pub const AT_ATTRIBUTE_LIST: u32 = 0x20;
pub const AT_FILE_NAME: u32 = 0x30;
pub const AT_DATA: u32 = 0x80;
pub const AT_BITMAP: u32 = 0xB0;
pub const AT_END: u32 = 0xFFFF_FFFF;

pub const NS_POSIX: u8 = 0;
pub const NS_WIN32: u8 = 1;
pub const NS_DOS: u8 = 2;
pub const NS_WIN32_DOS: u8 = 3;

#[inline]
pub fn u16le(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
#[inline]
pub fn u32le(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}
#[inline]
pub fn u64le(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

/// Verify and undo the multi-sector fixup in place. Returns false for a record
/// that is not a valid `FILE` record (free/garbage/torn write).
pub fn apply_fixup(rec: &mut [u8]) -> bool {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return false;
    }
    let (Some(usa_off), Some(usa_cnt)) = (u16le(rec, 4), u16le(rec, 6)) else {
        return false;
    };
    let (usa_off, usa_cnt) = (usa_off as usize, usa_cnt as usize);
    if usa_cnt < 2 || usa_off + usa_cnt * 2 > rec.len() || (usa_cnt - 1) * NTFS_BLOCK > rec.len() {
        return false;
    }
    let (a, b) = (rec[usa_off], rec[usa_off + 1]);
    for i in 1..usa_cnt {
        let p = i * NTFS_BLOCK - 2;
        if rec[p] != a || rec[p + 1] != b {
            return false;
        }
        rec[p] = rec[usa_off + 2 * i];
        rec[p + 1] = rec[usa_off + 2 * i + 1];
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecHeader {
    pub seq: u16,
    pub flags: u16,
    /// Base record reference (0 for base records).
    pub base: u64,
    pub attrs_off: usize,
    pub used: usize,
}
pub const REC_IN_USE: u16 = 1;
pub const REC_DIRECTORY: u16 = 2;

pub fn header(rec: &[u8]) -> Option<RecHeader> {
    Some(RecHeader {
        seq: u16le(rec, 0x10)?,
        flags: u16le(rec, 0x16)?,
        base: u64le(rec, 0x20)?,
        attrs_off: u16le(rec, 0x14)? as usize,
        used: (u32le(rec, 0x18)? as usize).min(rec.len()),
    })
}

#[derive(Clone, Copy, Debug)]
pub struct Attr<'a> {
    pub ty: u32,
    pub nonres: bool,
    pub name_len: u8,
    /// The whole attribute (header + value / mapping pairs).
    pub raw: &'a [u8],
}

impl<'a> Attr<'a> {
    pub fn resident_value(&self) -> Option<&'a [u8]> {
        if self.nonres {
            return None;
        }
        let len = u32le(self.raw, 0x10)? as usize;
        let off = u16le(self.raw, 0x14)? as usize;
        self.raw.get(off..off.checked_add(len)?)
    }
    pub fn lowest_vcn(&self) -> Option<u64> {
        if self.nonres { u64le(self.raw, 0x10) } else { Some(0) }
    }
    pub fn highest_vcn(&self) -> Option<u64> {
        if self.nonres { u64le(self.raw, 0x18) } else { Some(0) }
    }
    /// Real (logical) size of the attribute value.
    pub fn data_size(&self) -> Option<u64> {
        if self.nonres { u64le(self.raw, 0x30) } else { u32le(self.raw, 0x10).map(u64::from) }
    }
    pub fn runlist(&self) -> Option<&'a [u8]> {
        if !self.nonres {
            return None;
        }
        let off = u16le(self.raw, 0x20)? as usize;
        self.raw.get(off..)
    }
}

pub struct AttrIter<'a> {
    rec: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = Attr<'a>;
    fn next(&mut self) -> Option<Attr<'a>> {
        if self.pos + 16 > self.end {
            return None;
        }
        let ty = u32le(self.rec, self.pos)?;
        if ty == AT_END {
            return None;
        }
        let len = u32le(self.rec, self.pos + 4)? as usize;
        if len < 16 || self.pos + len > self.end {
            self.pos = self.end;
            return None;
        }
        let raw = &self.rec[self.pos..self.pos + len];
        self.pos += len;
        Some(Attr { ty, nonres: raw[8] != 0, name_len: raw[9], raw })
    }
}

pub fn attrs<'a>(rec: &'a [u8], h: &RecHeader) -> AttrIter<'a> {
    AttrIter { rec, pos: h.attrs_off, end: h.used }
}

#[derive(Clone, Copy, Debug)]
pub struct FileName<'a> {
    pub parent: u64,
    pub ns: u8,
    /// UTF-16LE bytes.
    pub name16: &'a [u8],
}

pub fn parse_file_name(v: &[u8]) -> Option<FileName<'_>> {
    let nlen = *v.get(0x40)? as usize;
    let ns = *v.get(0x41)?;
    Some(FileName { parent: u64le(v, 0)?, ns, name16: v.get(0x42..0x42 + 2 * nlen)? })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// Absolute LCN, or -1 for a sparse run.
    pub lcn: i64,
    pub len: u64,
}

/// Decode NTFS mapping pairs. Returns None on malformed input.
pub fn decode_runlist(rl: &[u8]) -> Option<Vec<Run>> {
    let mut runs = Vec::new();
    let mut p = 0usize;
    let mut lcn: i64 = 0;
    loop {
        let h = *rl.get(p)?;
        if h == 0 {
            break;
        }
        let ls = (h & 0x0F) as usize;
        let os = (h >> 4) as usize;
        if ls == 0 || ls > 8 || os > 8 {
            return None;
        }
        p += 1;
        let lb = rl.get(p..p + ls)?;
        let mut len = 0u64;
        for (i, &b) in lb.iter().enumerate() {
            len |= (b as u64) << (8 * i);
        }
        p += ls;
        if os == 0 {
            runs.push(Run { lcn: -1, len });
        } else {
            let ob = rl.get(p..p + os)?;
            let mut off = 0i64;
            for (i, &b) in ob.iter().enumerate() {
                off |= (b as i64) << (8 * i);
            }
            // Sign-extend.
            let shift = 64 - 8 * os as u32;
            off = (off << shift) >> shift;
            lcn = lcn.checked_add(off)?;
            if lcn < 0 {
                return None;
            }
            runs.push(Run { lcn, len });
            p += os;
        }
    }
    Some(runs)
}

#[inline]
fn push_utf16(arena: &mut Vec<u8>, b: &[u8]) {
    // ASCII fast path.
    if b.chunks_exact(2).all(|c| c[1] == 0 && c[0] < 0x80 && c[0] != 0) {
        arena.extend(b.chunks_exact(2).map(|c| c[0]));
        return;
    }
    let mut buf = [0u8; 4];
    for r in char::decode_utf16(b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]))) {
        let ch = match r {
            Ok('\0') | Err(_) => '\u{FFFD}',
            Ok(c) => c,
        };
        arena.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Bulk MFT scan: per-chunk parsing (parallel) + assembly into a VolumeScan.

pub const R_IN_USE: u8 = 1;
pub const R_DIR: u8 = 2;
pub const R_HAS_SIZE: u8 = 4;

#[derive(Clone, Copy, Debug, Default)]
pub struct MftRec {
    pub seq: u16,
    pub flags: u8,
    pub attrs: u32,
    pub mtime: u64,
    pub ctime: u64,
    pub size: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct MftName {
    /// Base record number that owns this name.
    pub rec: u32,
    pub parent: u32,
    pub parent_seq: u16,
    pub ns: u8,
    pub off: u32,
    pub len: u32,
}

#[derive(Default)]
pub struct MftChunk {
    pub first: u64,
    pub recs: Vec<MftRec>,
    pub names: Vec<MftName>,
    pub arena: Vec<u8>,
    /// Sizes found in extension records: (base record, size).
    pub ext_sizes: Vec<(u32, u64)>,
}

const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Parse `buf` (a run of consecutive MFT records starting at record `first`).
/// `buf` is modified in place (fixups).
pub fn parse_chunk(buf: &mut [u8], first: u64, rsize: usize) -> MftChunk {
    let count = buf.len() / rsize;
    let mut out = MftChunk {
        first,
        recs: Vec::with_capacity(count),
        names: Vec::with_capacity(count),
        arena: Vec::with_capacity(count * 16),
        ext_sizes: Vec::new(),
    };
    for (i, rec) in buf.chunks_exact_mut(rsize).enumerate() {
        let recno = first + i as u64;
        let mut r = MftRec::default();
        if recno <= u32::MAX as u64 && apply_fixup(rec) {
            if let Some(h) = header(rec) {
                if h.flags & REC_IN_USE != 0 {
                    parse_record(rec, &h, recno as u32, &mut r, &mut out);
                }
            }
        }
        out.recs.push(r);
    }
    out
}

fn parse_record(rec: &[u8], h: &RecHeader, recno: u32, r: &mut MftRec, out: &mut MftChunk) {
    let base = h.base & REF_MASK;
    let is_ext = base != 0;
    if is_ext && base > u32::MAX as u64 {
        return;
    }
    let owner = if is_ext { base as u32 } else { recno };
    let mut got_size = false;
    for a in attrs(rec, h) {
        match a.ty {
            AT_STANDARD_INFORMATION if !is_ext => {
                if let Some(v) = a.resident_value() {
                    r.ctime = u64le(v, 0x00).unwrap_or(0);
                    r.mtime = u64le(v, 0x08).unwrap_or(0);
                    r.attrs = u32le(v, 0x20).unwrap_or(0);
                }
            }
            AT_FILE_NAME => {
                let Some(fname) = a.resident_value().and_then(parse_file_name) else { continue };
                let parent = fname.parent & REF_MASK;
                if parent > u32::MAX as u64 || fname.name16.is_empty() {
                    continue;
                }
                let off = out.arena.len();
                push_utf16(&mut out.arena, fname.name16);
                out.names.push(MftName {
                    rec: owner,
                    parent: parent as u32,
                    parent_seq: (fname.parent >> 48) as u16,
                    ns: fname.ns,
                    off: off as u32,
                    len: (out.arena.len() - off) as u32,
                });
            }
            AT_DATA if a.name_len == 0 && !got_size && a.lowest_vcn() == Some(0) => {
                if let Some(sz) = a.data_size() {
                    got_size = true;
                    if is_ext {
                        out.ext_sizes.push((owner, sz));
                    } else {
                        r.size = sz;
                    }
                }
            }
            _ => {}
        }
    }
    if !is_ext {
        r.seq = h.seq;
        r.flags = R_IN_USE
            | if h.flags & REC_DIRECTORY != 0 { R_DIR } else { 0 }
            | if got_size { R_HAS_SIZE } else { 0 };
    }
}

/// Parse ONE base record fetched on demand (USN update path).
/// `Some(None)` = record not in use / extension record (→ treat as deleted),
/// `None` = not a FILE record at all (garbage).
/// `fixed_up` = the buffer already had its update sequence applied
/// (FSCTL_GET_NTFS_FILE_RECORD may return records either way; we try both).
pub fn record_state(rec: &mut [u8], recno: u32) -> Option<Option<crate::live::RecState>> {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return None;
    }
    let _ = apply_fixup(rec); // if it fails the record was already fixed up
    let h = header(rec)?;
    if h.flags & REC_IN_USE == 0 || h.base & REF_MASK != 0 {
        return Some(None);
    }
    let mut r = MftRec::default();
    let mut chunk = MftChunk::default();
    parse_record(rec, &h, recno, &mut r, &mut chunk);
    let is_dir = r.flags & R_DIR != 0;
    let name_of = |n: &MftName| String::from_utf8_lossy(&chunk.arena[n.off as usize..(n.off + n.len) as usize]).into_owned();
    let mut names: Vec<(u32, String)> = Vec::new();
    if is_dir {
        if let Some(best) = chunk.names.iter().filter(|n| n.ns != NS_DOS).min_by_key(|n| ns_rank(n.ns)) {
            names.push((best.parent, name_of(best)));
        }
    } else {
        for n in chunk.names.iter().filter(|n| n.ns != NS_DOS) {
            let pair = (n.parent, name_of(n));
            if !names.contains(&pair) {
                names.push(pair);
            }
        }
    }
    Some(Some(crate::live::RecState { is_dir, attrs: r.attrs, mtime: r.mtime, ctime: r.ctime, size: r.size, names }))
}

#[inline]
fn ns_rank(ns: u8) -> u8 {
    match ns {
        NS_WIN32 | NS_WIN32_DOS => 0,
        NS_POSIX => 1,
        _ => 2,
    }
}

/// Merge parsed chunks of one volume into a `VolumeScan`. Drops metafiles,
/// orphans, stale parent references and DOS 8.3 aliases.
pub fn assemble(root: &str, meta: VolumeMeta, total: usize, chunks: Vec<MftChunk>) -> VolumeScan {
    let mut recs = vec![MftRec::default(); total];
    let mut names: Vec<MftName> = Vec::with_capacity(chunks.iter().map(|c| c.names.len()).sum());
    let mut arena: Vec<u8> = Vec::with_capacity(chunks.iter().map(|c| c.arena.len()).sum());
    let mut ext_sizes = Vec::new();
    for c in chunks {
        let f = c.first as usize;
        if f < total {
            let m = c.recs.len().min(total - f);
            recs[f..f + m].copy_from_slice(&c.recs[..m]);
        }
        let shift = arena.len() as u32;
        arena.extend_from_slice(&c.arena);
        names.extend(c.names.into_iter().map(|mut n| {
            n.off += shift;
            n
        }));
        ext_sizes.extend(c.ext_sizes);
    }
    for (b, sz) in ext_sizes {
        if let Some(r) = recs.get_mut(b as usize) {
            if r.flags & R_HAS_SIZE == 0 {
                r.size = sz;
                r.flags |= R_HAS_SIZE;
            }
        }
    }

    // Best name per directory record.
    let mut dir_best = vec![NONE; total];
    for (ni, n) in names.iter().enumerate() {
        let r = n.rec as usize;
        if r >= total || recs[r].flags & R_DIR == 0 || n.ns == NS_DOS {
            continue;
        }
        let cur = dir_best[r];
        if cur == NONE || ns_rank(n.ns) < ns_rank(names[cur as usize].ns) {
            dir_best[r] = ni as u32;
        }
    }

    // Directory validity: reachable from the root through live, seq-matching parents.
    const UNK: u8 = 0;
    const OK: u8 = 1;
    const BAD: u8 = 2;
    const VISIT: u8 = 3;
    let mut state = vec![UNK; total];
    let root_ok = total > ROOT_RECORD && recs[ROOT_RECORD].flags & (R_IN_USE | R_DIR) == (R_IN_USE | R_DIR);
    if !root_ok {
        return VolumeScan { meta, ..VolumeScan::new(root) };
    }
    state[ROOT_RECORD] = OK;
    let parent_ok = |n: &MftName, recs: &[MftRec]| -> bool {
        let p = n.parent as usize;
        p < total && (n.parent_seq == 0 || recs[p].seq == n.parent_seq)
    };
    let mut stack = Vec::new();
    for r0 in 0..total {
        if state[r0] != UNK || recs[r0].flags & R_DIR == 0 {
            continue;
        }
        stack.clear();
        let mut cur = r0;
        let verdict = loop {
            match state[cur] {
                OK => break OK,
                BAD | VISIT => break BAD,
                _ => {}
            }
            let nm = dir_best[cur];
            if cur < FIRST_USER_RECORD
                || recs[cur].flags & (R_IN_USE | R_DIR) != (R_IN_USE | R_DIR)
                || nm == NONE
                || !parent_ok(&names[nm as usize], &recs)
            {
                state[cur] = BAD;
                break BAD;
            }
            state[cur] = VISIT;
            stack.push(cur);
            cur = names[nm as usize].parent as usize;
        };
        for &s in &stack {
            state[s] = verdict;
        }
    }

    // Emit entries: root, then directories, then files. `parent` temporarily
    // holds the parent RECORD number; remapped to local ids afterwards.
    let mut scan = VolumeScan::new(root);
    scan.meta = meta;
    scan.entries[0].mtime = recs[ROOT_RECORD].mtime;
    scan.entries[0].ctime = recs[ROOT_RECORD].ctime;
    scan.entries[0].attrs = recs[ROOT_RECORD].attrs | ATTR_DIRECTORY;
    scan.entries[0].rec = ROOT_RECORD as u32;
    let mut dir_local = vec![NONE; total];
    dir_local[ROOT_RECORD] = 0;
    scan.entries.reserve(names.len());
    scan.arena.reserve(arena.len());
    let push = |scan: &mut VolumeScan, n: &MftName, r: &MftRec, is_dir: bool| -> u32 {
        let id = scan.entries.len() as u32;
        let off = scan.arena.len() as u32;
        scan.arena.extend_from_slice(&arena[n.off as usize..(n.off + n.len) as usize]);
        scan.entries.push(ScanEntry {
            parent: n.parent,
            name_off: off,
            name_len: n.len,
            attrs: r.attrs | if is_dir { ATTR_DIRECTORY } else { 0 },
            size: if is_dir { 0 } else { r.size },
            mtime: r.mtime,
            ctime: r.ctime,
            rec: n.rec,
        });
        id
    };
    for r in 0..total {
        if r != ROOT_RECORD && state[r] == OK {
            let n = names[dir_best[r] as usize];
            dir_local[r] = push(&mut scan, &n, &recs[r], true);
        }
    }
    for n in &names {
        let r = n.rec as usize;
        if r >= total || r < FIRST_USER_RECORD || n.ns == NS_DOS {
            continue;
        }
        let rr = recs[r];
        if rr.flags & (R_IN_USE | R_DIR) != R_IN_USE {
            continue;
        }
        let p = n.parent as usize;
        if p >= total || state[p] != OK || !parent_ok(n, &recs) {
            continue;
        }
        push(&mut scan, n, &rr, false);
    }
    for e in scan.entries.iter_mut().skip(1) {
        e.parent = dir_local[e.parent as usize];
        debug_assert!(e.parent != NONE);
    }
    scan
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Build a synthetic 1 KiB MFT record (with a valid fixup) for tests/fuzzing seeds.
    pub fn make_record(seq: u16, flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // usa offset
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // usn + 2 blocks
        r[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut p = 0x38;
        for a in attrs {
            r[p..p + a.len()].copy_from_slice(a);
            p += a.len();
        }
        r[p..p + 4].copy_from_slice(&AT_END.to_le_bytes());
        p += 8;
        r[0x18..0x1C].copy_from_slice(&(p as u32).to_le_bytes());
        // Fixup: USN = 0xABCD; save original tails into the array.
        let usn = [0xCD, 0xAB];
        r[0x30..0x32].copy_from_slice(&usn);
        for i in 1..3 {
            let t = i * 512 - 2;
            let (a, b) = (r[t], r[t + 1]);
            r[0x30 + 2 * i] = a;
            r[0x30 + 2 * i + 1] = b;
            r[t..t + 2].copy_from_slice(&usn);
        }
        r
    }

    pub fn resident(ty: u32, name_len: u8, value: &[u8]) -> Vec<u8> {
        let len = (0x18 + value.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = name_len;
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a[0x18..0x18 + value.len()].copy_from_slice(value);
        a
    }

    pub fn std_info(mtime: u64, attrs: u32) -> Vec<u8> {
        let mut v = vec![0u8; 0x48];
        v[8..16].copy_from_slice(&mtime.to_le_bytes());
        v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
        resident(AT_STANDARD_INFORMATION, 0, &v)
    }

    pub fn file_name(parent: u64, parent_seq: u16, ns: u8, name: &str) -> Vec<u8> {
        let u: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42 + 2 * u.len()];
        v[0..8].copy_from_slice(&(parent | (parent_seq as u64) << 48).to_le_bytes());
        v[0x40] = u.len() as u8;
        v[0x41] = ns;
        for (i, c) in u.iter().enumerate() {
            v[0x42 + 2 * i..0x44 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        resident(AT_FILE_NAME, 0, &v)
    }

    pub fn nonres_data(size: u64) -> Vec<u8> {
        let mut a = vec![0u8; 0x48];
        a[0..4].copy_from_slice(&AT_DATA.to_le_bytes());
        a[4..8].copy_from_slice(&0x48u32.to_le_bytes());
        a[8] = 1;
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        a[0x40] = 0x11; // 1-byte len, 1-byte offset
        a[0x41] = 1;
        a[0x42] = 0x10;
        a
    }

    #[test]
    fn runlist() {
        // len 0x18 @ +0x5634, then len 0x10 @ -0x10 (relative), sparse 0x20.
        let rl = [0x21, 0x18, 0x34, 0x56, 0x11, 0x10, 0xF0, 0x01, 0x20, 0x00];
        let r = decode_runlist(&rl).unwrap();
        assert_eq!(
            r,
            vec![
                Run { lcn: 0x5634, len: 0x18 },
                Run { lcn: 0x5624, len: 0x10 },
                Run { lcn: -1, len: 0x20 }
            ]
        );
        assert!(decode_runlist(&[0x21, 0x18]).is_none());
    }

    #[test]
    fn fixup_detects_torn_record() {
        let mut r = make_record(1, REC_IN_USE, 0, &[]);
        let mut bad = r.clone();
        assert!(apply_fixup(&mut r));
        bad[1022] ^= 1;
        assert!(!apply_fixup(&mut bad));
    }

    fn volume(records: Vec<(usize, Vec<u8>)>, total: usize) -> VolumeScan {
        let mut buf = vec![0u8; total * 1024];
        for (n, r) in records {
            buf[n * 1024..(n + 1) * 1024].copy_from_slice(&r);
        }
        // Parse in two chunks to exercise merging.
        let half = (total / 2) * 1024;
        let (a, b) = buf.split_at_mut(half);
        let c1 = parse_chunk(a, 0, 1024);
        let c2 = parse_chunk(b, (total / 2) as u64, 1024);
        assemble("T:", VolumeMeta::default(), total, vec![c2, c1])
    }

    #[test]
    fn assemble_tree() {
        let dir = REC_IN_USE | REC_DIRECTORY;
        let recs = vec![
            (5, make_record(5, dir, 0, &[std_info(1, 0x16), file_name(5, 5, NS_WIN32_DOS, ".")])),
            // $Extend-like metafile dir (record 11) and a child: must be hidden.
            (11, make_record(11, dir, 0, &[std_info(1, 0), file_name(5, 5, NS_WIN32_DOS, "$Extend")])),
            (30, make_record(1, dir, 0, &[std_info(2, 0), file_name(5, 5, NS_DOS, "PROGRA~1"), file_name(5, 5, NS_WIN32, "Program Files")])),
            (31, make_record(3, REC_IN_USE, 0, &[std_info(3, 0x20), file_name(30, 1, NS_WIN32, "app.exe"), nonres_data(123456)])),
            // Hard link: second name in another dir.
            (32, make_record(1, REC_IN_USE, 0, &[std_info(4, 0), file_name(30, 1, NS_POSIX, "a.txt"), file_name(5, 5, NS_POSIX, "b.txt")])),
            (33, make_record(1, REC_IN_USE, 0, &[file_name(11, 11, NS_WIN32, "$UsnJrnl")])),
            // Stale parent sequence → orphan.
            (34, make_record(1, REC_IN_USE, 0, &[file_name(30, 9, NS_WIN32, "stale.txt")])),
            // Name and size living in an extension record of 35.
            (35, make_record(1, REC_IN_USE, 0, &[std_info(5, 0)])),
            (36, make_record(1, REC_IN_USE, 35, &[file_name(5, 5, NS_WIN32, "big.iso"), nonres_data(1 << 40)])),
            // Not in use.
            (37, make_record(1, 0, 0, &[file_name(5, 5, NS_WIN32, "deleted.txt")])),
        ];
        let s = volume(recs, 40);
        let mut got: Vec<(String, u32, u64)> = (1..s.entries.len())
            .map(|i| {
                let e = s.entries[i];
                (s.name(i).to_string(), e.parent, e.size)
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("Program Files".into(), 0, 0),
                ("a.txt".into(), 1, 0),
                ("app.exe".into(), 1, 123456),
                ("b.txt".into(), 0, 0),
                ("big.iso".into(), 0, 1 << 40),
            ]
        );
        assert_eq!(s.entries[0].attrs & ATTR_DIRECTORY, ATTR_DIRECTORY);
    }

    #[test]
    fn garbage_never_panics() {
        // Cheap deterministic "fuzz": random bytes with a FILE signature.
        let mut x: u64 = 0x9E3779B97F4A7C15;
        for _ in 0..2000 {
            let mut r = vec![0u8; 1024];
            for b in r.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            r[0..4].copy_from_slice(b"FILE");
            r[6] = 3;
            r[7] = 0;
            r[4] = 0x30;
            r[5] = 0;
            let t1 = [r[0x30], r[0x31]];
            r[510..512].copy_from_slice(&t1);
            r[1022..1024].copy_from_slice(&t1);
            let _ = parse_chunk(&mut r, 100, 1024);
        }
    }
}
