//! The pf-vaapi AV1 header side, no display. Input is a knob byte then the bytes. `knob & 3`
//! picks the mode, 0 and 1 a low overhead OBU stream through `av1dec::Stream`, 2 the OBU framing
//! and bit reader against a reference writer, 3 a sequence header payload behind a `u16` LE
//! length then a frame header payload. Frames must stay in their ranges, their tiles inside the
//! unit, and cutting the stream at its temporal delimiters must not change them.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_vaapi::av1dec::header::{
    self, FrameHeader, FrameType, NUM_REF_FRAMES, SEGMENTATION_FEATURE_MAX, parse_frame_header,
    qindex, relative_dist, shear_valid,
};
use pf_vaapi::av1dec::obu::{self, Bits, leb128, next_obu, obus};
use pf_vaapi::av1dec::refs::RefFrame;
use pf_vaapi::av1dec::seq::{SequenceHeader, parse_sequence_header};
use pf_vaapi::av1dec::stream::{Frame, Stream, decoded_frames};

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    match knob & 3 {
        0 | 1 => stream(rest),
        2 => framing(rest),
        _ => headers(rest),
    }
});

/// What a frame came out as, its header and where its tiles lie.
type Seen = (Frame, Vec<(usize, usize)>);

fn run(tu: &[u8], s: &mut Stream, out: &mut Vec<Seen>) -> bool {
    let mut at = 0;
    loop {
        let before = at;
        let f = match s.next_frame(tu, &mut at) {
            Ok(Some(f)) => f,
            Ok(None) => return true,
            Err(_) => return false,
        };
        assert!(before < at && at <= tu.len(), "the cursor went from {before} to {at}");
        let seq = s.seq.expect("a frame without a sequence header");
        check_frame(&f.header, &seq);
        let mut tiles = Vec::new();
        if !f.header.show_existing_frame {
            tiles = check_tiles(&f.header, s, tu.len());
        }
        let refreshed = f.header.refresh_frame_flags;
        s.finish(&f);
        for (i, r) in s.refs.iter().enumerate() {
            if refreshed >> i & 1 == 1 && !f.header.show_existing_frame {
                assert!(r.valid && r.order_hint == f.header.order_hint, "slot {i} not refreshed");
            }
        }
        out.push((f, tiles));
    }
}

fn stream(b: &[u8]) {
    let mut whole = Vec::new();
    let clean = run(b, &mut Stream::new(), &mut whole);
    if say() {
        eprintln!("av1 frames {} clean {clean}", whole.len());
    }
    let _ = decoded_frames(b, false);
    if !clean {
        return;
    }
    // The same OBUs one temporal unit at a time. A frame may straddle a delimiter in a malformed
    // stream, which fails a unit alone, so only clean cuts compare.
    let mut cut = Vec::new();
    let mut all_clean = true;
    let mut s = Stream::new();
    let mut from = 0;
    let mut at = 0;
    while at < b.len() {
        let Ok((o, n)) = next_obu(&b[at..]) else {
            break;
        };
        if o.obu_type == obu::OBU_TEMPORAL_DELIMITER && at > from {
            let mut part = Vec::new();
            all_clean &= run(&b[from..at], &mut s, &mut part);
            cut.extend(part.into_iter().map(|(f, t)| (f, shift(&t, from))));
            from = at;
        }
        at += n;
    }
    let mut part = Vec::new();
    all_clean &= run(&b[from..], &mut s, &mut part);
    cut.extend(part.into_iter().map(|(f, t)| (f, shift(&t, from))));
    assert!(!all_clean || cut == whole, "cut at temporal units the frames differ");
}

fn shift(tiles: &[(usize, usize)], by: usize) -> Vec<(usize, usize)> {
    tiles.iter().map(|&(a, n)| (a + by, n)).collect()
}

fn check_frame(h: &FrameHeader, seq: &SequenceHeader) {
    assert!(h.frame_to_show_map_idx < NUM_REF_FRAMES as u8);
    if h.show_existing_frame {
        return;
    }
    assert!((1..=1 << 16).contains(&h.frame_width) && (1..=1 << 16).contains(&h.frame_height));
    assert!(
        h.upscaled_width >= h.frame_width,
        "{} upscaled to {}",
        h.frame_width,
        h.upscaled_width
    );
    assert!((8..=16).contains(&h.superres_denom) && (h.superres_denom > 8) == h.use_superres);
    assert!(h.render_width >= 1 && h.render_height >= 1);
    assert!(h.ref_frame_idx.iter().all(|&i| usize::from(i) < NUM_REF_FRAMES));
    assert!(h.primary_ref_frame <= header::PRIMARY_REF_NONE);
    assert!(h.interpolation_filter <= header::SWITCHABLE && h.tx_mode <= header::TX_MODE_SELECT);
    if seq.enable_order_hint {
        assert!(u32::from(h.order_hint) < 1 << seq.order_hint_bits);
        let m = 1i32 << (seq.order_hint_bits - 1);
        for &o in &h.order_hints {
            let d = relative_dist(seq, o, h.order_hint);
            assert!((-m..m).contains(&d), "relative_dist {d} past {m}");
        }
    } else {
        assert_eq!(h.order_hint, 0);
    }
    let t = &h.tile_info;
    let (cols, rows) = (usize::from(t.cols), usize::from(t.rows));
    assert!((1..=64).contains(&cols) && (1..=64).contains(&rows), "{cols}x{rows} tiles");
    assert!((1..=4).contains(&t.size_bytes));
    assert!(usize::from(t.context_update_tile_id) < cols * rows);
    assert!(cols <= 1 << t.cols_log2 && rows <= 1 << t.rows_log2);
    let sb = if seq.use_128x128_superblock { 128 } else { 64 };
    let sum: u32 = t.width_in_sbs[..cols].iter().map(|&w| u32::from(w)).sum();
    assert_eq!(sum, h.frame_width.div_ceil(sb), "tile columns {:?}", &t.width_in_sbs[..cols]);
    assert!(t.width_in_sbs[..cols].iter().all(|&w| w >= 1));
    let q = &h.quant;
    for d in [q.delta_q_y_dc, q.delta_q_u_dc, q.delta_q_u_ac, q.delta_q_v_dc, q.delta_q_v_ac] {
        assert!((-64..=63).contains(&d));
    }
    assert!(q.qm_y <= 15 && q.qm_u <= 15 && q.qm_v <= 15);
    for i in 0..header::MAX_SEGMENTS {
        assert!((0..=255).contains(&qindex(h, i)));
        for (j, &v) in h.seg.feature_data[i].iter().enumerate() {
            let m = SEGMENTATION_FEATURE_MAX[j];
            assert!((-m..=m).contains(&v), "segment {i} feature {j} {v}");
        }
    }
    assert!(h.lf.level.iter().all(|&l| l <= 63) && h.lf.sharpness <= 7);
    assert!(h.lf.ref_deltas.iter().chain(&h.lf.mode_deltas).all(|&d| (-64..=63).contains(&d)));
    assert!(h.cdef.bits <= 3 && h.cdef.damping_minus_3 <= 3);
    assert!(h.lr.frame_type.iter().all(|&t| t <= header::RESTORE_SWITCHABLE));
    for rf in 1..8 {
        assert_eq!(h.gm.invalid[rf], !shear_valid(&h.gm.params[rf]), "gm {rf} validity");
        assert!(h.gm.gm_type[rf] <= header::AFFINE);
    }
    if h.is_intra() {
        assert!(h.gm.gm_type.iter().all(|&g| g == header::IDENTITY));
    }
    let g = &h.film_grain;
    assert!(g.num_y_points <= 14 && g.num_cb_points <= 10 && g.num_cr_points <= 10);
    assert!(g.ar_coeff_lag <= 3 && g.grain_scaling_minus_8 <= 3 && g.ar_coeff_shift_minus_6 <= 3);
    if h.frame_type == FrameType::Key && h.show_frame {
        assert_eq!(h.refresh_frame_flags, header::ALL_FRAMES);
    }
}

fn check_tiles(h: &FrameHeader, s: &Stream, len: usize) -> Vec<(usize, usize)> {
    let n = usize::from(h.tile_info.cols) * usize::from(h.tile_info.rows);
    assert_eq!(s.tiles.len(), n, "a frame of {} tiles, {n} in its tile info", s.tiles.len());
    let mut next = 0u16;
    let mut out = Vec::with_capacity(n);
    for g in &s.groups {
        assert_eq!(g.tg_start, next, "tile groups out of order");
        assert!(g.tg_start <= g.tg_end && g.data_start <= g.data_end && g.data_end <= len);
        assert_eq!(g.count, usize::from(g.tg_end - g.tg_start) + 1);
        next = g.tg_end + 1;
        for (k, t) in s.tiles[g.first..g.first + g.count].iter().enumerate() {
            let num = usize::from(g.tg_start) + k;
            let cols = usize::from(h.tile_info.cols);
            assert_eq!((usize::from(t.row), usize::from(t.col)), (num / cols, num % cols));
            let (at, size) = (g.data_start + t.offset as usize, t.size as usize);
            assert!(size >= 1 && at + size <= g.data_end, "a tile past its group");
            out.push((at, size));
        }
    }
    assert_eq!(usize::from(next), n, "the tile groups miss tiles");
    for w in out.windows(2) {
        assert!(w[0].0 + w[0].1 <= w[1].0, "tiles overlap");
    }
    out
}

fn headers(b: &[u8]) {
    let Some((len, rest)) = b.split_first_chunk::<2>() else {
        return;
    };
    let n = usize::from(u16::from_le_bytes(*len)).min(rest.len());
    let (sp, fp) = rest.split_at(n);
    let Ok(seq) = parse_sequence_header(sp) else {
        return;
    };
    assert!(seq.seq_profile <= 2 && seq.operating_points_cnt_minus_1 <= 31);
    assert!(matches!(seq.color.bit_depth, 8 | 10 | 12));
    assert!(seq.frame_width_bits <= 16 && seq.frame_height_bits <= 16);
    assert!(seq.max_frame_width <= 1 << 16 && seq.max_frame_height <= 1 << 16);
    assert!(seq.order_hint_bits <= 8 && seq.enable_order_hint == (seq.order_hint_bits > 0));
    assert!(seq.same_sequence(&seq));
    let _ = (seq.operating_point_idc(), seq.id_len(), seq.color.num_planes());
    // A sequence header reads the same with bytes behind it.
    let mut longer = sp.to_vec();
    longer.extend_from_slice(fp);
    assert_eq!(parse_sequence_header(&longer).ok(), Some(seq));
    let refs = [RefFrame::EMPTY; NUM_REF_FRAMES];
    let Ok(h) = parse_frame_header(fp, &seq, &refs, 0, 0) else {
        return;
    };
    check_frame(&h, &seq);
    assert!(h.header_bits <= fp.len() * 8 && h.header_bytes() <= fp.len());
    let mut longer = fp.to_vec();
    longer.extend_from_slice(&[0xA5; 16]);
    assert_eq!(parse_frame_header(&longer, &seq, &refs, 0, 0), Ok(h), "bytes past it changed it");
}

/// OBUs written by a reference writer split back the same, and the bit reader reads back what a
/// reference writer put.
fn framing(b: &[u8]) {
    for o in obus(b) {
        let Ok(o) = o else {
            break;
        };
        let at = o.payload.as_ptr() as usize - b.as_ptr() as usize;
        assert!(at + o.payload.len() <= b.len() && o.obu_type <= 15);
        assert!(o.extension || (o.temporal_id, o.spatial_id) == (0, 0));
        let _ = obu::in_operating_point(&o, 0x0101);
    }
    let mut stream = Vec::new();
    let mut want = Vec::new();
    for (i, c) in b.chunks(4).enumerate() {
        let ty = c[0] >> 3 & 15;
        let ext = c[0] & 4 != 0;
        let payload = &c[1..];
        let mut header = vec![ty << 3 | u8::from(ext) << 2 | 2];
        if ext {
            header.push((i as u8 & 7) << 5 | (c[0] & 3) << 3);
        }
        stream.extend_from_slice(&header);
        put_leb128(&mut stream, payload.len() as u64);
        stream.extend_from_slice(payload);
        want.push((ty, payload));
    }
    let got: Vec<_> = obus(&stream).map(|o| o.expect("a written OBU refused")).collect();
    assert_eq!(got.len(), want.len());
    for (o, (ty, p)) in got.iter().zip(&want) {
        assert!(o.obu_type == *ty && o.payload == *p);
    }
    for c in b.chunks(5) {
        let v = c.iter().fold(0u64, |v, &x| v << 8 | u64::from(x)) as u32;
        let mut e = Vec::new();
        put_leb128(&mut e, u64::from(v));
        e.push(0xFF);
        assert_eq!(leb128(&e), Ok((v, e.len() - 1)), "leb128 of {v}");
    }
    bits(b);
}

fn put_leb128(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    F(u32, u32),
    Su(u32, i32),
    Ns(u32, u32),
    Uvlc(u32),
}

fn bits(b: &[u8]) {
    let mut ops = Vec::new();
    for c in b.chunks(5) {
        let mut v = [0u8; 4];
        v[..c.len() - 1].copy_from_slice(&c[1..]);
        let v = u32::from_le_bytes(v);
        let n = u32::from(c[0] >> 2) % 33;
        ops.push(match c[0] & 3 {
            0 => Op::F(n, if n == 32 { v } else { v & ((1 << n) - 1) }),
            1 => {
                let n = n.clamp(1, 16);
                let half = 1i32 << (n - 1);
                Op::Su(n, (v as i32).rem_euclid(2 * half) - half)
            }
            2 => {
                let n = (v >> 16).max(1);
                Op::Ns(n, (v & 0xFFFF) % n)
            }
            _ => Op::Uvlc(v.min(u32::MAX - 1) >> (n % 32)),
        });
    }
    let mut w = Writer::default();
    for &op in &ops {
        match op {
            Op::F(n, v) => w.put(u64::from(v), n),
            Op::Su(n, v) => w.put(v as u64 & ((1 << n) - 1), n),
            Op::Ns(n, v) => {
                // ns(n) of section 4.10.7.
                let width = 32 - n.leading_zeros();
                let m = (1u64 << width) - u64::from(n);
                if u64::from(v) < m {
                    w.put(u64::from(v), width - 1);
                } else {
                    let x = u64::from(v) + m;
                    w.put(x >> 1, width - 1);
                    w.put(x & 1, 1);
                }
            }
            Op::Uvlc(v) => {
                let x = u64::from(v) + 1;
                let len = 64 - x.leading_zeros();
                w.put(0, len - 1);
                w.put(x, len);
            }
        }
    }
    let total = w.bits;
    let mut r = Bits::new(&w.out);
    for &op in &ops {
        match op {
            Op::F(n, v) => assert_eq!(r.f(n), Ok(v), "f({n})"),
            Op::Su(n, v) => assert_eq!(r.su(n), Ok(v), "su({n})"),
            Op::Ns(n, v) => assert_eq!(r.ns(n), Ok(v), "ns({n})"),
            Op::Uvlc(v) => assert_eq!(r.uvlc(), Ok(v), "uvlc"),
        }
    }
    assert_eq!(r.pos(), total);
    assert_eq!(r.f(8).is_err(), total + 8 > w.out.len() * 8, "a read past the end");
}

#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    bits: usize,
}

impl Writer {
    fn put(&mut self, v: u64, n: u32) {
        for i in (0..n).rev() {
            if self.bits.is_multiple_of(8) {
                self.out.push(0);
            }
            let bit = (v >> i & 1) as u8;
            *self.out.last_mut().expect("pushed") |= bit << (7 - self.bits % 8);
            self.bits += 1;
        }
    }
}

fn say() -> bool {
    static SAY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}
