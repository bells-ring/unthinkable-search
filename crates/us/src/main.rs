//! us.exe — GUI (no console window). CLI lives in usc.exe.
//! `us.exe [--live off] [--data DIR]` = GUI; `us.exe --broker <pipe> <gui pid>` =
//! the elevated metadata broker (see broker.rs).
#![windows_subsystem = "windows"]

#[global_allocator]
static ALLOC: us_core::alloc::CachingAlloc = us_core::alloc::CachingAlloc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--broker") => us::broker::broker_main(&args[1..]),
        _ => us::gui::run_with(&args),
    };
    std::process::exit(code);
}
