// Test code, where fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

use pf_av1::Av1Dec;
use pf_harness::Harness;
use pf_sim::corrupt::harness::{Fed, drive_pieces};
use pf_sim::corrupt::{Chunking, Corpus, Input, Tracking, corpus_gate, heap_bounded, iterations};
use profluens_core::time::Timestamp;

#[global_allocator]
static HEAP: Tracking = Tracking;

const KIB: usize = 1024;

const SLOT: usize = 64 * KIB;

const SLOTS: u32 = 2;

const NAMES: [&str; 12] = [
    "inter_64x48",
    "altref_64x48",
    "odd_61x37",
    "mono_64x48",
    "tiles_256x128",
    "grain_64x48",
    "resize_64x48_96x64",
    "tenbit_64x48",
    "tenbit_odd_61x37",
    "tenbit444_64x48",
    "i422_64x48",
    "tenbit422_64x48",
];

struct Fixture {
    name: &'static str,
    bytes: Vec<u8>,
    fields: Vec<usize>,
    frames: usize,
}

fn fixtures() -> Vec<Fixture> {
    let dir = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
    NAMES
        .iter()
        .map(|name| {
            let bytes = std::fs::read(format!("{dir}/{name}.ivf")).expect("fixture ivf");
            let frames =
                std::fs::read_to_string(format!("{dir}/{name}.md5")).expect("md5").lines().count();
            let mut fields = Vec::new();
            let mut at = 32;
            while at + 12 <= bytes.len() {
                let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                fields.extend([at, at + 1, at + 12, at + 13, at + 14, at + 15]);
                at += 12 + n;
            }
            fields.retain(|&f| f < bytes.len());
            Fixture { name, bytes, fields, frames }
        })
        .collect()
}

fn inputs(fixtures: &[Fixture]) -> Vec<Input<'_>> {
    fixtures.iter().map(|f| Input { name: f.name, bytes: &f.bytes, fields: &f.fields }).collect()
}

fn records(bytes: &[u8]) -> Vec<&[u8]> {
    if !bytes.starts_with(b"DKIF") || bytes.len() < 32 {
        return vec![bytes];
    }
    let mut out = Vec::new();
    let mut at = 32;
    while at + 12 <= bytes.len() {
        let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        at += 12;
        let end = at.saturating_add(n).min(bytes.len());
        out.push(&bytes[at..end]);
        at = end;
    }
    out
}

const TOKENS: &[&[u8]] = &[
    &[0x12, 0x00],
    &[0x0A, 0x0B, 0x00, 0x00, 0x00, 0x04, 0x3F, 0x1F],
    &[0x32],
    &[0x12, 0x00, 0x32, 0x01, 0x80],
    &[0x12, 0x00, 0x32, 0x01, 0x90],
    &[0xFF; 16],
    &[0x00; 16],
];

fn flush_point(bytes: &[u8]) -> Option<usize> {
    (bytes.len() % 2 == 1).then_some(1 + bytes.len() % 3)
}

fn element_target(bytes: &[u8], chunking: Chunking, flush: Option<usize>) -> Fed {
    let pieces: Vec<&[u8]> = match chunking {
        Chunking::Whole => records(bytes),
        other => other.pieces(bytes).collect(),
    };
    let mut h = Harness::with_pool(Av1Dec::new(), SLOT, SLOTS);
    let driven = drive_pieces(&mut h, &pieces, flush, &|i, buf| {
        buf.pts = Timestamp::from_millis(40 * i as u64);
    });
    assert!(
        driven.out_buffers <= pieces.len(),
        "{} frames from {} units",
        driven.out_buffers,
        pieces.len()
    );
    assert!(driven.out_bytes <= driven.out_buffers * SLOT);
    driven
}

#[test]
fn fixtures_decode_completely() {
    for f in fixtures() {
        let driven = element_target(&f.bytes, Chunking::Whole, None);
        assert!(!driven.failed, "{}", f.name);
        assert_eq!(driven.out_buffers, f.frames, "{} through the element", f.name);
    }
}

const HEAP_BASE: usize = 2 * (KIB * KIB) + SLOTS as usize * SLOT;
const HEAP_PER_BYTE: usize = 32 + 512;

#[test]
fn av1dec_element_survives_corruption() {
    let fixtures = fixtures();
    let gate = Corpus {
        name: "av1dec_element",
        inputs: &inputs(&fixtures),
        tokens: TOKENS,
        iters: iterations(300),
        sweep: 8,
        tolerate: &[],
    };
    corpus_gate(
        &gate,
        &heap_bounded(HEAP_BASE, HEAP_PER_BYTE, |bytes, chunking| {
            element_target(bytes, chunking, flush_point(bytes));
        }),
    );
}

#[test]
fn pure_noise_never_decodes_to_more_than_it_holds() {
    let gate = Corpus {
        name: "av1_noise",
        inputs: &[],
        tokens: TOKENS,
        iters: iterations(120),
        sweep: 0,
        tolerate: &[],
    };
    corpus_gate(
        &gate,
        &heap_bounded(HEAP_BASE, HEAP_PER_BYTE, |bytes, chunking| {
            element_target(bytes, chunking, flush_point(bytes));
        }),
    );
}
