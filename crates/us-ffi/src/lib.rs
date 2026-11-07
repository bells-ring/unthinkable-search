//! C ABI of unthinkable-search: `unthinkable_search.dll`.
//! Header: `crates/us-ffi/include/unthinkable_search.h`; guide: docs/INTEGRATION.md.
//!
//! Model: `us_open` starts the index service (load/build + live updates) in a
//! background thread. `us_search` returns an immutable result set bound to the
//! index snapshot it ran on: accessors stay valid and consistent while live
//! updates publish newer snapshots. All functions are thread-safe; handles
//! must not be used after `us_close` / `us_results_free`.
//!
//! Strings are UTF-8. Text getters copy into a caller buffer and return the
//! full length in bytes (without NUL); pass `cap = 0` to query the length.
//!
//! Panics never cross the boundary: build with `--profile release-dll`
//! (panic = "unwind") and every entry point catches them (→ error return,
//! message in `us_last_error`).

#![allow(clippy::missing_safety_doc)]

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use us::service::{self, Event, LiveMode, Service, State};
use us_core::sort::SortKey;
use us_core::Index;

#[global_allocator]
static ALLOC: us_core::alloc::CachingAlloc = us_core::alloc::CachingAlloc;

pub const US_API_VERSION: u32 = 1;
pub const US_OPEN_LIVE: u32 = 1;

pub type UsCallback = extern "C" fn(user: *mut c_void, event: c_int);
pub const US_EVENT_INDEX: c_int = 1;
pub const US_EVENT_STATUS: c_int = 2;

pub struct UsEngine {
    svc: Service,
    cb: Arc<Mutex<Option<(UsCallback, usize)>>>,
}

pub struct UsResults {
    ix: Arc<Index>,
    ids: Vec<u32>,
    ms: f64,
}

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_error(e: impl Into<String>) {
    LAST_ERROR.with(|l| *l.borrow_mut() = e.into());
}

fn guard<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into());
            set_error(format!("internal error: {msg}"));
            fallback
        }
    }
}

/// Copy `s` into `buf` (NUL-terminated, truncated to `cap - 1` bytes on a
/// UTF-8 boundary). Returns the full length of `s`.
unsafe fn put_str(s: &str, buf: *mut c_char, cap: usize) -> usize {
    if !buf.is_null() && cap > 0 {
        let mut n = s.len().min(cap - 1);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(s.as_ptr(), buf as *mut u8, n);
            *buf.add(n) = 0;
        }
    }
    s.len()
}

unsafe fn opt_str<'a>(p: *const c_char) -> Result<Option<&'a str>, String> {
    if p.is_null() {
        return Ok(None);
    }
    unsafe { CStr::from_ptr(p) }.to_str().map(Some).map_err(|_| "string argument is not valid UTF-8".to_string())
}

/// Folder containing this DLL.
fn dll_dir() -> PathBuf {
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    unsafe {
        let mut h = std::ptr::null_mut();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if GetModuleHandleExW(flags, dll_dir as *const () as *const u16, &mut h) != 0 {
            let mut buf = vec![0u16; 32768];
            let n = GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) as usize;
            if n > 0 {
                let p = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
                if let Some(d) = p.parent() {
                    return d.to_path_buf();
                }
            }
        }
    }
    PathBuf::from(".")
}

#[unsafe(no_mangle)]
pub extern "C" fn us_api_version() -> u32 {
    US_API_VERSION
}

/// Library version, static NUL-terminated string.
#[unsafe(no_mangle)]
pub extern "C" fn us_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Last error of the calling thread. Returns its length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_last_error(buf: *mut c_char, cap: usize) -> usize {
    LAST_ERROR.with(|l| unsafe { put_str(&l.borrow(), buf, cap) })
}

/// Start the engine. `data_dir`: folder of `index.usdb` (NULL = `<dll dir>\data`).
/// `flags`: `US_OPEN_LIVE` = live updates (in-process when elevated, else the
/// broker `us.exe` next to the DLL, one UAC prompt). Returns NULL on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_open(data_dir: *const c_char, flags: u32) -> *mut UsEngine {
    guard(std::ptr::null_mut(), || {
        let dir = match unsafe { opt_str(data_dir) } {
            Ok(d) => d.map(PathBuf::from).unwrap_or_else(|| dll_dir().join("data")),
            Err(e) => {
                set_error(e);
                return std::ptr::null_mut();
            }
        };
        let opts = service::Options {
            data_dir: dir,
            live: if flags & US_OPEN_LIVE != 0 { LiveMode::Auto } else { LiveMode::Off },
            // Either binary accepts `--broker`; prefer the GUI one (no console).
            broker_exe: [dll_dir().join("us.exe"), dll_dir().join("usc.exe")]
                .into_iter()
                .find(|p| p.is_file())
                .unwrap_or_else(|| dll_dir().join("us.exe")),
        };
        let cb: Arc<Mutex<Option<(UsCallback, usize)>>> = Arc::new(Mutex::new(None));
        let cb2 = cb.clone();
        let svc = Service::start(opts, move |ev| {
            let c = *cb2.lock().unwrap();
            if let Some((f, user)) = c {
                let code = match ev {
                    Event::Index => US_EVENT_INDEX,
                    Event::Status => US_EVENT_STATUS,
                };
                let _ = catch_unwind(|| f(user as *mut c_void, code));
            }
        });
        Box::into_raw(Box::new(UsEngine { svc, cb }))
    })
}

/// Stop the engine, save the index, free the handle. NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_close(e: *mut UsEngine) {
    if !e.is_null() {
        guard((), || {
            let e = unsafe { Box::from_raw(e) };
            *e.cb.lock().unwrap() = None;
            e.svc.stop();
        })
    }
}

/// Called on the engine's thread on every new index snapshot
/// (`US_EVENT_INDEX`) and status change (`US_EVENT_STATUS`). NULL = none.
/// Keep it short; don't call `us_close` from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_set_callback(e: *mut UsEngine, cb: Option<UsCallback>, user: *mut c_void) {
    if let Some(e) = unsafe { e.as_ref() } {
        *e.cb.lock().unwrap() = cb.map(|f| (f, user as usize));
    }
}

/// Wait until an index is available. 1 = ready, 0 = timeout.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_wait_ready(e: *const UsEngine, timeout_ms: u32) -> c_int {
    let Some(e) = (unsafe { e.as_ref() }) else { return 0 };
    guard(0, || e.svc.wait_index(Duration::from_millis(timeout_ms as u64)).is_some() as c_int)
}

/// 0 loading, 1 indexing, 2 ready.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_state(e: *const UsEngine) -> c_int {
    let Some(e) = (unsafe { e.as_ref() }) else { return 0 };
    match e.svc.status().0 {
        State::Loading => 0,
        State::Indexing => 1,
        State::Ready => 2,
    }
}

/// 1 if live updates are active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_is_live(e: *const UsEngine) -> c_int {
    unsafe { e.as_ref() }.is_some_and(|e| e.svc.status().1) as c_int
}

/// Human-readable status (Russian UI text).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_status_text(e: *const UsEngine, buf: *mut c_char, cap: usize) -> usize {
    let Some(e) = (unsafe { e.as_ref() }) else { return 0 };
    unsafe { put_str(&e.svc.status().2, buf, cap) }
}

/// Changes every time live updates publish a new snapshot (0 = none yet).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_generation(e: *const UsEngine) -> u64 {
    unsafe { e.as_ref() }.map_or(0, |e| e.svc.generation())
}

/// Number of indexed files and folders.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_entry_count(e: *const UsEngine) -> u64 {
    unsafe { e.as_ref() }.and_then(|e| e.svc.index()).map_or(0, |ix| ix.len() as u64)
}

/// Run a query (same syntax as the GUI). `sort`: 0 name, 1 path, 2 size,
/// 3 modified, 4 created. Returns NULL if the index is not ready yet or on
/// bad arguments (see `us_last_error`). Free with `us_results_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_search(e: *const UsEngine, query: *const c_char, sort: u32, descending: c_int) -> *mut UsResults {
    guard(std::ptr::null_mut(), || {
        let Some(e) = (unsafe { e.as_ref() }) else {
            set_error("engine is NULL");
            return std::ptr::null_mut();
        };
        let q = match unsafe { opt_str(query) } {
            Ok(q) => q.unwrap_or(""),
            Err(err) => {
                set_error(err);
                return std::ptr::null_mut();
            }
        };
        let Some(key) = SortKey::from_index(sort) else {
            set_error(format!("bad sort {sort}"));
            return std::ptr::null_mut();
        };
        let Some(ix) = e.svc.index() else {
            set_error("index is not ready yet");
            return std::ptr::null_mut();
        };
        let (ids, ms) = us::output::query_sorted(&ix, q, key, descending != 0);
        Box::into_raw(Box::new(UsResults { ix, ids, ms }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_free(r: *mut UsResults) {
    if !r.is_null() {
        drop(unsafe { Box::from_raw(r) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_count(r: *const UsResults) -> u32 {
    unsafe { r.as_ref() }.map_or(0, |r| r.ids.len() as u32)
}

/// Search time in milliseconds (including sorting).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_ms(r: *const UsResults) -> f64 {
    unsafe { r.as_ref() }.map_or(0.0, |r| r.ms)
}

fn item(r: *const UsResults, i: u32) -> Option<(&'static UsResults, u32)> {
    // SAFETY: the caller guarantees `r` is a live handle for the duration of the call.
    let r: &'static UsResults = unsafe { r.as_ref() }?;
    r.ids.get(i as usize).map(|&id| (r, id))
}

/// Full path of item `i`, e.g. `C:\Windows\notepad.exe`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_full_path(r: *const UsResults, i: u32, buf: *mut c_char, cap: usize) -> usize {
    let Some((r, id)) = item(r, i) else { return unsafe { put_str("", buf, cap) } };
    let mut s = String::new();
    r.ix.full_path_into(id, &mut s);
    unsafe { put_str(&s, buf, cap) }
}

/// Name of item `i` (the original name for an item in the Recycle Bin).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_name(r: *const UsResults, i: u32, buf: *mut c_char, cap: usize) -> usize {
    let Some((r, id)) = item(r, i) else { return unsafe { put_str("", buf, cap) } };
    unsafe { put_str(r.ix.display_name(id), buf, cap) }
}

/// Containing folder of item `i` (empty for a volume root).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_dir(r: *const UsResults, i: u32, buf: *mut c_char, cap: usize) -> usize {
    let Some((r, id)) = item(r, i) else { return unsafe { put_str("", buf, cap) } };
    let mut s = String::new();
    r.ix.dir_path_into(id, &mut s);
    unsafe { put_str(&s, buf, cap) }
}

/// Size in bytes; folders: recursive size of all files below.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_size(r: *const UsResults, i: u32) -> u64 {
    item(r, i).map_or(0, |(r, id)| r.ix.total_size(id))
}

/// Last write time, FILETIME (100 ns since 1601-01-01 UTC); 0 = unknown.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_mtime(r: *const UsResults, i: u32) -> u64 {
    item(r, i).map_or(0, |(r, id)| r.ix.mtime[id as usize])
}

/// Creation time, FILETIME; 0 = unknown.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_ctime(r: *const UsResults, i: u32) -> u64 {
    item(r, i).map_or(0, |(r, id)| r.ix.ctime[id as usize])
}

/// Windows file attributes (FILE_ATTRIBUTE_*).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_attrs(r: *const UsResults, i: u32) -> u32 {
    item(r, i).map_or(0, |(r, id)| r.ix.attrs[id as usize])
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_results_is_dir(r: *const UsResults, i: u32) -> c_int {
    item(r, i).is_some_and(|(r, id)| r.ix.is_dir(id)) as c_int
}

/// JSON request → JSON response, same protocol as `usc serve` (commands
/// `ping`, `info`, `search`). Returns a NUL-terminated UTF-8 string that must
/// be released with `us_free_string`; NULL only on invalid arguments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_request(e: *const UsEngine, json: *const c_char) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        let Some(e) = (unsafe { e.as_ref() }) else {
            set_error("engine is NULL");
            return std::ptr::null_mut();
        };
        let req = match unsafe { opt_str(json) } {
            Ok(Some(s)) => s,
            Ok(None) => {
                set_error("json is NULL");
                return std::ptr::null_mut();
            }
            Err(err) => {
                set_error(err);
                return std::ptr::null_mut();
            }
        };
        let resp = us::serve::request(&e.svc, req);
        // JSON never contains raw NULs (they are escaped), so this cannot fail.
        std::ffi::CString::new(resp).map_or(std::ptr::null_mut(), |c| c.into_raw())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn us_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { std::ffi::CString::from_raw(s) });
    }
}
