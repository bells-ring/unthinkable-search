//! Fallback indexer: parallel directory enumeration. Works without admin
//! rights and on any file system (FAT/exFAT/ReFS/network), but is much slower
//! than reading the MFT.
//!
//! `FindExInfoBasic` skips the 8.3 short name, `FIND_FIRST_EX_LARGE_FETCH`
//! uses a bigger kernel buffer (fewer transitions). Reparse-point directories
//! (junctions, symlinks, mount points) are listed but never descended into —
//! avoids loops and double counting. Cloud placeholders are
//! never opened, so no OneDrive hydration is triggered.

use crate::util::wide;
use rayon::prelude::*;
use us_core::scan::{is_recycle_info_name, recycle_original_name, ATTR_DIRECTORY, ATTR_REPARSE_POINT, RECYCLE_INFO_MAX, VolumeScan};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
};

struct Item {
    off: u32,
    len: u32,
    attrs: u32,
    size: u64,
    mtime: u64,
    ctime: u64,
    name16: Vec<u16>, // only for directories we descend into
    sub: Option<Box<Node>>,
    /// Original item name, for a `$I…` file in a Recycle Bin.
    orig: Option<String>,
}

/// `...\$Recycle.Bin\<SID>`
fn is_recycle_sid_dir(path: &[u16]) -> bool {
    String::from_utf16_lossy(path).rsplit('\\').nth(1).is_some_and(|d| d.eq_ignore_ascii_case("$recycle.bin"))
}

/// Metadata files are tiny and never cloud placeholders, so opening them here
/// hydrates nothing. Unreadable (another user's bin) = the `$R…` name stays.
fn read_recycle_info(dir: &[u16], name: &[u16], size: u64) -> Option<String> {
    use std::os::windows::ffi::OsStringExt;
    if size > RECYCLE_INFO_MAX as u64 {
        return None;
    }
    let mut p = dir.to_vec();
    p.push(b'\\' as u16);
    p.extend_from_slice(name);
    let data = std::fs::read(std::ffi::OsString::from_wide(&p)).ok()?;
    recycle_original_name(&data)
}

#[derive(Default)]
struct Node {
    arena: Vec<u8>,
    items: Vec<Item>,
}

fn list(path: &[u16]) -> Node {
    let mut node = Node::default();
    let mut pat = Vec::with_capacity(path.len() + 3);
    pat.extend_from_slice(path);
    pat.extend_from_slice(&[b'\\' as u16, b'*' as u16, 0]);
    let mut fd: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let h = unsafe {
        FindFirstFileExW(
            pat.as_ptr(),
            FindExInfoBasic,
            &mut fd as *mut _ as *mut _,
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return node; // access denied etc.: just an empty directory
    }
    let in_bin = is_recycle_sid_dir(path);
    loop {
        let n = fd.cFileName.iter().position(|&c| c == 0).unwrap_or(fd.cFileName.len());
        let name = &fd.cFileName[..n];
        let dot = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
        if !dot {
            let off = node.arena.len() as u32;
            let s = String::from_utf16_lossy(name);
            node.arena.extend_from_slice(s.as_bytes());
            let attrs = fd.dwFileAttributes;
            let is_dir = attrs & ATTR_DIRECTORY != 0;
            let descend = is_dir && attrs & ATTR_REPARSE_POINT == 0;
            let size = (fd.nFileSizeHigh as u64) << 32 | fd.nFileSizeLow as u64;
            let orig = (in_bin && !is_dir && is_recycle_info_name(&s)).then(|| read_recycle_info(path, name, size)).flatten();
            node.items.push(Item {
                off,
                len: s.len() as u32,
                attrs,
                size: if is_dir { 0 } else { size },
                mtime: (fd.ftLastWriteTime.dwHighDateTime as u64) << 32 | fd.ftLastWriteTime.dwLowDateTime as u64,
                ctime: (fd.ftCreationTime.dwHighDateTime as u64) << 32 | fd.ftCreationTime.dwLowDateTime as u64,
                name16: if descend { name.to_vec() } else { Vec::new() },
                sub: None,
                orig,
            });
        }
        if unsafe { FindNextFileW(h, &mut fd) } == 0 {
            break;
        }
    }
    unsafe { FindClose(h) };

    node.items.par_iter_mut().filter(|it| !it.name16.is_empty()).for_each(|it| {
        let mut p = Vec::with_capacity(path.len() + 1 + it.name16.len());
        p.extend_from_slice(path);
        p.push(b'\\' as u16);
        p.extend_from_slice(&it.name16);
        it.sub = Some(Box::new(list(&p)));
        it.name16 = Vec::new();
    });
    node
}

fn flatten(node: &Node, parent: u32, scan: &mut VolumeScan) {
    for it in &node.items {
        let name = unsafe { std::str::from_utf8_unchecked(&node.arena[it.off as usize..(it.off + it.len) as usize]) };
        let id = scan.push_rec(parent, name, it.attrs, it.size, it.mtime, it.ctime, us_core::scan::NONE);
        if let Some(orig) = &it.orig {
            scan.recycle.push((id, orig.clone()));
        }
        if let Some(sub) = &it.sub {
            flatten(sub, id, scan);
        }
    }
}

/// Enumerate a whole volume, e.g. `walk_volume('C', 16)`.
/// Each volume gets its OWN thread pool: enumeration is I/O-bound (threads
/// mostly block in the kernel), and a slow HDD must not starve an SSD.
pub fn walk_volume(letter: char, threads: usize) -> VolumeScan {
    // `\\?\` prefix: no MAX_PATH limit, no path normalization.
    let mut root = wide(&format!(r"\\?\{letter}:"));
    root.pop(); // drop NUL
    let tree = match rayon::ThreadPoolBuilder::new().num_threads(threads.max(1)).build() {
        Ok(pool) => pool.install(|| list(&root)),
        Err(_) => list(&root),
    };
    let mut scan = VolumeScan::new(&format!("{letter}:"));
    flatten(&tree, 0, &mut scan);
    scan
}
