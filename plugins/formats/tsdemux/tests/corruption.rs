//! The untrusted-input gate for `tsdemux`, seeded corruptions whole and in pieces, and a seek.

#![allow(clippy::disallowed_methods)] // test fixtures

use pf_harness::Harness;
use pf_sim::corrupt::harness::{Shim, feed_bytes, pool_allowance};
use pf_sim::corrupt::{
    Case, Chunking, Site, Tracking, Violation, check_heap, check_output, gate, heap_mark, mutate,
    noise, prefix_then_garbage, sweep_gate,
};
use pf_sim::prng::Prng;
use pf_tsdemux::TsDemux;

#[global_allocator]
static HEAP: Tracking = Tracking;

static H264_AAC: &[u8] = include_bytes!("../../ts/tests/fixtures/h264_aac.ts");
static HEVC_AC3: &[u8] = include_bytes!("../../ts/tests/fixtures/hevc_ac3.ts");
static WRAP: &[u8] = include_bytes!("../../ts/tests/fixtures/wrap.ts");
static AAC_LATM: &[u8] = include_bytes!("../../ts/tests/fixtures/aac_latm.ts");

struct Base {
    name: &'static str,
    data: &'static [u8],
    sites: Vec<Site>,
}

fn sites(data: &[u8]) -> Vec<Site> {
    let mut sites = Vec::new();
    for at in (0..data.len()).step_by(188) {
        sites.push(Site::boundary(at));
        for field in (0..24).step_by(4) {
            sites.push(Site::be(at + field, 4));
        }
    }
    sites
}

fn bases() -> Vec<Base> {
    [("h264_aac", H264_AAC), ("hevc_ac3", HEVC_AC3), ("wrap", WRAP), ("aac_latm", AAC_LATM)]
        .map(|(name, data)| Base { name, data, sites: sites(data) })
        .into()
}

fn case(prng: &mut Prng, bases: &[Base]) -> Case {
    let base = &bases[prng.below(bases.len() as u64) as usize];
    let (data, ops) = match prng.below(20) {
        0 => noise(prng, 2048),
        1 | 2 => prefix_then_garbage(prng, base.data, &base.sites, 1024),
        _ => mutate(prng, base.data, &base.sites),
    };
    let chunking = Chunking::pick(prng, data.len());
    Case { base: base.name, ops, data, chunking, knob: prng.next_u64() }
}

/// A buffer per selected stream up to 4 MiB, 64 KiB reserved each, at most 32 streams.
const DEMUX_HEAP: usize = 32 * (64 << 10) + (4 << 20) + (1 << 20);

fn drive(case: &Case, bases: &[Base]) -> Result<(), Violation> {
    let mark = heap_mark();
    let pristine = bases.iter().find(|b| b.name == case.base).map_or(bases[0].data, |b| b.data);
    let head = match case.knob >> 4 & 1 {
        0 => case.data.clone(),
        _ => pristine.to_vec(),
    };
    let flush = (case.knob >> 5 & 3 == 0).then_some((case.knob >> 8) as usize % 16);
    let fed = feed_bytes(TsDemux::new(head), case, flush, case.data.len() + 64)?;
    // ADTS loses its header and gains a 2-octet config per flush or change, up to a 7-octet frame.
    check_output(fed.out_bytes, 2 * case.data.len() + 64)?;
    // The head probe and the run each hold a demuxer, a parked unit is copied once.
    check_heap(mark, 2 * DEMUX_HEAP + 8 * case.data.len() + pool_allowance(case.knob))
}

#[test]
fn the_element_survives_corrupt_streams() {
    let bases = bases();
    gate("tsdemux", 1_500, &|prng| case(prng, &bases), &|case| drive(case, &bases));
}

#[test]
fn the_element_survives_every_truncation_and_extreme_field() {
    let bases = bases();
    for base in &bases {
        sweep_gate("tsdemux sweep", base.name, base.data, &base.sites, 120, &|case| {
            drive(case, &bases)
        });
    }
}

#[test]
fn the_driver_collects_what_the_element_puts_out() {
    for knob in [0u64, 1 << 6, 3 << 6] {
        let case = Case {
            base: "h264_aac",
            ops: Vec::new(),
            data: H264_AAC.to_vec(),
            chunking: Chunking::Fixed(3000),
            knob,
        };
        let fed = feed_bytes(TsDemux::new(H264_AAC.to_vec()), &case, None, H264_AAC.len())
            .expect("in bounds");
        assert!(!fed.failed);
        assert_eq!(fed.out_buffers, 30 + 96, "knob {knob}");
        assert!(fed.out_bytes > H264_AAC.len() / 2 && fed.out_bytes < H264_AAC.len());
    }
}

fn stamps(buffers: &[profluens_core::buffer::Buffer]) -> Vec<(Option<u64>, usize)> {
    buffers.iter().map(|b| (b.pts.nanos(), b.memory.len())).collect()
}

#[test]
fn a_seek_in_a_file_keeps_its_timeline() {
    let shim = || Shim { inner: TsDemux::new(H264_AAC.to_vec()) };
    let mut whole = Harness::with_pool(shim(), 1 << 16, 256);
    whole.start().expect("start");
    for piece in H264_AAC.chunks(4096) {
        let buf = whole.alloc(piece);
        whole.push(buf).expect("push");
    }
    let mut reference = stamps(&whole.drain_outputs());
    reference.extend(stamps(&whole.eos().expect("eos")));
    assert_eq!(reference.len(), 30 + 96);

    let mut h = Harness::with_pool(shim(), 1 << 16, 256);
    h.start().expect("start");
    for piece in H264_AAC[..188 * 60].chunks(4096) {
        let buf = h.alloc(piece);
        h.push(buf).expect("push");
    }
    let before = h.drain_outputs().len();
    assert!(before > 0);
    let to = H264_AAC.len() * 2 / 3 + 50;
    h.seek(to as u64, profluens_core::time::Timestamp(1_300_000_000)).expect("seek");
    for piece in H264_AAC[to..].chunks(4096) {
        let buf = h.alloc(piece);
        h.push(buf).expect("push");
    }
    let mut after = stamps(&h.drain_outputs());
    after.extend(stamps(&h.eos().expect("eos")));
    let units: Vec<&(Option<u64>, usize)> = after.iter().filter(|u| u.1 != 2).collect();
    assert!(units.len() > 20 && units.len() < reference.len() / 2, "{} units", units.len());
    for unit in &units {
        assert!(
            reference.contains(unit),
            "{unit:?} is not stamped as in the file read from its start"
        );
    }
    let first = units.iter().filter_map(|u| u.0).min().unwrap();
    assert!(first > 1_000_000_000, "the units are from behind the seek, {first}");
}
