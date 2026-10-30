//! The untrusted-input gate for `flvdemux`, seeded corruptions whole and in pieces, and a file
//! with broken tags read on past them.

#![allow(clippy::disallowed_methods)] // test fixtures

mod common;

use common::{MS, fixture, of, packets, run, tags};
use pf_flv::FlvDemux;
use pf_sim::corrupt::harness::{feed_bytes, pool_allowance};
use pf_sim::corrupt::{
    Case, Chunking, Site, Tracking, Violation, check_heap, check_output, gate, heap_mark, mutate,
    noise, prefix_then_garbage, sweep_gate,
};
use pf_sim::prng::Prng;
use profluens_core::buffer::BufferFlags;

#[global_allocator]
static HEAP: Tracking = Tracking;

static H264_AAC: &[u8] = include_bytes!("fixtures/h264_aac.flv");
static HEVC_AAC: &[u8] = include_bytes!("fixtures/hevc_aac.flv");
static H264_MP3: &[u8] = include_bytes!("fixtures/h264_mp3.flv");
static VP9_PCM: &[u8] = include_bytes!("fixtures/vp9_pcm.flv");

struct Base {
    name: &'static str,
    data: &'static [u8],
    sites: Vec<Site>,
}

/// Each tag's start, its size and timestamp fields, its back pointer and body head.
fn sites(data: &[u8]) -> Vec<Site> {
    let mut sites = vec![Site::be(5, 4)];
    for (at, _) in tags(data) {
        sites.extend([Site::boundary(at), Site::be(at + 1, 3), Site::be(at + 4, 4)]);
        sites.extend([Site::be(at.saturating_sub(4), 4), Site::be(at + 11, 1)]);
        sites.push(Site::be(at + 13, 3));
    }
    sites
}

fn bases() -> Vec<Base> {
    [("h264_aac", H264_AAC), ("hevc_aac", HEVC_AAC), ("h264_mp3", H264_MP3), ("vp9_pcm", VP9_PCM)]
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

/// The carry and the parked output hold a tag each, a resync window at most a tag.
const DEMUX_HEAP: usize = 2 << 20;

fn drive(case: &Case, bases: &[Base]) -> Result<(), Violation> {
    let mark = heap_mark();
    let pristine = bases.iter().find(|b| b.name == case.base).map_or(bases[0].data, |b| b.data);
    let head = match case.knob >> 4 & 1 {
        0 => case.data.clone(),
        _ => pristine.to_vec(),
    };
    let flush = (case.knob >> 5 & 3 == 0).then_some((case.knob >> 8) as usize % 16);
    let fed = feed_bytes(FlvDemux::new(head), case, flush, case.data.len() + 64)?;
    // Annex B of one or two octet lengths grows a NAL by up to three, the heads go again.
    check_output(fed.out_bytes, 3 * case.data.len() + (64 << 10))?;
    check_heap(mark, DEMUX_HEAP + 8 * case.data.len() + pool_allowance(case.knob))
}

#[test]
fn the_element_survives_corrupt_streams() {
    let bases = bases();
    gate("flvdemux", 1_500, &|prng| case(prng, &bases), &|case| drive(case, &bases));
}

#[test]
fn the_element_survives_every_truncation_and_extreme_field() {
    let bases = bases();
    for base in &bases {
        sweep_gate("flvdemux sweep", base.name, base.data, &base.sites, 120, &|case| {
            drive(case, &bases)
        });
    }
}

#[test]
fn broken_tags_lose_themselves_and_nothing_else() {
    let data = fixture("h264_aac");
    let rows = packets("h264_aac");
    let tags = tags(&data);
    let (smashed, resized, junk_at) = (tags[20].0, tags[41].0, tags[60].0);
    let mut damaged = data.clone();
    damaged[smashed..smashed + 11].fill(0xEE);
    // A size that runs into the next tag.
    damaged[resized + 3] = damaged[resized + 3].wrapping_add(40);
    damaged.splice(junk_at..junk_at, [0x09, 0, 0, 0x30, 0, 0, 0, 0, 0, 0, 0, 0x17, 0xEE, 0xAA]);
    let lost = [smashed as u64, resized as u64];
    for chunk in [1usize, 777, 1 << 20] {
        let out = run(&damaged, &data, chunk);
        for (n, (_, _, rec)) in out.streams.iter().enumerate() {
            let want: Vec<(Option<u64>, usize)> = of(&rows, n)
                .iter()
                .filter(|p| !lost.contains(&p.pos))
                .map(|p| (Some(p.pts as u64 * MS), p.size))
                .collect();
            let got: Vec<(Option<u64>, usize)> = rec.frames().iter().map(|u| (u.0, u.1)).collect();
            assert_eq!(got, want, "stream {n} in pieces of {chunk}");
        }
        let video = &out.streams[0].2;
        assert!(video.units.iter().filter(|u| u.2 & BufferFlags::HEADER.0 != 0).count() == 1);
    }
}
