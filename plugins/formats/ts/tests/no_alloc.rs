//! After build and first PID mapping or buffer growth, demuxing makes no allocator call.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::{FIXTURES, Rng, SIZES, reframe};
use pf_mpegts::{Config, Demux, Event, Limits};

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

#[derive(Default)]
struct Seen {
    units: u64,
    bytes: u64,
    other: u64,
}

fn pass(demux: &mut Demux, data: &[u8], chunk: usize, seen: &mut Seen) {
    let mut sink = |event: Event<'_>| match event {
        Event::Pes(u) => {
            seen.units += 1;
            seen.bytes += u.payload.len() as u64;
        }
        _ => seen.other += 1,
    };
    for piece in data.chunks(chunk) {
        demux.push(piece, &mut sink);
    }
}

#[test]
fn steady_state_makes_no_allocation() {
    for f in FIXTURES {
        for size in SIZES {
            let data = reframe(f.data, size);
            let mut demux = Demux::new(Config::default()).unwrap();
            let mut seen = Seen::default();

            // Every unit fits the initial reserve, so the only allocations are one buffer per PID.
            let before = calls();
            pass(&mut demux, &data, 4096, &mut seen);
            assert_eq!(
                calls() - before,
                demux.pes_pids() as u64,
                "{} as {size:?} first pass",
                f.name
            );
            assert!(demux.pes_pids() > 0);
            assert_eq!(demux.packet_size(), Some(size));

            let before = calls();
            for round in 0..20 {
                pass(&mut demux, &data, 1 + round * 331, &mut seen);
            }
            demux.flush(&mut |_| {});
            assert_eq!(calls() - before, 0, "{} as {size:?} steady state", f.name);
            assert!(seen.units > 100 && seen.bytes > 0 && seen.other > 0);
        }
    }
}

#[test]
fn searching_for_the_packet_size_makes_no_allocation() {
    let f = &FIXTURES[0];
    let forms = SIZES.map(|size| reframe(f.data, size));
    let mut rng = Rng::new(11);
    let mut junk = [0u8; 40_000];
    for b in &mut junk {
        *b = rng.next() as u8;
    }
    let mut demux = Demux::new(Config::default()).unwrap();
    let mut seen = Seen::default();
    pass(&mut demux, &forms[0], 4096, &mut seen);

    let before = calls();
    let mut sizes = [0u32; 3];
    for round in 0..60 {
        let form = &forms[rng.below(3)];
        let cut = rng.below(form.len());
        pass(&mut demux, &form[..cut], 1 + rng.below(3000), &mut seen);
        if let Some(size) = demux.packet_size() {
            sizes[size as usize] += 1;
        }
        match round % 5 {
            0 => pass(&mut demux, &junk[..rng.below(junk.len())], 613, &mut seen),
            1 => demux.discontinuity(),
            2 => demux.flush(&mut |_| {}),
            _ => {}
        }
    }
    assert_eq!(calls() - before, 0);
    assert!(sizes.iter().all(|&n| n > 0), "every packet size was locked, {sizes:?}");
}

#[test]
fn damage_and_control_calls_make_no_allocation() {
    let f = &FIXTURES[0];
    let mut demux = Demux::new(Config::default()).unwrap();
    let mut seen = Seen::default();
    pass(&mut demux, f.data, 4096, &mut seen);
    let mut rng = Rng::new(7);
    let mut junk = [0u8; 1500];
    for b in &mut junk {
        *b = rng.next() as u8;
    }

    let before = calls();
    for round in 0..50 {
        pass(&mut demux, &junk[..1 + rng.below(1499)], 97, &mut seen);
        let cut = rng.below(f.data.len());
        pass(&mut demux, &f.data[..cut], 777, &mut seen);
        match round % 4 {
            0 => demux.flush(&mut |_| {}),
            1 => demux.discontinuity(),
            2 => {
                demux.select(0x101, round % 8 == 2);
                demux.set_position(0);
            }
            _ => {}
        }
        pass(&mut demux, f.data, 4096, &mut seen);
    }
    assert_eq!(calls() - before, 0);

    let before = calls();
    demux.reset();
    pass(&mut demux, f.data, 4096, &mut seen);
    demux.flush(&mut |_| {});
    assert_eq!(calls() - before, 0);
}

#[test]
fn buffers_grow_to_the_unit_size_and_no_further() {
    let f = &FIXTURES[0];
    let limits = Limits { pes_reserve_bytes: 0, ..Limits::default() };
    let mut demux = Demux::new(Config { limits, select_all: true }).unwrap();
    let mut seen = Seen::default();
    let before = calls();
    pass(&mut demux, f.data, 4096, &mut seen);
    let grown = calls() - before;
    assert!(grown > demux.pes_pids() as u64, "the small reserve had to grow");
    assert!(grown < 16, "growth doubles, {grown} calls");
    let before = calls();
    for _ in 0..5 {
        pass(&mut demux, f.data, 4096, &mut seen);
    }
    assert_eq!(calls() - before, 0);
    assert!(demux.pes_buffer_capacity() <= limits.max_pes_bytes as usize);
}
