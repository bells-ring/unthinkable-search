//! usc.exe — command-line interface (index / search / bench / watch / selftest).

#[global_allocator]
static ALLOC: us_core::alloc::CachingAlloc = us_core::alloc::CachingAlloc;

fn main() {
    std::process::exit(us::cli::main());
}
