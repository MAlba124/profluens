//! Counts heap allocations per packet of a steady `LibopusEncoder::encode`, expected 0.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use pf_opus::libopus::LibopusEncoder;
use pf_opus::{EncoderConfig, OPUS_RATE};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct Counting;
// SAFETY: delegates to the System allocator, only bumps a relaxed counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        unsafe {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            System.alloc(l)
        }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        unsafe {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            System.realloc(p, l, new)
        }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[allow(clippy::disallowed_methods)] // one-shot measurement harness
fn main() {
    for &channels in &[1u8, 2] {
        let mut enc = LibopusEncoder::new(EncoderConfig::new(channels)).unwrap();
        let ch = channels as usize;
        let flen = enc.frame_bytes() / 2;
        let n = flen / ch;

        let mut frame = vec![0i16; flen];
        let mut bytes = vec![0u8; flen * 2];

        let mut out = Vec::new();
        for k in 0..200 {
            fill(&mut frame, k * n, n, ch);
            to_le(&frame, &mut bytes);
            enc.encode(&bytes, &mut out).unwrap();
        }

        let measure = 500;
        let before = ALLOCS.load(Ordering::Relaxed);
        for k in 0..measure {
            fill(&mut frame, (200 + k) * n, n, ch);
            to_le(&frame, &mut bytes);
            enc.encode(&bytes, &mut out).unwrap();
        }
        let steady = ALLOCS.load(Ordering::Relaxed) - before;
        println!(
            "{}ch  {:>5} packets measured  {:>7} allocs  {:.3}/pkt",
            channels,
            measure,
            steady,
            steady as f64 / measure as f64,
        );
    }
    println!("\n(goal: 0/pkt in steady state, as on the decode side, see alloc_check.rs)");
}

fn to_le(frame: &[i16], bytes: &mut [u8]) {
    for (b, s) in bytes.as_chunks_mut::<2>().0.iter_mut().zip(frame) {
        b.copy_from_slice(&s.to_le_bytes());
    }
}

fn fill(frame: &mut [i16], base: usize, n: usize, ch: usize) {
    for i in 0..n {
        let t = (base + i) as f64 / OPUS_RATE as f64;
        let s = (0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 16384.0) as i16;
        for c in 0..ch {
            frame[i * ch + c] = s;
        }
    }
}
