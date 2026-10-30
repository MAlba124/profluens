//! Per-thread heap accounting. Install [`Tracking`] as `#[global_allocator]`, else it reads zero.
//!
//! It counts the Rust heap of the calling thread only. What a C library mallocs (libopus,
//! libFLAC, dav1d) and what another thread allocates are not seen, and a free here of memory
//! another thread allocated lowers the count. A bound on a C decoder's state needs its allocation
//! routed through Rust, or an RSS sample.

// Needed to implement `GlobalAlloc`. The only such module in the crate.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    // Const-initialised and without a destructor, so touching them never allocates.
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

/// What a pool maps on first touching a size class, a first region of up to 256 KiB and
/// its table of 32 entries of 16 bytes (`profluens_core::memory`).
pub const CLASS_REGION_COST: usize = (256 << 10) + 32 * 16;

pub struct Tracking;

fn grow(bytes: usize) {
    let _ = LIVE.try_with(|live| {
        let now = live.get().wrapping_add(bytes as isize);
        live.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

fn shrink(bytes: usize) {
    let _ = LIVE.try_with(|live| live.set(live.get().wrapping_sub(bytes as isize)));
}

// SAFETY: every call forwards to `System` with the caller's arguments unchanged.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grow(layout.size());
        let p = unsafe { System.alloc(layout) };
        if p.is_null() {
            shrink(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grow(layout.size());
        let p = unsafe { System.alloc_zeroed(layout) };
        if p.is_null() {
            shrink(layout.size());
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Grown before shrunk, a moving realloc holds both for a moment.
        grow(new_size);
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        shrink(if p.is_null() { new_size } else { layout.size() });
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        shrink(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }
}

pub fn heap_mark() -> isize {
    let live = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(live));
    live
}

pub fn heap_growth(mark: isize) -> usize {
    (PEAK.with(Cell::get) - mark).max(0) as usize
}

pub fn heap_during<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let mark = heap_mark();
    let result = f();
    (result, heap_growth(mark))
}
