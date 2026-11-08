//! Past warming up, walking chunks and cutting frames makes no allocator call.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pf_avi::codec::Framer;
use pf_avi::{MoviWalker, probe_header};

// Per thread so parallel tests don't interfere, const init so reading it never allocates.
thread_local! {
    static CALLS: Cell<u64> = const { Cell::new(0) };
}

fn calls() -> u64 {
    CALLS.with(Cell::get)
}

fn count() {
    CALLS.with(|c| c.set(c.get() + 1));
}

struct CountingAlloc;

// SAFETY: every call forwards to `System` unchanged, the count is bookkeeping only.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

static TINY: &[u8] = include_bytes!("fixtures/tiny.avi");

/// Chunks and payload bytes seen walking `movi` pushed a piece at a time.
fn walk(w: &mut MoviWalker, movi: &[u8]) -> (usize, usize) {
    let (mut chunks, mut bytes) = (0, 0);
    for piece in movi.chunks(4096) {
        w.push(piece);
        while let Some(c) = w.peek_chunk() {
            assert_eq!(w.peek_chunk(), Some(c), "a peek leaves the chunk at the head");
            (chunks, bytes) = (chunks + 1, bytes + w.data(&c).len());
            w.take(&c);
        }
    }
    (chunks, bytes)
}

#[test]
fn a_warm_walker_hands_out_chunks_without_an_allocation() {
    let (_, movi) = probe_header(TINY).expect("an avi");
    let movi = &TINY[movi.expect("movi").movi_data_start as usize..];
    let mut w = MoviWalker::new();
    let cold = walk(&mut w, movi);
    assert_eq!(cold.0, 75 + 87, "every video and audio chunk");
    w.resync();
    let before = calls();
    assert_eq!(walk(&mut w, movi), cold, "the same chunks again");
    assert_eq!(calls() - before, 0, "the walker's buffer reused");
}

// The test's stream is built on the heap, before the count.
#[allow(clippy::disallowed_methods)]
#[test]
fn a_warm_framer_cuts_frames_without_an_allocation() {
    let mut stream = Vec::new();
    for n in 0..20u8 {
        let mut f = vec![0x0b, 0x77, 0, 0, 30, 8 << 3];
        f.resize(1792, n);
        stream.extend_from_slice(&f);
    }
    let cut = |fr: &mut Framer| {
        let mut frames = 0;
        for slice in stream.chunks(2333) {
            fr.push("ac3", slice);
            while let Some(f) = fr.frame() {
                assert_eq!((f.len(), &f[..2]), (1792, &[0x0b, 0x77][..]));
                fr.take();
                frames += 1;
            }
        }
        frames
    };
    let mut fr = Framer::default();
    assert_eq!(cut(&mut fr), 20);
    fr.clear();
    let before = calls();
    assert_eq!(cut(&mut fr), 20);
    assert_eq!(calls() - before, 0, "the carry and frame list reused");
}
