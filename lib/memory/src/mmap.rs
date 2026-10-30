//! Anonymous mappings as pool regions. Alignment above a page maps extra and trims the head and
//! tail.

use std::ffi::CStr;

use profluens_core::memory::Allocator;

use crate::name::name_anon;

pub fn page_size() -> usize {
    // SAFETY: a query, no pointer.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n > 0 { n as usize } else { 4096 }
}

pub const HUGE_PAGE: usize = 2 << 20;

const fn flags(ctx: *mut ()) -> libc::c_int {
    let base = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    if ctx.is_null() { base } else { base | libc::MAP_HUGETLB }
}

unsafe fn map(ctx: *mut (), len: usize, align: usize, name: &CStr) -> *mut u8 {
    let (page, huge) = (page_size(), !ctx.is_null());
    let unit = if huge { HUGE_PAGE } else { page };
    let len = len.div_ceil(unit) * unit;
    let spare = if align > unit { align } else { 0 };
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    // SAFETY: an anonymous mapping, no pointer in and `-1` for the fd.
    let base = unsafe { libc::mmap(std::ptr::null_mut(), len + spare, prot, flags(ctx), -1, 0) };
    if base == libc::MAP_FAILED {
        return std::ptr::null_mut();
    }
    let base = base as usize;
    let aligned = base.div_ceil(align.max(1)) * align.max(1);
    // SAFETY: the head and the tail are inside the fresh mapping and unreferenced.
    unsafe {
        if aligned > base {
            libc::munmap(base as *mut libc::c_void, aligned - base);
        }
        let tail = base + len + spare - (aligned + len);
        if tail > 0 {
            libc::munmap((aligned + len) as *mut libc::c_void, tail);
        }
    }
    if !name.is_empty() {
        let _ = name_anon(aligned, len, Some(name));
    }
    aligned as *mut u8
}

unsafe fn unmap(ctx: *mut (), base: *mut u8, len: usize, _align: usize, _token: u64) {
    let unit = if ctx.is_null() { page_size() } else { HUGE_PAGE };
    // SAFETY: `base` and the rounded `len` are the mapping `map` left after trimming.
    unsafe { libc::munmap(base as *mut libc::c_void, len.div_ceil(unit) * unit) };
}

// SAFETY: anonymous private mappings, zeroed, unmapped once, with no context to outlive.
pub const ANON: Allocator = unsafe { Allocator::new(std::ptr::null_mut(), map, unmap, None) };

pub const HUGE: Allocator =
    // SAFETY: as `ANON`, the dangling context is never dereferenced.
    unsafe { Allocator::new(std::ptr::dangling_mut::<()>(), map, unmap, None) };
