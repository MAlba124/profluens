//! Per-`process()` scratch, a stein-arena [`Bump`] over one mapping reserved on first use.
#![allow(unsafe_code)]

use std::alloc::{AllocError, Allocator, Layout};
use std::cell::OnceCell;
use std::ptr::NonNull;

use stein_arena::{ArenaError, Bump};

/// Address space one scratch reserves, virtual so it costs no memory. 16 MiB on 32-bit.
#[cfg(target_pointer_width = "64")]
pub const SCRATCH_CAP: usize = 256 << 20;
#[cfg(not(target_pointer_width = "64"))]
pub const SCRATCH_CAP: usize = 16 << 20;

/// Why a scratch allocation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScratchError {
    Reserve(ArenaError),
    Full { asked: usize, left: usize },
}

impl std::fmt::Display for ScratchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reserve(e) => write!(f, "scratch reservation failed, {e}"),
            Self::Full { asked, left } => {
                write!(f, "scratch full, {asked} bytes asked, {left} left")
            }
        }
    }
}

impl std::error::Error for ScratchError {}

impl From<ArenaError> for ScratchError {
    fn from(e: ArenaError) -> Self {
        match e {
            ArenaError::Full { asked, left } => Self::Full { asked, left },
            e => Self::Reserve(e),
        }
    }
}

/// The smallest page size the bump's mapping is aligned to.
const BASE_ALIGN: usize = 4096;

/// A bump allocator reset between calls through `&mut self`, so nothing handed out survives a
/// reset.
pub struct Arena {
    bump: OnceCell<Bump>,
    cap: usize,
}

impl Arena {
    pub const fn with_cap(cap: usize) -> Self {
        Self { bump: OnceCell::new(), cap }
    }

    fn bump(&self) -> Result<&Bump, ScratchError> {
        if let Some(b) = self.bump.get() {
            return Ok(b);
        }
        let b = Bump::reserve(self.cap)?;
        Ok(self.bump.get_or_init(|| b))
    }

    #[allow(clippy::mut_from_ref)]
    pub fn alloc_bytes(&self, n: usize) -> Result<&mut [u8], ScratchError> {
        Ok(self.bump()?.alloc_bytes(n)?)
    }

    /// `size` zero bytes aligned to `align`, a power of two, until the next reset.
    #[allow(clippy::mut_from_ref)]
    pub fn alloc(&self, size: usize, align: usize) -> Result<&mut [u8], ScratchError> {
        let ptr = self.bump()?.alloc_raw(size, align)?;
        // SAFETY: `size` bytes of the mapping no other live block overlaps, written before the
        // slice is formed.
        unsafe {
            ptr.as_ptr().write_bytes(0, size);
            Ok(std::slice::from_raw_parts_mut(ptr.as_ptr(), size))
        }
    }

    pub fn reset(&mut self) {
        if let Some(b) = self.bump.get_mut() {
            b.reset();
        }
    }

    pub fn used(&self) -> usize {
        self.bump.get().map_or(0, Bump::len)
    }
}

impl Default for Arena {
    fn default() -> Self {
        Self::with_cap(SCRATCH_CAP)
    }
}

// SAFETY: blocks never overlap and live until a reset, which needs `&mut` and so outlives every
// `&Arena`.
unsafe impl Allocator for &Arena {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // The bump aligns offsets, which holds only for alignments that divide its page aligned
        // base.
        if layout.align() > BASE_ALIGN {
            return Err(AllocError);
        }
        if layout.size() == 0 {
            // Aligned and non-null by construction, never dereferenced.
            let dangling = NonNull::new(std::ptr::without_provenance_mut::<u8>(layout.align()))
                .ok_or(AllocError)?;
            return Ok(NonNull::slice_from_raw_parts(dangling, 0));
        }
        let bump = self.bump().map_err(|_| AllocError)?;
        let ptr = bump.alloc_raw(layout.size(), layout.align()).map_err(|_| AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    // SAFETY: `ptr` is a block of this arena. Only the newest one goes back.
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if let (Some(b), true) = (self.bump.get(), layout.size() > 0) {
            // SAFETY: as above, nothing uses the block after this.
            unsafe { b.free_last(ptr.as_ptr(), layout.size()) };
        }
    }

    // SAFETY: `ptr` is a block of this arena, moved to a fresh block that overlaps no live one if
    // not in place.
    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if let Some(b) = self.bump.get() {
            let end = ptr.as_ptr().wrapping_add(old.size());
            if old.size() > 0
                && new.align() <= old.align()
                && b.grow_last(end, new.size() - old.size())
            {
                return Ok(NonNull::slice_from_raw_parts(ptr, new.size()));
            }
        }
        let fresh = self.allocate(new)?;
        // SAFETY: `fresh` holds at least `old.size()` bytes and overlaps no live block.
        unsafe {
            std::ptr::copy_nonoverlapping(ptr.as_ptr(), fresh.as_ptr().cast::<u8>(), old.size())
        };
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocs_are_disjoint_and_writable() {
        let arena = Arena::with_cap(1 << 20);
        let a = arena.alloc_bytes(16).unwrap();
        let b = arena.alloc_bytes(16).unwrap();
        a.fill(0xAA);
        b.fill(0xBB);
        assert!(a.iter().all(|&x| x == 0xAA));
        assert!(b.iter().all(|&x| x == 0xBB), "second region untouched by the first");
        assert_eq!((a.len(), b.len()), (16, 16));
    }

    #[test]
    fn nothing_is_reserved_until_first_use() {
        let mut arena = Arena::default();
        assert!(arena.bump.get().is_none());
        arena.reset();
        assert_eq!(arena.used(), 0);
        arena.alloc_bytes(1).unwrap();
        assert!(arena.bump.get().is_some());
    }

    #[test]
    fn reset_reuses_memory_and_bytes_read_zero() {
        let mut arena = Arena::with_cap(1 << 20);
        let addr = {
            let a = arena.alloc_bytes(100).unwrap();
            a.fill(7);
            a.as_ptr() as usize
        };
        arena.reset();
        assert_eq!(arena.used(), 0);
        let b = arena.alloc_bytes(100).unwrap();
        assert_eq!(b.as_ptr() as usize, addr, "reset rewinds to the same memory");
        assert!(b.iter().all(|&x| x == 0));
    }

    #[test]
    fn aligned_allocs_are_aligned_and_zero() {
        let mut arena = Arena::with_cap(1 << 20);
        arena.alloc_bytes(1).unwrap().fill(5);
        let s = arena.alloc(64, 64).unwrap();
        assert_eq!(s.as_ptr() as usize % 64, 0);
        s.fill(3);
        arena.reset();
        arena.alloc_bytes(1).unwrap();
        let s = arena.alloc(64, 64).unwrap();
        assert!(s.iter().all(|&x| x == 0), "a reused aligned region kept old bytes");
    }

    #[test]
    fn allocator_respects_alignment() {
        let arena = Arena::with_cap(1 << 20);
        arena.alloc_bytes(1).unwrap();
        let p = (&arena).allocate(Layout::from_size_align(64, 64).unwrap()).unwrap();
        assert_eq!(p.cast::<u8>().as_ptr() as usize % 64, 0);
        let z = (&arena).allocate(Layout::from_size_align(0, 32).unwrap()).unwrap();
        assert_eq!(z.len(), 0);
    }

    #[test]
    fn vec_grows_last_allocation_in_place() {
        let arena = Arena::with_cap(1 << 20);
        {
            let mut v: Vec<u64, &Arena> = Vec::new_in(&arena);
            for i in 0..500u64 {
                v.push(i);
            }
            assert_eq!(v.len(), 500);
            assert_eq!(v.iter().copied().sum::<u64>(), (0..500u64).sum::<u64>());
            let used = arena.used();
            assert!(used <= 512 * 8 + 64, "grow was not in place, arena used {used} bytes");
        }
        assert_eq!(arena.used(), 0, "last allocation was not reclaimed on drop");
    }

    #[test]
    fn a_vec_behind_another_allocation_moves_and_keeps_its_data() {
        let arena = Arena::with_cap(1 << 20);
        let mut v: Vec<u32, &Arena> = Vec::with_capacity_in(4, &arena);
        v.extend_from_slice(&[1, 2, 3, 4]);
        let fence = arena.alloc_bytes(8).unwrap();
        fence.fill(9);
        v.push(5);
        assert_eq!(v.as_slice(), &[1, 2, 3, 4, 5]);
        assert!(fence.iter().all(|&x| x == 9), "the move overwrote a live region");
    }

    #[test]
    fn reclaims_only_the_last_allocation() {
        let arena = Arena::with_cap(1 << 20);
        let a_ref = &arena;
        let l = Layout::from_size_align(64, 8).unwrap();
        let a = a_ref.allocate(l).unwrap();
        let after_a = arena.used();
        let b = a_ref.allocate(l).unwrap();
        let after_b = arena.used();
        assert!(after_b > after_a);
        unsafe { a_ref.deallocate(a.cast(), l) };
        assert_eq!(arena.used(), after_b, "interior free must not rewind");
        unsafe { a_ref.deallocate(b.cast(), l) };
        assert_eq!(arena.used(), after_a, "last free must rewind the cursor");
    }

    #[test]
    fn a_spent_cap_is_an_error_not_an_abort() {
        let arena = Arena::with_cap(4096);
        assert!(arena.alloc_bytes(4096).is_ok());
        assert!(matches!(arena.alloc_bytes(1), Err(ScratchError::Full { asked: 1, left: 0 })));
        assert!((&arena).allocate(Layout::from_size_align(8, 8).unwrap()).is_err());
    }
}
