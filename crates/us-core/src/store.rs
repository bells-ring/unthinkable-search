//! On-disk index format `index.usdb` (little-endian, x86-64 only).
//!
//! ```text
//! magic  "USDB" u32 version  [v3+: u32 fold_version]  u64 entry_count  u32 volume_count
//! per volume: u32 root_len, root bytes, u8 kind, u32 serial, u64 journal_id, i64 next_usn, u32 root_entry
//! columns (each: u64 byte_len, bytes): names, names_off, folded, folded_off, parent, size, mtime, ctime, attrs, rec, vol
//! u64 FNV-1a checksum of all preceding bytes
//! ```
//! Columns are read straight into their final `Vec<T>` (no intermediate copy).
//! Loading validates every invariant the search code relies on, so a corrupted
//! or tampered file is rejected instead of causing out-of-bounds access.

use crate::index::{Index, VolumeInfo};
use crate::scan::{NONE, ScanKind, VolumeMeta};
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 4] = b"USDB";
const VERSION: u32 = 4;
/// Oldest file version we can still read (v3 had no creation times → full reindex).
const MIN_VERSION: u32 = 4;

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

struct Hasher(u64);
impl Hasher {
    fn new() -> Self {
        Hasher(0xcbf29ce484222325)
    }
    // Word-at-a-time FNV-style mix: fast enough to not dominate load time.
    fn update(&mut self, b: &[u8]) {
        let mut h = self.0;
        let mut it = b.chunks_exact(8);
        for c in &mut it {
            h = (h ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x100000001b3);
        }
        for &x in it.remainder() {
            h = (h ^ x as u64).wrapping_mul(0x100000001b3);
        }
        self.0 = h;
    }
}

struct HW<W: Write> {
    w: W,
    h: Hasher,
}
impl<W: Write> HW<W> {
    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.h.update(b);
        self.w.write_all(b)
    }
    fn col<T: Copy>(&mut self, v: &[T]) -> io::Result<()> {
        let bytes = as_bytes(v);
        self.put(&(bytes.len() as u64).to_le_bytes())?;
        self.put(bytes)
    }
}

fn as_bytes<T: Copy>(v: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

pub fn save(ix: &Index, path: &Path) -> io::Result<()> {
    // Per-process temp name: GUI, `usc serve` and the DLL may share one data dir.
    let tmp = path.with_extension(format!("usdb.{}.tmp", std::process::id()));
    {
        let f = File::create(&tmp)?;
        let mut w = HW { w: BufWriter::with_capacity(1 << 20, f), h: Hasher::new() };
        w.put(MAGIC)?;
        w.put(&VERSION.to_le_bytes())?;
        w.put(&crate::fold::FOLD_VERSION.to_le_bytes())?;
        w.put(&(ix.len() as u64).to_le_bytes())?;
        w.put(&(ix.volumes.len() as u32).to_le_bytes())?;
        for v in &ix.volumes {
            w.put(&(v.root.len() as u32).to_le_bytes())?;
            w.put(v.root.as_bytes())?;
            w.put(&[v.meta.kind as u8])?;
            w.put(&v.meta.serial.to_le_bytes())?;
            w.put(&v.meta.journal_id.to_le_bytes())?;
            w.put(&v.meta.next_usn.to_le_bytes())?;
            w.put(&v.root_entry.to_le_bytes())?;
        }
        w.col(&ix.names)?;
        w.col(&ix.names_off)?;
        w.col(&ix.folded)?;
        w.col(&ix.folded_off)?;
        w.col(&ix.parent)?;
        w.col(&ix.size)?;
        w.col(&ix.mtime)?;
        w.col(&ix.ctime)?;
        w.col(&ix.attrs)?;
        w.col(&ix.rec)?;
        w.col(&ix.vol)?;
        let sum = w.h.0;
        w.w.write_all(&sum.to_le_bytes())?;
        let f = w.w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

struct HR<R: Read> {
    r: R,
    h: Hasher,
}
impl<R: Read> HR<R> {
    fn get(&mut self, b: &mut [u8]) -> io::Result<()> {
        self.r.read_exact(b)?;
        self.h.update(b);
        Ok(())
    }
    fn u32(&mut self) -> io::Result<u32> {
        let mut b = [0; 4];
        self.get(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn u64(&mut self) -> io::Result<u64> {
        let mut b = [0; 8];
        self.get(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    fn col<T: Copy + Default>(&mut self, max_len: usize) -> io::Result<Vec<T>> {
        let bytes = self.u64()? as usize;
        let sz = std::mem::size_of::<T>();
        if bytes % sz != 0 || bytes / sz > max_len {
            return Err(bad("column size"));
        }
        let mut v = vec![T::default(); bytes / sz];
        let b = unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, bytes) };
        self.get(b)?;
        Ok(v)
    }
}

pub fn load(path: &Path) -> io::Result<Index> {
    let f = File::open(path)?;
    let file_len = f.metadata()?.len() as usize;
    let mut r = HR { r: io::BufReader::with_capacity(1 << 20, f), h: Hasher::new() };
    let mut m = [0u8; 4];
    r.get(&mut m)?;
    let ver = r.u32()?;
    if &m != MAGIC || !(MIN_VERSION..=VERSION).contains(&ver) {
        return Err(bad("not a usdb file of a supported version"));
    }
    let fold_version = if ver >= 3 { r.u32()? } else { 1 };
    let n = r.u64()? as usize;
    if n >= NONE as usize || n > file_len {
        return Err(bad("entry count"));
    }
    let nv = r.u32()? as usize;
    if nv > 255 {
        return Err(bad("volume count"));
    }
    let mut volumes = Vec::with_capacity(nv);
    for _ in 0..nv {
        let l = r.u32()? as usize;
        if l > 1024 {
            return Err(bad("root len"));
        }
        let mut b = vec![0; l];
        r.get(&mut b)?;
        let root = String::from_utf8(b).map_err(|_| bad("root utf8"))?;
        let mut k = [0u8; 1];
        r.get(&mut k)?;
        let kind = if k[0] == 1 { ScanKind::Mft } else { ScanKind::Walk };
        let serial = r.u32()?;
        let journal_id = r.u64()?;
        let next_usn = r.u64()? as i64;
        let root_entry = r.u32()?;
        volumes.push(VolumeInfo { root, meta: VolumeMeta { kind, serial, journal_id, next_usn }, root_entry });
    }
    let ix = Index {
        volumes,
        names: r.col(file_len)?,
        names_off: r.col(n + 1)?,
        folded: r.col(file_len)?,
        folded_off: r.col(n + 1)?,
        parent: r.col(n)?,
        size: r.col(n)?,
        mtime: r.col(n)?,
        ctime: r.col(n)?,
        attrs: r.col(n)?,
        rec: r.col(n)?,
        vol: r.col(n)?,
        dir_topo: Default::default(),
        dir_sizes: Default::default(),
    };
    let want = r.h.0;
    let mut s = [0u8; 8];
    r.r.read_exact(&mut s)?;
    if u64::from_le_bytes(s) != want {
        return Err(bad("checksum mismatch"));
    }
    validate(&ix)?;
    if fold_version != crate::fold::FOLD_VERSION {
        // Folding rules changed: re-fold + re-sort from the stored names. No
        // disk rescan, USN cursors preserved.
        return Ok(crate::live::apply(&ix, &[], &[]));
    }
    Ok(ix)
}

/// Check every invariant `Index` accessors and the search code rely on.
pub fn validate(ix: &Index) -> io::Result<()> {
    use rayon::prelude::*;
    let n = ix.parent.len();
    if ix.size.len() != n || ix.mtime.len() != n || ix.ctime.len() != n || ix.attrs.len() != n || ix.rec.len() != n || ix.vol.len() != n {
        return Err(bad("column lengths"));
    }
    if ix.names_off.len() != n + 1 || ix.folded_off.len() != n + 1 {
        return Err(bad("offset lengths"));
    }
    if ix.names_off[0] != 0 || ix.names_off[n] as usize != ix.names.len() {
        return Err(bad("names_off bounds"));
    }
    if ix.folded.first() != Some(&0) || ix.folded_off[0] != 1 || ix.folded_off[n] as usize != ix.folded.len() {
        return Err(bad("folded_off bounds"));
    }
    let ok = (0..n).into_par_iter().all(|i| {
        let (a, b) = (ix.names_off[i] as usize, ix.names_off[i + 1] as usize);
        let (fa, fb) = (ix.folded_off[i] as usize, ix.folded_off[i + 1] as usize);
        let p = ix.parent[i];
        a <= b
            && fa < fb
            && ix.folded[fb - 1] == 0
            && !ix.folded[fa..fb - 1].contains(&0)
            && std::str::from_utf8(&ix.names[a..b]).is_ok()
            && (p == NONE || (p as usize) < n)
    });
    if !ok {
        return Err(bad("entry invariants"));
    }
    let nv = ix.volumes.len();
    if ix.vol.par_iter().any(|&v| v as usize >= nv) {
        return Err(bad("volume column"));
    }
    if ix.volumes.iter().any(|v| v.root_entry as usize >= n) {
        return Err(bad("root entry"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_tamper() {
        let ix = crate::index::tests::sample();
        // Stay inside the project tree (portability rule: no %TEMP%).
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("usdb_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.usdb");
        save(&ix, &p).unwrap();
        let ld = load(&p).unwrap();
        assert_eq!(ld.names, ix.names);
        assert_eq!(ld.folded_off, ix.folded_off);
        assert_eq!(ld.parent, ix.parent);
        assert_eq!(ld.volumes, ix.volumes);
        let mut b = std::fs::read(&p).unwrap();
        let k = b.len() / 2;
        b[k] ^= 0x55;
        std::fs::write(&p, &b).unwrap();
        assert!(load(&p).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
