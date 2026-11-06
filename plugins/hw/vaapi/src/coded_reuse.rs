//! An encoder session codes every picture through the one coded buffer it made, the bytes those
//! pictures carry are the ones a new buffer each picture gives, straight into a pool slot or
//! staged behind a dry pool, across a picture size change. Skips without VA-API encode.

// Test code. The allocation ban is for element hot paths.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use profluens_core::element::Element;
use profluens_core::format::ValueDesc;
use profluens_core::id::SinkPad;
use profluens_core::time::Timestamp;

use crate::enc::FRESH_CODED;
use crate::stats::Stats;
use crate::{VaapiH264Enc, VaapiH265Enc};

const FRAMES: u64 = 300;
/// The picture size changes here, a session of its own from this picture on.
const SWITCH: u64 = 200;
const SIZES: [(u32, u32); 2] = [(320, 240), (352, 288)];
/// The pictures a dry pool holds on to, the encoder stages what it codes meanwhile.
const HELD: std::ops::Range<u64> = 20..31;
const DRY_SLOTS: u32 = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pool {
    Free,
    /// [`DRY_SLOTS`] slots, the pictures out over [`HELD`] kept in them.
    Dry,
}

struct Run {
    out: Vec<(Timestamp, Vec<u8>)>,
    coded_bufs: u64,
}

fn raw(w: u32, h: u32) -> [(&'static str, ValueDesc); 4] {
    [
        ("width", ValueDesc::Int(w.into())),
        ("height", ValueDesc::Int(h.into())),
        ("pixfmt", ValueDesc::Id("nv12")),
        ("fps", ValueDesc::Rat(25, 1)),
    ]
}

/// A moving gradient with noise, so pictures code to a few KiB each.
fn picture(w: u32, h: u32, i: u64) -> Vec<u8> {
    let mut x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..(w * h * 3 / 2) as usize)
        .map(|p| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ((p as u64 % w as u64 + i * 3) % 200 + (x & 15) + 16) as u8
        })
        .collect()
}

fn run(el: impl Element + 'static, stats: Arc<Stats>, fresh: bool, pool: Pool) -> Run {
    FRESH_CODED.set(fresh);
    let slots = if pool == Pool::Dry { DRY_SLOTS } else { 16 };
    let mut hx = pf_harness::Harness::with_pool(el, 256 * 1024, slots);
    let (w, h) = SIZES[0];
    hx.fix_sink_format(SinkPad(0), "video/raw", &raw(w, h));
    let mut out = Vec::new();
    let mut take = |bufs: Vec<profluens_core::buffer::Buffer>| {
        let n = bufs.len();
        out.extend(bufs.into_iter().map(|b| (b.pts, b.memory.data().to_vec())));
        n
    };
    for i in 0..FRAMES {
        let (w, h) = SIZES[usize::from(i >= SWITCH)];
        if i == SWITCH {
            hx.change_sink_format(SinkPad(0), "video/raw", &raw(w, h)).expect("the new size");
        }
        let mut b = hx.alloc(&picture(w, h, i));
        b.pts = Timestamp::from_nanos(i * 40_000_000);
        hx.push(b).expect("pushed");
        if pool == Pool::Dry && HELD.contains(&i) {
            continue;
        }
        let n = take(hx.drain_outputs());
        if pool == Pool::Dry && i == HELD.end {
            // a slot each held picture, the rest of the pictures coded meanwhile were staged
            assert!(n <= DRY_SLOTS as usize, "{n} pictures out of a pool of {DRY_SLOTS}");
        }
    }
    take(hx.eos().expect("the end"));
    FRESH_CODED.set(false);
    let coded_bufs = stats.snapshot().coded_bufs;
    Run { out, coded_bufs }
}

fn reuses<E: Element + 'static>(name: &str, make: fn() -> E, stats: fn(&E) -> Arc<Stats>) {
    let coded = |fresh, pool| {
        let el = make();
        let st = stats(&el);
        run(el, st, fresh, pool)
    };
    let reused = coded(false, Pool::Free);
    assert_eq!(reused.out.len() as u64, FRAMES, "{name} every picture");
    assert_eq!(reused.coded_bufs, 2, "{name} one coded buffer a session, two sessions");
    let fresh = coded(true, Pool::Free);
    assert_eq!(fresh.coded_bufs, FRAMES, "{name} the old path, a buffer each picture");
    assert!(reused.out == fresh.out, "{name} the reused buffer codes the bytes a new one codes");
    let dry = coded(false, Pool::Dry);
    assert_eq!(dry.coded_bufs, 2, "{name} staging takes no buffer of its own");
    assert!(dry.out == fresh.out, "{name} staged behind a dry pool, the same bytes in order");
}

fn device(test: &str, family: &str) -> bool {
    let ok = crate::probe().is_some_and(|c| c.supports_encode(family));
    if !ok {
        eprintln!("skip {test}: no VA-API {family} encode");
    }
    ok
}

#[test]
fn h264enc_codes_every_picture_through_one_coded_buffer() {
    if device("h264enc_coded_reuse", "h264/annexb") {
        reuses("vaapih264enc", VaapiH264Enc::new, VaapiH264Enc::stats);
    }
}

#[test]
fn h265enc_codes_every_picture_through_one_coded_buffer() {
    if device("h265enc_coded_reuse", "h265/annexb") {
        reuses("vaapih265enc", VaapiH265Enc::new, VaapiH265Enc::stats);
    }
}
