//! Demux throughput and steady state allocations over fixtures in memory, `main` is the harness.
//! Run with `cargo bench -p pf-mpegts --bench demux_throughput`.

#![allow(clippy::disallowed_methods)] // bench fixtures

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use pf_mpegts::{Config, Demux, Event};

#[path = "../tests/common/mod.rs"]
mod common;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

struct CountingAlloc;

// SAFETY: every call forwards to `System` unchanged, the count is bookkeeping only.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

const CHUNK: usize = 64 * 1024;
const TRIAL_BYTES: usize = 24 << 20;
const TRIALS: usize = 9;
const WARMUP: usize = 2;

struct Totals {
    units: u64,
    payload: u64,
    pcrs: u64,
    anomalies: u64,
}

fn run(demux: &mut Demux, data: &[u8], passes: usize, totals: &mut Totals) {
    let mut sink = |event: Event<'_>| match event {
        Event::Pes(unit) => {
            totals.units += 1;
            totals.payload += black_box(unit.payload).len() as u64;
        }
        Event::Pcr { pcr, .. } => totals.pcrs += black_box(pcr) & 1,
        Event::Anomaly(_) => totals.anomalies += 1,
        Event::Pat { .. } | Event::Pmt { .. } => {}
    };
    for _ in 0..passes {
        for chunk in data.chunks(CHUNK) {
            demux.push(black_box(chunk), &mut sink);
        }
    }
}

fn bench(name: &str, data: &[u8]) {
    bench_framed(name, data, 188);
}

fn bench_framed(name: &str, data: &[u8], packet: usize) {
    let mut demux = Demux::new(Config::default()).expect("default limits are valid");
    let mut totals = Totals { units: 0, payload: 0, pcrs: 0, anomalies: 0 };
    let passes = (TRIAL_BYTES / data.len()).max(1);
    for _ in 0..WARMUP {
        run(&mut demux, data, passes, &mut totals);
    }
    let mut seconds = [0f64; TRIALS];
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for s in &mut seconds {
        let start = Instant::now();
        run(&mut demux, data, passes, &mut totals);
        *s = start.elapsed().as_secs_f64();
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    seconds.sort_by(f64::total_cmp);
    let median = seconds[TRIALS / 2];
    let bytes = (data.len() * passes) as f64;
    println!(
        "{name:<14} {:>8.0} MB/s {:>7.1}M packets/s  {} allocations in {} trials  ({} units, {} payload bytes, {} anomalies)",
        bytes / median / 1e6,
        bytes / packet as f64 / median / 1e6,
        allocations,
        TRIALS,
        totals.units,
        totals.payload,
        totals.anomalies,
    );
    assert_eq!(allocations, 0, "{name} allocated in steady state");
    black_box(totals.pcrs);
}

#[allow(clippy::disallowed_methods)] // bench setup, built before anything is timed
fn broadcast_like() -> Vec<u8> {
    use common::{Fill, Ts, pat_section, pattern, pes_bytes, pmt_section};
    let mut ts = Ts::new();
    let pat = pat_section(1, 0, &[(1, 0x1000)]);
    let pmt = pmt_section(1, 0, 0x100, &[], &[(0x1B, 0x100, &[]), (0x0F, 0x101, &[])]);
    let picture = pattern(1, 25_000);
    let audio = pattern(2, 576);
    for n in 0..50u64 {
        ts.section(0, &pat);
        ts.section(0x1000, &pmt);
        let dts = 90_000 + n * 3600;
        ts.packet(0x100, false, 0, Some(dts * 300), &[], Fill::Adaptation);
        ts.pes(0x100, 0x40, &pes_bytes(0xE0, Some(dts + 7200), Some(dts), &picture, false));
        for k in 0..2 {
            ts.pes(0x101, 0, &pes_bytes(0xC0, Some(dts + k * 1800), None, &audio, true));
        }
    }
    ts.out
}

fn main() {
    let broadcast = broadcast_like();
    bench("broadcast like", &broadcast);
    for size in common::SIZES.into_iter().skip(1) {
        let packet = pf_mpegts::framed_len(size);
        bench_framed(&format!("same in {packet}"), &common::reframe(&broadcast, size), packet);
    }
    bench("h264_aac.ts", include_bytes!("../tests/fixtures/h264_aac.ts"));
    bench("hevc_ac3.ts", include_bytes!("../tests/fixtures/hevc_ac3.ts"));
    bench("mpeg2.ts", include_bytes!("../tests/fixtures/mpeg2.ts"));
    let mut nulls = [0xFFu8; 188 * 256];
    for packet in nulls.as_chunks_mut::<188>().0 {
        packet[..4].copy_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
    }
    bench("null packets", &nulls);
}
