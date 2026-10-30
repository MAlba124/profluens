//! pf-vpl's pure parts, no runtime. Input is a knob byte then the bytes.
//! `knob >> 2 & 3` 0 is the Annex B splitter and the encode plan. There `knob & 2` 0 splits the
//! bytes as one access unit, H.265 when `knob & 1`, against start codes found here, and
//! `knob & 2` 2 reads the bytes as 8 byte encoder query answers and holds `encode_plan` and
//! `rc_taken` to them.
//! 1 is geometry, `plan`, `plan_fixed`, `fit`, `out_info` and the raw copy's crop offsets.
//! 2 is time, the 90 kHz stamps both ways, decode times and `PtsMap` against a ledger.
//! 3 is counters, `rate`, `bitstream` offsets, the encoder's ring `Fifo` against a deque,
//! `key_and_prefix`, the surfaces kept over the runtime's suggestion, the pools sized from a
//! consumer's hold and the per element `Meter` against a model.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use libfuzzer_sys::fuzz_target;
use pf_vpl::dec::{Budget, OUT_BUDGET, PENDING_CAP, bitstream, out_surfaces};
use pf_vpl::emit::raw_copy;
use pf_vpl::enc::{Fifo, RATE_MAX_KBPS, RING, Settings, dts_of, key_and_prefix, rate};
use pf_vpl::ffi;
use pf_vpl::format::{Layout, raw_frame_len};
use pf_vpl::frames::Geometry;
use pf_vpl::group::Join;
use pf_vpl::nal::{
    AVC_IDR, AVC_SPS, HEVC_CRA, HEVC_IDR_N_LP, HEVC_IDR_W_RADL, HEVC_SPS, avc_type, has_sps,
    hevc_type, is_keyframe, nals,
};
use pf_vpl::park::Park;
use pf_vpl::probe::{EncQuery, LOOKAHEAD, VplCaps, codec_of, family_of};
use pf_vpl::pts::{PtsMap, Stamp, from_ts90, to_ts90};
use pf_vpl::stats::{Meter, PERIOD_NS};
use pf_vpl::vppcore::{OUT_SURFACES, Target, fit, out_info, out_pool, plan, plan_fixed};
use profluens_core::time::Timestamp;

const CODECS: [u32; 4] =
    [ffi::MFX_CODEC_AVC, ffi::MFX_CODEC_HEVC, ffi::MFX_CODEC_VP9, ffi::MFX_CODEC_AV1];
const RCS: [u16; 4] = [
    ffi::MFX_RATECONTROL_VBR,
    ffi::MFX_RATECONTROL_LA,
    ffi::MFX_RATECONTROL_LA_HRD,
    ffi::MFX_RATECONTROL_CBR,
];

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    match knob >> 2 & 3 {
        0 if knob & 2 == 0 => unit(rest, knob & 1 != 0),
        0 => encode_plan(rest),
        1 => geometry(rest),
        2 => time(rest),
        _ => counters(rest),
    }
});

/// Little endian fields off the front, zeros past the end.
struct Take<'a>(&'a [u8]);

impl Take<'_> {
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        let n = N.min(self.0.len());
        out[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        out
    }

    fn u8(&mut self) -> u8 {
        self.bytes::<1>()[0]
    }

    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.bytes())
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.bytes())
    }

    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.bytes())
    }

    fn done(&self) -> bool {
        self.0.is_empty()
    }
}

fn even(v: u32) -> u32 {
    (v & !1).max(2)
}

fn geometry(bytes: &[u8]) {
    let mut t = Take(bytes);
    let (w, h) = (u32::from(t.u16() % 9000), u32::from(t.u16() % 9000));
    let target = Target {
        max_width: u32::from(t.u16() % 9000),
        max_height: u32::from(t.u16() % 9000),
        depth: if t.u8() & 1 == 0 { 8 } else { 10 },
    };
    let input = if t.u8() & 1 == 0 { Layout::Nv12 } else { Layout::P010 };
    let (crop_x, crop_y) = (u32::from(t.u8() % 64), u32::from(t.u8() % 64));
    if w > 0 && h > 0 {
        let (bw, bh) = (u64::from(target.max_width.max(1)), u64::from(target.max_height.max(1)));
        let (a, b) = fit(u64::from(w), u64::from(h), bw, bh);
        assert!(a <= bw && b <= bh, "{w}x{h} fit to {bw}x{bh} is {a}x{b}, past the box");
        assert!(a == bw || b == bh, "{w}x{h} fit to {bw}x{bh} is {a}x{b}, short of it");
    }
    let ten = input.depth().min(target.depth) > 8;
    match plan(w, h, input, target) {
        None => assert!(w == 0 || h == 0, "{w}x{h} {target:?} unplanned"),
        Some(p) => {
            assert!(w > 0 && h > 0);
            assert!(p.width % 2 == 0 && p.height % 2 == 0 && p.width >= 2 && p.height >= 2);
            assert_eq!(p.place, (0, 0, p.width, p.height), "a plan has no bars");
            let bw = if target.max_width == 0 { w } else { target.max_width.min(w) };
            let bh = if target.max_height == 0 { h } else { target.max_height.min(h) };
            assert!(p.width <= bw.max(2) && p.height <= bh.max(2), "{w}x{h} {target:?} {p:?}");
            assert_eq!(p.out == Layout::P010, ten);
            assert_eq!(p.is_identity(w, h, input), p.width == w && p.height == h && p.out == input);
        }
    }
    match plan_fixed(w, h, input, target) {
        None => assert!(w == 0 || h == 0 || target.max_width == 0 || target.max_height == 0),
        Some(p) => {
            assert_eq!((p.width, p.height), (even(target.max_width), even(target.max_height)));
            let (x, y, pw, ph) = p.place;
            assert!(x % 2 == 0 && y % 2 == 0 && pw % 2 == 0 && ph % 2 == 0, "{p:?}");
            assert!(pw >= 2 && ph >= 2 && x + pw <= p.width && y + ph <= p.height, "{p:?}");
            assert_eq!(p.out == Layout::P010, ten);
            let info = out_info(p, (25, 1));
            assert!(u32::from(info.Width) >= p.width && info.Width.is_multiple_of(16));
            assert!(u32::from(info.Height) >= p.height && info.Height.is_multiple_of(16));
            let crop = (info.CropX, info.CropY, info.CropW, info.CropH);
            assert_eq!(crop, (x as u16, y as u16, pw as u16, ph as u16));
        }
    }
    if w == 0 || h == 0 {
        return;
    }
    let g = Geometry {
        coded_w: (w + crop_x).next_multiple_of(16),
        coded_h: (h + crop_y).next_multiple_of(16),
        disp_w: w,
        disp_h: h,
        crop_x,
        crop_y,
        fourcc: 0,
    };
    for layout in [Layout::Nv12, Layout::P010] {
        let c = raw_copy(layout, g);
        let bps = layout.bps();
        assert_eq!(c.y_len + 2 * c.c_len, raw_frame_len(layout, w, h), "{g:?} {layout:?}");
        assert_eq!(c.y_skip, crop_x as usize * bps);
        assert_eq!(c.c_skip, (crop_x & !1) as usize * bps, "Cb Cr pairs, two samples each");
        assert!(c.y_skip + c.y_row <= g.coded_w as usize * bps, "luma past the row {g:?}");
        assert!(c.c_skip + c.c_row <= g.coded_w as usize * bps, "chroma past the row {g:?}");
        assert!(c.c_top + c.ch <= g.coded_h.div_ceil(2), "chroma past the plane {g:?}");
        assert_eq!(c.c_row * c.ch as usize, 2 * c.c_len);
    }
}

fn time(bytes: &[u8]) {
    let mut t = Take(bytes);
    let pts = Timestamp::from_nanos(t.u64().min(Timestamp::MAX.0));
    let back = from_ts90(to_ts90(pts));
    assert!(back.0.abs_diff(pts.0) <= 11_112, "{pts:?} came back {back:?}");
    let ts = t.u64();
    let ns = from_ts90(ts);
    if ts == ffi::MFX_TIMESTAMP_UNKNOWN as u64 {
        assert_eq!(ns, Timestamp::NONE);
    } else {
        assert!(ns.is_some(), "{ts} is a time");
        if ts < u64::MAX / 100_000 {
            assert!(to_ts90(ns).abs_diff(ts) <= 1, "{ts} came back {}", to_ts90(ns));
        }
    }
    let dts = t.u64() as i64;
    assert_eq!(dts_of(dts), if dts < 0 { Timestamp::NONE } else { from_ts90(dts as u64) });
    // Each put pts is fresh, so an exact stamp back is one put and not handed out before.
    let (mut map, mut next, mut out) = (PtsMap::new(), 0u64, HashSet::new());
    let mut last: Option<Timestamp> = None;
    let check = |s: Stamp, what: &str, next: u64, out: &mut HashSet<u64>| {
        if s.duration.is_some() {
            assert!(s.pts.0 < next, "{what} made up {s:?}");
            assert!(out.insert(s.pts.0), "{what} handed {s:?} out twice");
        }
    };
    while !t.done() {
        let op = t.u8();
        let ts = u64::from(op >> 3);
        match op & 7 {
            0..=2 => {
                map.put(ts, Stamp { pts: Timestamp(next), duration: Timestamp(1), only: false });
                next += 1;
            }
            3 => {
                let s = map.take(ts);
                if s.duration.is_none() {
                    assert_eq!(s.pts, from_ts90(ts), "a stamp not put is rescaled");
                }
                check(s, "take", next, &mut out);
            }
            4 => {
                if let Some(s) = map.take_next(last) {
                    check(s, "take_next", next, &mut out);
                    assert!(last.is_none_or(|l| s.pts > l), "{s:?} not after {last:?}");
                    last = Some(s.pts);
                }
            }
            5 => {
                let s = map.take_or_next(ts, op & 0x80 != 0, last);
                check(s, "take_or_next", next, &mut out);
            }
            6 => last = None,
            _ => map.clear(),
        }
    }
}

fn counters(bytes: &[u8]) {
    let mut t = Take(bytes);
    let (b, m, hrd) = (t.u32(), t.u32(), t.u32());
    let r = rate(b, m, hrd);
    let mult = u32::from(r.multiplier);
    let bc = b.min(RATE_MAX_KBPS);
    assert!(mult >= 1);
    assert!(u32::from(r.target) * mult <= bc && bc - u32::from(r.target) * mult < mult, "{r:?}");
    assert!(r.max >= r.target && r.initial_kb <= r.buffer_kb, "{r:?}");
    assert_eq!(r.buffer_kb == 0, hrd == 0, "{r:?} from hrd {hrd}");
    let data = vec![0u8; usize::from(t.u8())];
    let offset = t.u32() % 600;
    let codec = t.u32();
    let bs = bitstream(&data, 5, offset, offset & 1 == 0, codec);
    assert_eq!(bs.DataOffset, offset.min(data.len() as u32));
    assert_eq!(bs.DataOffset + bs.DataLength, data.len() as u32);
    assert_eq!(bs.MaxLength, data.len() as u32);
    assert_eq!(bs.CodecId, codec);
    let slots = t.u32();
    let budget = t.u32();
    assert_eq!(out_surfaces(Budget::Raw, slots), PENDING_CAP as u32 + 2);
    assert_eq!(out_surfaces(Budget::Vpl(budget), slots), budget, "a surface consumer as set");
    let gpu = out_surfaces(Budget::Gpu(budget), slots);
    assert!(gpu >= budget && gpu >= slots.saturating_add(1), "one per slot that may carry one");
    assert_eq!(out_surfaces(Budget::Gpu(OUT_BUDGET), 0), OUT_BUDGET);
    let (hold, depth) = (t.u32(), t.u16());
    let pool = out_pool((hold > 0).then_some(hold), depth);
    assert!(pool >= OUT_SURFACES && pool >= hold.saturating_add(u32::from(depth)));
    assert_eq!(out_pool(None, depth), out_pool(Some(0), depth));
    let s = Settings {
        lookahead: t.u16(),
        bframes: t.u32(),
        async_depth: Some(depth),
        ..Settings::default()
    };
    let h = s.hold(depth);
    assert!(h > u32::from(s.lookahead) + u32::from(depth), "the lookahead and the depth fit");
    assert!(h <= u32::from(s.lookahead) + 7 + 2 + u32::from(depth), "the pyramid is capped");
    assert_eq!(Join::of_env(Some("none")), Join::None);
    assert_eq!(Join::of_env(std::str::from_utf8(&t.bytes::<4>()).ok()), Join::of_env(None));
    meter(&mut t);
    for k in 0..8u8 {
        let (idr, intra, sps) = (k & 1 != 0, k & 2 != 0, k & 4 != 0);
        let mut scans = 0;
        let (key, prefix) = key_and_prefix(idr, intra, || {
            scans += 1;
            sps
        });
        assert_eq!(scans, u32::from(idr || intra), "one scan, intra frames only");
        assert_eq!(key, idr || (intra && sps));
        assert_eq!(prefix, idr && !sps);
    }
    let (mut fifo, mut model) = (Fifo::default(), VecDeque::new());
    while !t.done() {
        if t.u8() & 1 == 0 {
            let want = (!fifo.is_full()).then(|| fifo.next()).flatten();
            assert_eq!(fifo.push(), want);
            if let Some(at) = want {
                assert!(!model.contains(&at), "slot {at} handed out twice");
                model.push_back(at);
            }
        } else {
            assert_eq!(fifo.front(), model.front().copied());
            assert_eq!(fifo.pop(), model.pop_front());
        }
        assert_eq!(fifo.len(), model.len());
        assert!(fifo.len() <= RING && fifo.is_full() == (model.len() == RING));
        assert_eq!(fifo.is_empty(), model.is_empty());
    }
}

/// The meter against a model, every event from the bytes.
fn meter(t: &mut Take<'_>) {
    let mut m = Meter::default();
    let (mut busy, mut pool, mut frames, mut latency, mut max, mut wait) = (0, 0, 0, 0, 0, 0);
    let mut since: Option<u64> = None;
    let mut done = 0u64;
    while !t.done() {
        let op = t.u8();
        let now = u64::from(t.u16());
        match op & 3 {
            0 => {
                if op & 4 == 0 {
                    m.park(Park::Busy);
                    busy += 1;
                } else {
                    m.park(Park::Pool);
                    pool += 1;
                }
            }
            1 => {
                let ready = op & 4 != 0;
                m.polled(ready, now);
                match (ready, since) {
                    (false, None) => since = Some(now),
                    (true, Some(s)) => {
                        wait += now.saturating_sub(s);
                        since = None;
                    }
                    _ => {}
                }
            }
            2 => {
                let submitted = u64::from(t.u16());
                m.done(submitted, now);
                let ns = now.saturating_sub(submitted);
                frames += 1;
                latency += ns;
                max = max.max(ns);
            }
            _ => {
                if m.due(now) {
                    done += 1;
                }
            }
        }
        assert_eq!((m.busy, m.pool, m.frames), (busy, pool, frames));
        assert_eq!((m.latency_ns, m.latency_max_ns, m.wait_ns), (latency, max, wait));
        assert!(m.latency_max_ns <= m.latency_ns);
        assert_eq!(m.mean_latency_ns(), latency.checked_div(frames).unwrap_or(0));
    }
    assert!(done <= 1 || PERIOD_NS <= u64::from(u16::MAX), "u16 clocks never pass a period twice");
}

fn say() -> bool {
    static SAY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}

/// Start codes left to right, none overlapping, H.264 B.1.1 and H.265 B.2.1.
fn starts(unit: &[u8]) -> Vec<usize> {
    let (mut out, mut i) = (Vec::new(), 0);
    while i + 3 <= unit.len() {
        if unit[i..i + 3] == [0, 0, 1] {
            out.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

fn unit(unit: &[u8], hevc: bool) {
    let got: Vec<&[u8]> = nals(unit).collect();
    let starts = starts(unit);
    assert_eq!(got.len(), starts.len(), "NALs other than the start codes");
    let base = unit.as_ptr() as usize;
    let mut prev = 0;
    for (i, (n, &s)) in got.iter().zip(&starts).enumerate() {
        let at = n.as_ptr() as usize - base;
        assert_eq!(at, s + 3, "NAL {i} not behind its start code");
        assert!(at >= prev && at + n.len() <= unit.len(), "NAL {i} out of order");
        let end = at + n.len();
        let next = starts.get(i + 1).copied().unwrap_or(unit.len());
        // A zero before the next start code is its leading zero_byte.
        let gap = &unit[end..next];
        assert!(gap.is_empty() || gap == [0], "NAL {i} cut {} bytes short", gap.len());
        assert!(!n.windows(3).any(|w| w == [0, 0, 1]), "NAL {i} holds a start code");
        prev = end;
    }
    let types: Vec<u8> =
        got.iter().filter_map(|n| if hevc { hevc_type(n) } else { avc_type(n) }).collect();
    assert_eq!(types.len(), got.iter().filter(|n| !n.is_empty()).count());
    let sps = if hevc { HEVC_SPS } else { AVC_SPS };
    assert_eq!(has_sps(unit, hevc), types.contains(&sps), "an SPS missed or made up");
    let key = types.iter().any(|&t| {
        if hevc { matches!(t, HEVC_IDR_W_RADL | HEVC_IDR_N_LP | HEVC_CRA) } else { t == AVC_IDR }
    });
    assert_eq!(is_keyframe(unit, hevc), key, "a keyframe missed or made up");
    if say() {
        eprintln!("vpl_nal hevc {hevc} nals {} types {types:?} key {key}", got.len());
    }
}

fn query(b: &[u8; 8]) -> EncQuery {
    let rc = RCS[usize::from(b[1] & 3)];
    let lookahead = [0, LOOKAHEAD, 1, 40][usize::from(b[1] >> 2 & 3)];
    EncQuery {
        codec: CODECS[usize::from(b[0] & 3)],
        low_power: b[0] & 4 != 0,
        rc,
        lookahead,
        ext_brc: b[0] & 8 != 0,
        status: i32::from(b[2] as i8),
        rc_out: if b[1] & 0x10 == 0 { rc } else { u16::from(b[3]) },
        lookahead_out: u16::from(b[4]),
        ext_brc_out: b[5] & 1 != 0,
        mbbrc_out: b[5] & 2 != 0,
        bpyramid_out: b[5] & 4 != 0,
        adaptive_i_out: b[5] & 8 != 0,
        adaptive_b_out: b[5] & 16 != 0,
    }
}

fn encode_plan(bytes: &[u8]) {
    let queries: Vec<EncQuery> = bytes.as_chunks::<8>().0.iter().map(query).collect();
    let caps = VplCaps {
        device: PathBuf::new(),
        vendor: String::new(),
        va_version: (1, 0),
        api: (2, 0),
        device_id: 0,
        integrated: false,
        decode_families: Vec::new(),
        decode_main10: false,
        decode_vp9_profile2: false,
        encode_families: Vec::new(),
        enc_queries: queries.clone(),
        fused: 0,
        import: false,
        tone_map: false,
    };
    let key = |q: &EncQuery| (q.codec, q.low_power, q.rc, q.lookahead, q.ext_brc);
    let mut keys: Vec<_> = queries.iter().map(key).collect();
    keys.sort_unstable();
    keys.dedup();
    let unique = keys.len() == queries.len();
    let mut plans = 0;
    for codec in CODECS {
        let family = family_of(codec).expect("a known codec");
        assert_eq!(codec_of(family), Some(codec), "{family} does not map back");
        for want in [false, true] {
            let got = caps.encode_plan(codec, want);
            plans += usize::from(got.is_some());
            let Some(p) = got else {
                if unique && !want {
                    let vbr = ffi::MFX_RATECONTROL_VBR;
                    assert!(!caps.rc_taken(codec, false, vbr) && !caps.rc_taken(codec, true, vbr));
                }
                continue;
            };
            let backing = queries.iter().find(|q| {
                key(q) == (codec, p.low_power, p.rc, q.lookahead, p.ext_brc)
                    && (q.lookahead == 0 || q.lookahead == LOOKAHEAD)
                    && p.lookahead == q.lookahead_out.min(q.lookahead)
                    && q.taken()
            });
            let Some(q) = backing else {
                panic!("a plan {p:?} no taken answer backs");
            };
            assert!(p.lookahead <= q.lookahead, "{p:?} from {q:?}");
            if !want {
                assert_eq!((p.rc, p.lookahead, p.ext_brc), (ffi::MFX_RATECONTROL_VBR, 0, false));
                assert!(caps.rc_taken(codec, p.low_power, p.rc), "a plan rc_taken refuses");
            }
            if p.ext_brc {
                assert!(want && p.rc == ffi::MFX_RATECONTROL_VBR, "{p:?}");
            }
        }
    }
    assert_eq!(codec_of("mpeg2"), None);
    if say() {
        eprintln!("vpl_nal queries {} unique {unique} plans {plans}", queries.len());
    }
    assert_eq!(family_of(0), None);
}
