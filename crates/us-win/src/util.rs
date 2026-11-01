//! Small Win32 helpers: owned handles, wide strings, aligned buffers.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::io;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn last_err() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

pub struct Handle(pub HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Handle {
    pub fn checked(h: HANDLE) -> io::Result<Handle> {
        if h == INVALID_HANDLE_VALUE || h.is_null() { Err(last_err()) } else { Ok(Handle(h)) }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Page-aligned zeroed buffer (required by FILE_FLAG_NO_BUFFERING).
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}
unsafe impl Send for AlignedBuf {}
impl AlignedBuf {
    pub fn new(len: usize) -> AlignedBuf {
        let len = len.max(4096).next_multiple_of(4096);
        let ptr = unsafe { alloc_zeroed(Layout::from_size_align(len, 4096).unwrap()) };
        assert!(!ptr.is_null(), "out of memory");
        AlignedBuf { ptr, len }
    }
    pub fn as_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}
impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, Layout::from_size_align(self.len, 4096).unwrap()) }
    }
}

/// Process-wide hardening & portability settings. Call first thing in main.
pub fn harden_process() {
    use windows_sys::Win32::System::Diagnostics::Debug::{SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX};
    use windows_sys::Win32::System::LibraryLoader::{SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32};
    unsafe {
        // No "insert disk" popups for empty card readers, no WER crash dialog/dumps.
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
        // DLL planting protection: load system DLLs from System32 only.
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }
}
