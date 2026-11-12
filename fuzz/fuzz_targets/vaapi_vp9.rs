//! The pf-vaapi VP9 header side, no display. Input is a knob byte then chunks, each behind a
//! `u16` LE length as the demuxer hands them over. A chunk is split as a superframe, each frame's
//! uncompressed header parsed against the slots before it and folded into the stream state, as
//! `vaapivp9dec` does. Headers must stay in their ranges and not depend on the bytes past them,
//! and a superframe must split back into the frames it was built from.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_bitstream::vp9::parse::superframe::{split_superframe, split_superframe_into};
use pf_bitstream::vp9::parse::{
    ColorSpace, FrameType, MAX_SEGMENTS, RefFrameState, SEGMENTATION_FEATURE_BITS, Vp9FrameHeader,
    parse_uncompressed_header, parse_uncompressed_header_with_refs,
};
use pf_bitstream::vp9::StreamState;

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let mut dims = [(0u32, 0u32); 8];
    let mut state = StreamState::new();
    let mut ranges = vec![(7, 3)];
    for chunk in records(rest) {
        split_superframe_into(chunk, &mut ranges);
        check_split(chunk, &ranges);
        for &(s, e) in &ranges {
            frame(&chunk[s..e], &mut dims, &mut state);
        }
        if knob & 1 != 0 {
            rebuild(&ranges, chunk, knob >> 1 & 3);
        }
    }
});

fn records(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let (len, rest) = match data.split_first_chunk::<2>() {
            Some((len, rest)) => (usize::from(u16::from_le_bytes(*len)), rest),
            None if data.is_empty() => return None,
            None => (data.len(), std::mem::take(&mut data)),
        };
        let (rec, tail) = rest.split_at(len.min(rest.len()));
        data = tail;
        Some(rec)
    })
}

fn check_split(chunk: &[u8], ranges: &[(usize, usize)]) {
    assert_eq!(split_superframe(chunk), ranges, "split_superframe_into kept a stale range");
    assert_eq!(ranges.is_empty(), chunk.is_empty());
    assert!(ranges.len() <= 8);
    let mut at = 0;
    for &(s, e) in ranges {
        assert!(s == at && s <= e && e <= chunk.len(), "{ranges:?} of {}", chunk.len());
        at = e;
    }
    if ranges.len() > 1 || ranges.first().is_some_and(|r| r.1 < chunk.len()) {
        // An index was taken, it sits behind the frames with its marker at both ends.
        let m = chunk[chunk.len() - 1];
        assert_eq!(m >> 5, 0b110);
        let index = 2 + (usize::from(m & 7) + 1) * (usize::from(m >> 3 & 3) + 1);
        assert!(at + index <= chunk.len() && chunk[chunk.len() - index] == m);
    }
}

/// The frames of a split written back as a superframe with `szb + 1` byte sizes split the same.
fn rebuild(ranges: &[(usize, usize)], chunk: &[u8], szb: u8) {
    let sizes: Vec<usize> = ranges.iter().map(|(s, e)| e - s).collect();
    let bytes = usize::from(szb) + 1;
    if sizes.is_empty() || sizes.iter().any(|&n| n >> (8 * bytes) != 0) {
        return;
    }
    let mut out: Vec<u8> = ranges.iter().flat_map(|&(s, e)| &chunk[s..e]).copied().collect();
    let m = 0b110 << 5 | szb << 3 | (sizes.len() - 1) as u8;
    out.push(m);
    for n in &sizes {
        out.extend_from_slice(&n.to_le_bytes()[..bytes]);
    }
    out.push(m);
    let got = split_superframe(&out);
    assert_eq!(got, ranges, "a written superframe split otherwise");
}

fn frame(f: &[u8], dims: &mut [(u32, u32); 8], state: &mut StreamState) {
    let refs = RefFrameState { ref_dims: &dims[..], color_config: state.color };
    let Ok(h) = parse_uncompressed_header_with_refs(f, Some(refs)) else {
        return;
    };
    check_header(&h, f.len());
    // Bytes past the header and the compressed header do not change it.
    if !h.show_existing_frame {
        let n = h.uncompressed_header_size_bytes;
        let again = parse_uncompressed_header_with_refs(&f[..n], Some(refs));
        assert_eq!(again, Ok(h), "the header alone parsed otherwise");
        let mut longer = f.to_vec();
        longer.extend_from_slice(&[0x5A; 9]);
        assert_eq!(parse_uncompressed_header_with_refs(&longer, Some(refs)), Ok(h));
    }
    let intra = h.frame_type == FrameType::KeyFrame || h.intra_only;
    let alone = parse_uncompressed_header(f);
    if intra || h.show_existing_frame {
        assert_eq!(alone, Ok(h), "an intra header read otherwise without the slots");
    }
    if h.show_existing_frame {
        return;
    }
    state.fold(&h);
    if intra {
        assert_eq!(state.color, h.color_config);
    }
    for seg in 0..MAX_SEGMENTS {
        assert!((0..=255).contains(&state.qindex(&h, seg)));
        let levels = state.filter_levels(&h, seg);
        assert!(levels.iter().flatten().all(|&l| l <= 63), "levels {levels:?}");
        if h.loop_filter.level == 0 {
            assert_eq!(levels, [[0; 2]; 4]);
        }
    }
    for (i, d) in dims.iter_mut().enumerate() {
        if h.refresh_frame_flags >> i & 1 == 1 {
            *d = (h.frame_width, h.frame_height);
        }
    }
}

fn check_header(h: &Vp9FrameHeader, len: usize) {
    assert!(h.profile <= 3);
    if h.show_existing_frame {
        assert!(h.frame_to_show_map_idx.is_some_and(|i| i < 8));
        return;
    }
    let cc = &h.color_config;
    let intra = h.frame_type == FrameType::KeyFrame || h.intra_only;
    // An inter frame inherits the colour of the last intra one, whatever profile it states.
    match h.profile {
        _ if !intra => assert!(matches!(cc.bit_depth, 8 | 10 | 12)),
        0 | 1 => assert_eq!(cc.bit_depth, 8),
        _ => assert!(matches!(cc.bit_depth, 10 | 12)),
    }
    if intra && h.profile & 1 == 0 {
        assert!(cc.subsampling_x && cc.subsampling_y && cc.color_space != ColorSpace::Rgb);
    }
    if cc.color_space == ColorSpace::Rgb {
        assert!(cc.color_range_full && !cc.subsampling_x && !cc.subsampling_y);
    }
    if h.frame_type == FrameType::KeyFrame {
        assert_eq!(h.refresh_frame_flags, 0xFF);
        assert!(h.ref_frame_idx.is_none() && !h.intra_only);
    }
    assert!(h.intra_only == (h.frame_type == FrameType::NonKeyFrame && intra));
    assert!(intra == h.ref_frame_idx.is_none());
    assert!(h.ref_frame_idx.is_none_or(|r| r.iter().all(|&i| i < 8)));
    assert!(h.interpolation_filter <= 4 && h.reset_frame_context <= 3);
    assert!(h.frame_context_idx <= 3 && (!intra || h.frame_context_idx == 0));
    assert!(h.uncompressed_header_size_bytes >= 1 && h.uncompressed_header_size_bytes <= len);
    // A width of 0 only comes from a slot never filled, the rest are 16-bit sizes plus one.
    assert!(h.frame_width <= 1 << 16 && h.frame_height <= 1 << 16);
    assert!((1..=1 << 16).contains(&h.render_width) || h.render_width == 0);
    let sb64 = h.frame_width.div_ceil(8).div_ceil(8);
    let (mut lo, mut hi) = (0u8, 1u8);
    while 64 << lo < sb64 {
        lo += 1;
    }
    while sb64 >> hi >= 4 {
        hi += 1;
    }
    let t = &h.tile_info;
    assert!(
        t.tile_cols_log2 >= lo && t.tile_cols_log2 <= (hi - 1).max(lo) && t.tile_cols_log2 <= 6
    );
    assert!(t.tile_rows_log2 <= 2);
    let lf = &h.loop_filter;
    assert!(lf.level <= 63 && lf.sharpness <= 7);
    let deltas = lf.ref_deltas.iter().chain(&lf.mode_deltas).flatten();
    assert!(deltas.clone().all(|d| (-63..=63).contains(d)));
    assert!(lf.delta_enabled || (!lf.delta_update && deltas.count() == 0));
    let q = &h.quantization;
    for d in [q.delta_q_y_dc, q.delta_q_uv_dc, q.delta_q_uv_ac] {
        assert!((-15..=15).contains(&d));
    }
    let zero = q.delta_q_y_dc == 0 && q.delta_q_uv_dc == 0 && q.delta_q_uv_ac == 0;
    assert_eq!(q.lossless, q.base_q_idx == 0 && zero);
    let seg = &h.segmentation;
    for (data, enabled) in seg.feature_data.iter().zip(&seg.feature_enabled) {
        for ((&v, &on), &bits) in data.iter().zip(enabled).zip(&SEGMENTATION_FEATURE_BITS) {
            let m = (1i16 << bits) - 1;
            assert!((-m..=m).contains(&v) && (on || v == 0));
        }
    }
    assert!(seg.enabled || !(seg.update_map || seg.update_data));
}
