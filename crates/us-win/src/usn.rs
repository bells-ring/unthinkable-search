//! USN change journal: which MFT records changed since a cursor, and their new
//! state fetched straight from the MFT (`FSCTL_GET_NTFS_FILE_RECORD`).
//!
//! We never open the changed files themselves (except Recycle Bin metadata, see
//! `fetch`): no OneDrive hydration, no last-access updates, no antivirus scans
//! triggered, exact size/mtime.

use crate::mft::{open_volume, query_usn, volume_data};
use crate::util::{last_err, Handle};
use std::io;
use us_core::live::RecState;
use us_core::ntfs;
use us_core::scan::{is_recycle_info_name, recycle_original_name, RECYCLE_INFO_MAX};
use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FileIdType, OpenFileById, ReadFile, FILE_FLAG_OPEN_NO_RECALL, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_FILE_RECORD, FSCTL_READ_USN_JOURNAL};

const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;
const ERROR_HANDLE_EOF: i32 = 38;
const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

#[derive(Debug)]
pub enum JournalError {
    /// The cursor can no longer be honoured (journal recreated / wrapped /
    /// disabled): the volume must be rescanned from the MFT.
    Reset(String),
    Io(io::Error),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Reset(s) => write!(f, "journal reset: {s}"),
            JournalError::Io(e) => write!(f, "{e}"),
        }
    }
}

pub struct Journal {
    pub letter: char,
    h: Handle,
    pub journal_id: u64,
    pub next_usn: i64,
    rsize: usize,
    buf: Vec<u8>,
    rec_buf: Vec<u8>,
}

impl Journal {
    /// Resume at `next_usn` of journal `journal_id` (both stored in the index).
    pub fn open(letter: char, journal_id: u64, next_usn: i64) -> Result<Journal, JournalError> {
        let h = open_volume(letter).map_err(JournalError::Io)?;
        let Some((cur_id, _)) = query_usn(&h) else {
            return Err(JournalError::Reset("journal not active".into()));
        };
        if cur_id != journal_id {
            return Err(JournalError::Reset(format!("journal id changed {journal_id:016X} -> {cur_id:016X}")));
        }
        let rsize = volume_data(&h).map_err(JournalError::Io)?.BytesPerFileRecordSegment as usize;
        Ok(Journal {
            letter,
            h,
            journal_id,
            next_usn,
            rsize,
            buf: vec![0u8; 256 * 1024],
            rec_buf: vec![0u8; 16 + rsize],
        })
    }

    /// Drain all records currently in the journal after the cursor. Appends the
    /// base record numbers touched (may contain duplicates). Never blocks.
    pub fn poll(&mut self, out: &mut Vec<u32>) -> Result<usize, JournalError> {
        let mut records = 0usize;
        loop {
            // READ_USN_JOURNAL_DATA_V0: StartUsn, ReasonMask, ReturnOnlyOnClose,
            // Timeout, BytesToWaitFor (0 = return immediately), UsnJournalID.
            let mut inp = [0u8; 40];
            inp[0..8].copy_from_slice(&self.next_usn.to_le_bytes());
            inp[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
            inp[32..40].copy_from_slice(&self.journal_id.to_le_bytes());
            let mut got = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    self.h.0,
                    FSCTL_READ_USN_JOURNAL,
                    inp.as_ptr() as *const _,
                    inp.len() as u32,
                    self.buf.as_mut_ptr() as *mut _,
                    self.buf.len() as u32,
                    &mut got,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let e = last_err();
                return match e.raw_os_error() {
                    Some(ERROR_HANDLE_EOF) => Ok(records),
                    Some(ERROR_JOURNAL_ENTRY_DELETED) => Err(JournalError::Reset("cursor fell off the journal".into())),
                    Some(ERROR_JOURNAL_NOT_ACTIVE) | Some(ERROR_JOURNAL_DELETE_IN_PROGRESS) => {
                        Err(JournalError::Reset("journal deleted/disabled".into()))
                    }
                    _ => Err(JournalError::Io(e)),
                };
            }
            let n = (got as usize).min(self.buf.len());
            if n < 8 {
                return Ok(records);
            }
            let next = i64::from_le_bytes(self.buf[0..8].try_into().unwrap());
            let mut p = 8usize;
            while p + 16 <= n {
                let len = u32::from_le_bytes(self.buf[p..p + 4].try_into().unwrap()) as usize;
                if len < 16 || p + len > n {
                    break;
                }
                let major = u16::from_le_bytes([self.buf[p + 4], self.buf[p + 5]]);
                // V2: 64-bit FRN at +8. V3: 128-bit id at +8 (NTFS: low 64 bits = FRN).
                if major == 2 || major == 3 {
                    let frn = u64::from_le_bytes(self.buf[p + 8..p + 16].try_into().unwrap()) & REF_MASK;
                    if frn <= u32::MAX as u64 {
                        out.push(frn as u32);
                    }
                    records += 1;
                }
                p += len;
            }
            let advanced = next != self.next_usn;
            self.next_usn = next;
            if n <= 8 || !advanced {
                return Ok(records);
            }
        }
    }

    /// Current state of base record `rec` (None = not in use → deleted).
    pub fn fetch(&mut self, rec: u32) -> io::Result<Option<RecState>> {
        let inp = (rec as i64).to_le_bytes();
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.h.0,
                FSCTL_GET_NTFS_FILE_RECORD,
                inp.as_ptr() as *const _,
                8,
                self.rec_buf.as_mut_ptr() as *mut _,
                self.rec_buf.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_err());
        }
        // NTFS_FILE_RECORD_OUTPUT_BUFFER: i64 FileReferenceNumber, u32 FileRecordLength, bytes...
        let got = got as usize;
        if got < 12 {
            return Ok(None);
        }
        // The FSCTL returns the nearest LOWER in-use record if `rec` is free.
        let frn = u64::from_le_bytes(self.rec_buf[0..8].try_into().unwrap()) & REF_MASK;
        if frn != rec as u64 {
            return Ok(None);
        }
        let len = (u32::from_le_bytes(self.rec_buf[8..12].try_into().unwrap()) as usize).min(got - 12).min(self.rsize);
        let r = &mut self.rec_buf[12..12 + len];
        let seq = ntfs::u16le(r, 0x10).unwrap_or(0);
        let mut st = ntfs::record_state(r, rec).flatten();
        // A `$I…` file just written by Explorer may not be flushed to its record
        // yet (and a very long path is not stored there at all). It is the only
        // file we ever open here, through the cache, by id.
        if let Some(s) = &mut st
            && !s.is_dir
            && s.orig_name.is_none()
            && s.names.iter().any(|(_, n)| is_recycle_info_name(n))
        {
            s.orig_name = self.read_by_id(rec, seq).and_then(|data| recycle_original_name(&data));
        }
        Ok(st)
    }

    fn read_by_id(&self, rec: u32, seq: u16) -> Option<Vec<u8>> {
        let id = FILE_ID_DESCRIPTOR {
            dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: ((seq as i64) << 48) | rec as i64 },
        };
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        let h = unsafe { OpenFileById(self.h.0, &id, GENERIC_READ, share, std::ptr::null(), FILE_FLAG_OPEN_NO_RECALL) };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return None;
        }
        let h = Handle(h);
        let mut buf = vec![0u8; RECYCLE_INFO_MAX];
        let mut got = 0u32;
        let ok = unsafe { ReadFile(h.0, buf.as_mut_ptr(), buf.len() as u32, &mut got, std::ptr::null_mut()) };
        if ok == 0 {
            return None;
        }
        buf.truncate(got as usize);
        Some(buf)
    }
}
