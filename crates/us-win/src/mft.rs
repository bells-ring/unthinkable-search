//! Fast indexer: raw `$MFT` read straight from the volume (requires admin).
//!
//! 1. `FSCTL_GET_NTFS_VOLUME_DATA` → cluster/record sizes, MFT location and valid length.
//! 2. Read MFT record 0 (the `$MFT` file itself) → `$DATA` runlist (extents) and `$BITMAP`
//!    (which records are in use). Fallback for heavily fragmented MFTs (attribute list):
//!    `FSCTL_GET_RETRIEVAL_POINTERS` on `X:\$MFT`.
//! 3. Split extents into large aligned blocks, skip blocks with no records in use,
//!    N worker threads each with their own unbuffered volume handle read + parse
//!    blocks (claimed in disk order → near-sequential stream even on HDD).
//! 4. `us_core::ntfs::assemble` → `VolumeScan`.
//!
//! The USN journal position is captured BEFORE reading so that changes made
//! during the scan are replayed later (no lost updates).

use crate::util::{last_err, wide, AlignedBuf, Handle};
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;
use us_core::ntfs::{self, MftChunk, AT_ATTRIBUTE_LIST, AT_BITMAP, AT_DATA};
use us_core::scan::{ScanKind, VolumeMeta, VolumeScan};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_GET_RETRIEVAL_POINTERS, FSCTL_QUERY_USN_JOURNAL, NTFS_VOLUME_DATA_BUFFER,
};

const GENERIC_READ: u32 = 0x8000_0000;
const ERROR_MORE_DATA: i32 = 234;

#[derive(Clone, Debug, Default)]
pub struct MftStats {
    pub mft_bytes: u64,
    pub bytes_read: u64,
    pub records: u64,
    pub blocks: usize,
    pub skipped_blocks: usize,
    pub threads: usize,
    pub read_parse_ms: f64,
    /// Summed over worker threads.
    pub read_ms_sum: f64,
    pub parse_ms_sum: f64,
    pub assemble_ms: f64,
}

pub fn open_volume(letter: char) -> io::Result<Handle> {
    let p = wide(&format!(r"\\.\{letter}:"));
    Handle::checked(unsafe {
        CreateFileW(
            p.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING,
            std::ptr::null_mut(),
        )
    })
}

fn ioctl_out(h: &Handle, code: u32, input: &[u8], out: &mut [u8]) -> io::Result<usize> {
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            code,
            input.as_ptr() as *const _,
            input.len() as u32,
            out.as_mut_ptr() as *mut _,
            out.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { Err(last_err()) } else { Ok(ret as usize) }
}

/// Positional read on a synchronous handle. `buf`, `off`, `buf.len()` must be sector aligned.
fn read_at(h: &Handle, off: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        let o = off + done as u64;
        ov.Anonymous.Anonymous.Offset = o as u32;
        ov.Anonymous.Anonymous.OffsetHigh = (o >> 32) as u32;
        let want = (buf.len() - done).min(1 << 30) as u32;
        let mut got = 0u32;
        let ok = unsafe { ReadFile(h.0, buf[done..].as_mut_ptr(), want, &mut got, &mut ov) };
        if ok == 0 {
            return Err(last_err());
        }
        if got == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        done += got as usize;
    }
    Ok(())
}

pub fn volume_data(h: &Handle) -> io::Result<NTFS_VOLUME_DATA_BUFFER> {
    let mut vd: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
    let out = unsafe {
        std::slice::from_raw_parts_mut(&mut vd as *mut _ as *mut u8, std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>())
    };
    ioctl_out(h, FSCTL_GET_NTFS_VOLUME_DATA, &[], out)?;
    Ok(vd)
}

/// (journal id, next usn) or None if the journal is not active.
pub fn query_usn(h: &Handle) -> Option<(u64, i64)> {
    let mut out = [0u8; 80];
    let n = ioctl_out(h, FSCTL_QUERY_USN_JOURNAL, &[], &mut out).ok()?;
    if n < 24 {
        return None;
    }
    Some((u64::from_le_bytes(out[0..8].try_into().unwrap()), i64::from_le_bytes(out[16..24].try_into().unwrap())))
}

/// (file offset / VCN-based byte offset, disk byte offset, byte length)
type Extent = (u64, u64, u64);

fn extents_from_runs(runs: &[ntfs::Run], cluster: u64) -> Vec<Extent> {
    let mut v = Vec::with_capacity(runs.len());
    let mut vcn = 0u64;
    for r in runs {
        if r.lcn >= 0 {
            v.push((vcn * cluster, r.lcn as u64 * cluster, r.len * cluster));
        }
        vcn += r.len;
    }
    v
}

/// Read a whole non-resident attribute (used for the small `$MFT:$BITMAP`).
fn read_runs(h: &Handle, ext: &[Extent], size: u64, sector: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(size as usize);
    for &(_, disk, len) in ext {
        if out.len() as u64 >= size {
            break;
        }
        let want = len.min(size - out.len() as u64);
        let mut buf = AlignedBuf::new(want.next_multiple_of(sector) as usize);
        let b = buf.as_mut();
        let n = want.next_multiple_of(sector) as usize;
        read_at(h, disk, &mut b[..n])?;
        out.extend_from_slice(&b[..want as usize]);
    }
    Ok(out)
}

fn retrieval_pointers(letter: char, cluster: u64) -> io::Result<Vec<Extent>> {
    let p = wide(&format!(r"\\.\{letter}:\$MFT"));
    let h = Handle::checked(unsafe {
        CreateFileW(
            p.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    })?;
    let mut ext = Vec::new();
    let mut start_vcn: i64 = 0;
    let mut out = vec![0u8; 64 * 1024];
    loop {
        let r = ioctl_out(&h, FSCTL_GET_RETRIEVAL_POINTERS, &start_vcn.to_le_bytes(), &mut out);
        let more = match &r {
            Ok(_) => false,
            Err(e) if e.raw_os_error() == Some(ERROR_MORE_DATA) => true,
            Err(_) => return Err(r.unwrap_err()),
        };
        // RETRIEVAL_POINTERS_BUFFER: u32 count, pad, i64 StartingVcn, then {i64 NextVcn, i64 Lcn}[count]
        let count = u32::from_le_bytes(out[0..4].try_into().unwrap()) as usize;
        let mut vcn = i64::from_le_bytes(out[8..16].try_into().unwrap());
        if count == 0 || 16 + count * 16 > out.len() {
            break;
        }
        for i in 0..count {
            let b = 16 + i * 16;
            let next = i64::from_le_bytes(out[b..b + 8].try_into().unwrap());
            let lcn = i64::from_le_bytes(out[b + 8..b + 16].try_into().unwrap());
            if lcn >= 0 && next > vcn {
                ext.push((vcn as u64 * cluster, lcn as u64 * cluster, (next - vcn) as u64 * cluster));
            }
            vcn = next;
        }
        if !more {
            break;
        }
        start_vcn = vcn;
    }
    Ok(ext)
}

struct Block {
    disk: u64,
    len: usize,
    first_rec: u64,
}

pub struct MftOptions {
    /// 0 = auto by seek penalty. More threads never helped: the MFT read is device-bound.
    pub threads: usize,
    pub block_bytes: usize,
}

impl Default for MftOptions {
    fn default() -> Self {
        MftOptions {
            threads: 0, // auto: 1 on HDD (seek penalty), 2 on SSD — measured best (seek-bound HDD, queue-depth-bound SSD)
            block_bytes: 1 << 20,
        }
    }
}

pub fn scan_mft(letter: char, opt: &MftOptions) -> io::Result<(VolumeScan, MftStats)> {
    let t0 = Instant::now();
    let h = open_volume(letter)?;
    let vd = volume_data(&h)?;
    let cluster = vd.BytesPerCluster as u64;
    let sector = (vd.BytesPerSector as u64).max(512);
    let rsize = vd.BytesPerFileRecordSegment as usize;
    if rsize < 512 || rsize > 65536 || cluster == 0 || !rsize.is_power_of_two() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad NTFS geometry"));
    }
    let valid = vd.MftValidDataLength as u64;
    let total = (valid / rsize as u64) as usize;
    // Journal position first: anything changed during the scan gets replayed.
    let (journal_id, next_usn) = query_usn(&h).unwrap_or((0, 0));

    // Record 0 → $DATA extents + $BITMAP.
    let r0_len = (rsize as u64).next_multiple_of(sector) as usize;
    let mut r0buf = AlignedBuf::new(r0_len);
    read_at(&h, vd.MftStartLcn as u64 * cluster, &mut r0buf.as_mut()[..r0_len])?;
    let rec0 = &mut r0buf.as_mut()[..rsize];
    if !ntfs::apply_fixup(rec0) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "MFT record 0 is corrupt"));
    }
    let hdr = ntfs::header(rec0).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MFT record 0 header"))?;
    let mut data_runs = None;
    let mut has_attr_list = false;
    let mut bitmap: Option<Vec<u8>> = None;
    let mut bitmap_attr = None;
    for a in ntfs::attrs(rec0, &hdr) {
        match a.ty {
            AT_ATTRIBUTE_LIST => has_attr_list = true,
            AT_DATA if a.name_len == 0 && data_runs.is_none() => {
                data_runs = a.runlist().and_then(ntfs::decode_runlist).map(|r| (r, a.highest_vcn().unwrap_or(0)));
            }
            AT_BITMAP if a.name_len == 0 => {
                if let Some(v) = a.resident_value() {
                    bitmap = Some(v.to_vec());
                } else if let (Some(rl), Some(sz)) = (a.runlist().and_then(ntfs::decode_runlist), a.data_size()) {
                    bitmap_attr = Some((rl, sz));
                }
            }
            _ => {}
        }
    }
    let need_clusters = valid.div_ceil(cluster);
    let mut extents = match data_runs {
        Some((runs, hi)) if !has_attr_list || hi + 1 >= need_clusters => extents_from_runs(&runs, cluster),
        _ => retrieval_pointers(letter, cluster)?,
    };
    if let Some((rl, sz)) = bitmap_attr {
        bitmap = read_runs(&h, &extents_from_runs(&rl, cluster), sz, sector).ok();
    }
    // Clamp extents to the valid length.
    extents.retain(|e| e.0 < valid);
    for e in extents.iter_mut() {
        e.2 = e.2.min(valid - e.0);
    }

    // Blocks.
    let in_use = |rec: u64| -> bool {
        match &bitmap {
            Some(b) => b.get((rec / 8) as usize).is_none_or(|&x| x & (1 << (rec % 8)) != 0),
            None => true,
        }
    };
    let bb = (opt.block_bytes as u64).next_multiple_of(rsize as u64).next_multiple_of(sector);
    let mut blocks = Vec::new();
    let mut skipped = 0usize;
    for &(foff, disk, len) in &extents {
        let mut o = 0u64;
        while o < len {
            let l = (len - o).min(bb);
            let l_al = l.next_multiple_of(sector);
            let first = (foff + o) / rsize as u64;
            let n = l / rsize as u64;
            if (first..first + n).any(in_use) {
                blocks.push(Block { disk: disk + o, len: l_al as usize, first_rec: first });
            } else {
                skipped += 1;
            }
            o += l;
        }
    }
    let auto_threads = if seek_penalty(&h) == Some(true) { 1 } else { 2 };
    drop(h);

    // Read + parse in parallel; each worker owns its handle and buffer.
    let next = AtomicUsize::new(0);
    let read_ns = AtomicU64::new(0);
    let parse_ns = AtomicU64::new(0);
    let threads = if opt.threads == 0 { auto_threads } else { opt.threads }.min(blocks.len().max(1));
    let t_rp = Instant::now();
    let results: Vec<io::Result<Vec<MftChunk>>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| -> io::Result<Vec<MftChunk>> {
                    let h = open_volume(letter)?;
                    let mut buf = AlignedBuf::new(bb as usize);
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(b) = blocks.get(i) else { break };
                        let slice = &mut buf.as_mut()[..b.len];
                        let tr = Instant::now();
                        read_at(&h, b.disk, slice)?;
                        read_ns.fetch_add(tr.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        let tp = Instant::now();
                        // Only whole records that belong to the MFT's valid range.
                        let recs = (b.len / rsize).min(total.saturating_sub(b.first_rec as usize));
                        out.push(ntfs::parse_chunk(&mut slice[..recs * rsize], b.first_rec, rsize));
                        parse_ns.fetch_add(tp.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    }
                    Ok(out)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let read_parse_ms = t_rp.elapsed().as_secs_f64() * 1e3;
    let mut chunks = Vec::new();
    for r in results {
        chunks.extend(r?);
    }
    let bytes_read: u64 = blocks.iter().map(|b| b.len as u64).sum();

    let t_as = Instant::now();
    let meta = VolumeMeta { kind: ScanKind::Mft, serial: vd.VolumeSerialNumber as u32, journal_id, next_usn };
    let scan = ntfs::assemble(&format!("{letter}:"), meta, total, chunks);
    let stats = MftStats {
        mft_bytes: valid,
        bytes_read,
        records: total as u64,
        blocks: blocks.len(),
        skipped_blocks: skipped,
        threads,
        read_parse_ms,
        read_ms_sum: read_ns.load(Ordering::Relaxed) as f64 / 1e6,
        parse_ms_sum: parse_ns.load(Ordering::Relaxed) as f64 / 1e6,
        assemble_ms: t_as.elapsed().as_secs_f64() * 1e3,
    };
    let _ = t0;
    Ok((scan, stats))
}

/// IOCTL_STORAGE_QUERY_PROPERTY(StorageDeviceSeekPenaltyProperty): true for rotational disks.
pub fn seek_penalty(h: &Handle) -> Option<bool> {
    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
    // STORAGE_PROPERTY_QUERY { PropertyId = 7 (SeekPenalty), QueryType = 0 (Standard), AdditionalParameters[1] }
    let mut q = [0u8; 12];
    q[0..4].copy_from_slice(&7u32.to_le_bytes());
    let mut out = [0u8; 12]; // DEVICE_SEEK_PENALTY_DESCRIPTOR { Version, Size, IncursSeekPenalty }
    let n = ioctl_out(h, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut out).ok()?;
    (n >= 9).then(|| out[8] != 0)
}
