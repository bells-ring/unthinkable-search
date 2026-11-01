//! Volume discovery.

use crate::util::wide;
use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};

pub const DRIVE_REMOVABLE: u32 = 2;
pub const DRIVE_FIXED: u32 = 3;
pub const DRIVE_REMOTE: u32 = 4;

#[derive(Clone, Debug)]
pub struct Volume {
    pub letter: char,
    pub fs: String,
    pub serial: u32,
    pub drive_type: u32,
}

impl Volume {
    pub fn root(&self) -> String {
        format!("{}:", self.letter)
    }
    pub fn is_ntfs(&self) -> bool {
        self.fs.eq_ignore_ascii_case("NTFS")
    }
}

/// Local fixed and removable volumes that have a mounted file system.
pub fn list() -> Vec<Volume> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = wide(&format!("{letter}:\\"));
        let dt = unsafe { GetDriveTypeW(root.as_ptr()) };
        if dt != DRIVE_FIXED && dt != DRIVE_REMOVABLE {
            continue;
        }
        let mut fs = [0u16; 64];
        let mut serial = 0u32;
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            continue; // e.g. empty card reader
        }
        let n = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
        out.push(Volume { letter, fs: String::from_utf16_lossy(&fs[..n]), serial, drive_type: dt });
    }
    out
}
