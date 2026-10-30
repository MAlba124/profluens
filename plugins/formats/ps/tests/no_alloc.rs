//! Past construction, demuxing and cutting video units makes no allocator call.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pf_ps::es::{Au, Framer, Stamp, Video};
use pf_ps::{Demux, Event};

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

static FIXTURES: [&[u8]; 4] = [
    include_bytes!("fixtures/mpeg2_ac3.vob"),
    include_bytes!("fixtures/mpeg1_mp2.mpg"),
    include_bytes!("fixtures/lpcm24.vob"),
    include_bytes!("fixtures/h264.vob"),
];

#[test]
fn demuxing_and_framing_make_no_allocation() {
    for (n, data) in FIXTURES.iter().enumerate() {
        let mut demux = Demux::new();
        let kind = if n == 3 { Video::H264 } else { Video::Mpeg12 };
        let mut framer = Framer::new(kind, 1 << 20, 8 << 20);
        let (mut units, mut bytes) = (0u64, 0u64);
        let before = calls();
        for chunk in [1usize, 7, 188, 2048, 4096, 1 << 20] {
            let mut sink = |e: Event<'_>| {
                let Event::Unit(u) = e else { return };
                units += 1;
                if u.key >> 8 == 0xE0 || u.key >> 8 == 0xE2 {
                    let stamp = u.pts.map(|pts| Stamp { pts, dts: u.dts });
                    framer.push(u.payload, stamp, u.offset, &mut |au: Au<'_>| {
                        bytes += au.bytes.len() as u64
                    });
                }
            };
            for piece in data.chunks(chunk) {
                demux.push(piece, &mut sink);
            }
            demux.flush(&mut sink);
            framer.reset();
            demux.set_position(0);
        }
        assert_eq!(calls() - before, 0, "fixture {n}");
        assert!(units > 40 && (n == 2 || bytes > 0), "fixture {n}: {units} units {bytes} bytes");
    }
}
