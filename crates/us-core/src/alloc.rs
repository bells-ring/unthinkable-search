//! Large-block cache for the global allocator.
//!
//! The Windows heap returns freed memory to the OS (big blocks always, smaller
//! ones once enough is free); the next
//! allocation of that size gets fresh pages and pays a page fault + zeroing per
//! 4 KiB page (~0.8 µs, measured): a 1 MiB result buffer costs ~0.2 ms before a
//! single id is written. Searches allocate such buffers on every keystroke and
//! live updates rebuild the index columns, so the program keeps a few freed
//! blocks (power-of-two classes 32 KiB–8 MiB, at most `CAP` bytes in total)
//! and hands them out again: reuse costs ~0.07 µs per page.
//!
//! Opt in per binary: `#[global_allocator] static A: us_core::alloc::CachingAlloc = us_core::alloc::CachingAlloc;`

use std::alloc::{GlobalAlloc, Layout, System};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

const MIN_SHIFT: u32 = 15; // 32 KiB
const MAX_SHIFT: u32 = 23; // 8 MiB
const CLASSES: usize = (MAX_SHIFT - MIN_SHIFT + 1) as usize;
const SLOTS: usize = 48;
/// Most bytes kept in freed blocks.
pub const CAP: usize = 8 << 20;
/// Largest alignment served from the cache (Vec<u8/u32/u64> and friends).
const MAX_ALIGN: usize = 16;

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());
#[allow(clippy::declare_interior_mutable_const)]
const ROW: [AtomicPtr<u8>; SLOTS] = [EMPTY; SLOTS];
static FREE: [[AtomicPtr<u8>; SLOTS]; CLASSES] = [ROW; CLASSES];
static CACHED: AtomicUsize = AtomicUsize::new(0);

pub struct CachingAlloc;

/// Size class (log2 of the block size) for cacheable layouts.
#[inline]
fn class(l: &Layout) -> Option<u32> {
    let s = l.size();
    if l.align() > MAX_ALIGN || !((1 << MIN_SHIFT)..=(1 << MAX_SHIFT)).contains(&s) {
        return None;
    }
    Some(s.next_power_of_two().trailing_zeros())
}

#[inline]
fn block(c: u32, align: usize) -> Layout {
    // SAFETY: power of two size ≤ 8 MiB, align ≤ 16 and a power of two.
    unsafe { Layout::from_size_align_unchecked(1usize << c, align) }
}

/// Bytes currently held in the cache (diagnostics).
pub fn cached_bytes() -> usize {
    CACHED.load(Ordering::Relaxed)
}

/// Return every cached block to the OS. Called after an index load/rebuild:
/// its temporary buffers are useless for searches. No-op unless
/// `CachingAlloc` is the global allocator.
pub fn trim() {
    for (ci, row) in FREE.iter().enumerate() {
        let c = MIN_SHIFT + ci as u32;
        for s in row {
            let p = s.swap(ptr::null_mut(), Ordering::Acquire);
            if !p.is_null() {
                CACHED.fetch_sub(1 << c, Ordering::Relaxed);
                // SAFETY: cached blocks come from System with this size and align ≤ 16.
                unsafe { System.dealloc(p, block(c, MAX_ALIGN)) };
            }
        }
    }
}

unsafe impl GlobalAlloc for CachingAlloc {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let Some(c) = class(&l) else { return unsafe { System.alloc(l) } };
        for s in &FREE[(c - MIN_SHIFT) as usize] {
            if s.load(Ordering::Relaxed).is_null() {
                continue; // cheap probe before the locked swap
            }
            let p = s.swap(ptr::null_mut(), Ordering::Acquire);
            if !p.is_null() {
                CACHED.fetch_sub(1 << c, Ordering::Relaxed);
                return p;
            }
        }
        unsafe { System.alloc(block(c, l.align())) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let Some(c) = class(&l) else { return unsafe { System.alloc_zeroed(l) } };
        for s in &FREE[(c - MIN_SHIFT) as usize] {
            if s.load(Ordering::Relaxed).is_null() {
                continue; // cheap probe before the locked swap
            }
            let p = s.swap(ptr::null_mut(), Ordering::Acquire);
            if !p.is_null() {
                CACHED.fetch_sub(1 << c, Ordering::Relaxed);
                unsafe { ptr::write_bytes(p, 0, l.size()) };
                return p;
            }
        }
        unsafe { System.alloc_zeroed(block(c, l.align())) }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        let Some(c) = class(&l) else { return unsafe { System.dealloc(p, l) } };
        if CACHED.load(Ordering::Relaxed) + (1 << c) <= CAP {
            for s in &FREE[(c - MIN_SHIFT) as usize] {
                if !s.load(Ordering::Relaxed).is_null() {
                    continue;
                }
                if s.compare_exchange(ptr::null_mut(), p, Ordering::Release, Ordering::Relaxed).is_ok() {
                    CACHED.fetch_add(1 << c, Ordering::Relaxed);
                    return;
                }
            }
        }
        unsafe { System.dealloc(p, block(c, l.align())) }
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc contract — new_size > 0, same alignment.
        let nl = unsafe { Layout::from_size_align_unchecked(new_size, l.align()) };
        match (class(&l), class(&nl)) {
            // Same class: the block is already big enough (Vec doubling stays in place).
            (Some(a), Some(b)) if a == b => p,
            // Neither side cached: let the system heap grow in place if it can.
            (None, None) => unsafe { System.realloc(p, l, new_size) },
            _ => {
                let np = unsafe { self.alloc(nl) };
                if !np.is_null() {
                    unsafe {
                        ptr::copy_nonoverlapping(p, np, l.size().min(new_size));
                        self.dealloc(p, l);
                    }
                }
                np
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_and_reuse() {
        let a = CachingAlloc;
        unsafe {
            let l = Layout::from_size_align(300 << 10, 8).unwrap(); // → 512 KiB class
            let p = a.alloc(l);
            assert!(!p.is_null());
            ptr::write_bytes(p, 0xAB, l.size());
            // grow inside the class: same pointer
            let q = a.realloc(p, l, 500 << 10);
            assert_eq!(p, q);
            let l2 = Layout::from_size_align(500 << 10, 8).unwrap();
            a.dealloc(q, l2);
            let z = a.alloc_zeroed(l);
            assert!(std::slice::from_raw_parts(z, l.size()).iter().all(|&b| b == 0));
            // grow across classes keeps the contents
            ptr::write_bytes(z, 7, l.size());
            let big = a.realloc(z, l, 3 << 20);
            assert!(std::slice::from_raw_parts(big, l.size()).iter().all(|&b| b == 7));
            a.dealloc(big, Layout::from_size_align(3 << 20, 8).unwrap());
            // small and huge blocks bypass the cache
            let s = Layout::from_size_align(1024, 8).unwrap();
            let sp = a.alloc(s);
            a.dealloc(sp, s);
            let h = Layout::from_size_align(32 << 20, 8).unwrap();
            let hp = a.alloc(h);
            a.dealloc(hp, h);
            assert!(cached_bytes() <= CAP);
        }
    }
}
