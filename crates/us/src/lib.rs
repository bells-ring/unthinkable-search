//! unthinkable-search application layer: CLI (usc.exe), GUI (us.exe),
//! live engine, benchmarks.

pub mod bench;
pub mod broker;
pub mod cli;
pub mod engine;
pub mod output;
pub mod gui;
pub mod selftest;
pub mod serve;
pub mod service;
pub mod shellmenu;

pub use cli::{data_dir, load_index, opt};

/// `<exe dir>\data` — the only place the program ever writes to.
pub fn data_dir_from_exe() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("exe path");
    exe.parent().unwrap().join("data")
}

/// Is this process running with an elevated (admin) token?
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return false;
        }
        let mut el = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(tok, TokenElevation, &mut el as *mut _ as *mut _, std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut len);
        CloseHandle(tok);
        ok != 0 && el.TokenIsElevated != 0
    }
}
