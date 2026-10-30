//! pf-hdr parsers round trip and stay in range. Input is a knob byte, its low nibble the
//! parser and its high nibble that parser's mode, then the bytes.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_hdr::bits::{BitWriter, Bits};
use pf_hdr::carry::{Carry, ENTRIES};
use pf_hdr::color::{
    ChromaSite, ContentLight, HdrStatic, Mastering, Matrix, Primaries, Range, StreamColor,
    TransferFn,
};
use pf_hdr::dovi::{DoviConfig, DoviError, RpuForm, RpuHeader};
use pf_hdr::h26x::{self, Codec, Scan};
use pf_hdr::hdr10plus::{Hdr10Plus, Hdr10PlusError, MAX_PAYLOAD, PeakGrid};
use pf_hdr::side::{self, FrameMeta, Kind, SIDE_CAP, SideError, Writer};
use pf_hdr::t35::{self, T35, T35Kind};
use pf_hdr::{av1, vp9};
use profluens_core::format::ValueDesc;

const HDR10PLUS_CODES: [u8; 6] = [0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
const DOVI_CODES: [u8; 7] = [0xB5, 0x00, 0x3B, 0x00, 0x00, 0x08, 0x00];
/// The fixed EMDF head of `dovi.rs`.
const EMDF_MAGIC: u32 = 0x01be_6841;

type ScanOut = (Option<Mastering>, Option<ContentLight>, Vec<u8>);

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let mode = knob >> 4;
    match knob & 0x0F {
        0 => hdr10plus(rest),
        1 => t35_message(rest),
        2 => dovi_record(rest),
        3 => rpu(rest),
        4 => sei(rest),
        5 => annexb(rest, mode),
        6 => obus(rest),
        7 => vp9_colour(rest),
        8 => vui(rest),
        9 => side_records(rest, mode),
        10 => carry(rest),
        _ => static_colour(rest),
    }
});

fn hdr10plus(p: &[u8]) {
    let h = match Hdr10Plus::parse(p) {
        Ok(h) => h,
        Err(Hdr10PlusError::TooLarge(n)) => return assert!(n == p.len() && n > MAX_PAYLOAD),
        Err(Hdr10PlusError::Windows(n)) => return assert_eq!(n, 0),
        Err(Hdr10PlusError::Grid { rows, cols }) => {
            return assert!(!(2..=25).contains(&rows) || !(2..=25).contains(&cols));
        }
        Err(Hdr10PlusError::Short) => return,
    };
    check_hdr10plus(&h);
    let w = write_hdr10plus(&h);
    assert!(w.len() <= p.len(), "a rewrite longer than the payload");
    assert_eq!(Hdr10Plus::parse(&w), Ok(h), "rewrite reads back different");
    assert_eq!(Hdr10Plus::parse(&p[..w.len()]), Ok(h), "bytes past the syntax changed it");
    assert_eq!(Hdr10Plus::parse(&p[..w.len() - 1]), Err(Hdr10PlusError::Short));
}

fn check_hdr10plus(h: &Hdr10Plus) {
    let n = usize::from(h.num_windows);
    assert!((1..=3).contains(&n));
    assert!(h.targeted_system_display_maximum_luminance < 1 << 27);
    assert!(h.windows[0].shape.is_none());
    for (i, w) in h.windows.iter().enumerate() {
        if i >= n {
            assert_eq!(*w, Default::default(), "a window past num_windows");
            continue;
        }
        assert_eq!(w.shape.is_some(), i > 0);
        assert!(w.maxscl.iter().all(|&v| v < 1 << 17) && w.average_maxrgb < 1 << 17);
        let pn = usize::from(w.num_distribution_maxrgb_percentiles);
        assert!(pn <= 15 && w.fraction_bright_pixels < 1024);
        assert!(w.distribution_maxrgb[..pn].iter().all(|&(p, v)| p < 128 && v < 1 << 17));
        assert!(w.distribution_maxrgb[pn..].iter().all(|&e| e == (0, 0)));
        if let Some(t) = w.tone_mapping {
            let an = usize::from(t.num_bezier_curve_anchors);
            assert!(t.knee_point_x < 4096 && t.knee_point_y < 4096 && an <= 15);
            assert!(t.bezier_curve_anchors[..an].iter().all(|&a| a < 1024));
            assert!(t.bezier_curve_anchors[an..].iter().all(|&a| a == 0));
        }
        assert!(w.color_saturation_weight.is_none_or(|s| s < 64));
    }
    let grids = [
        h.targeted_system_display_actual_peak_luminance,
        h.mastering_display_actual_peak_luminance,
    ];
    for g in grids.into_iter().flatten() {
        let (r, c) = (usize::from(g.rows), usize::from(g.cols));
        assert!((2..=25).contains(&r) && (2..=25).contains(&c));
        for (y, row) in g.values.iter().enumerate() {
            for (x, &v) in row.iter().enumerate() {
                assert!(if y < r && x < c { v < 16 } else { v == 0 }, "grid cell {y},{x}");
            }
        }
    }
    let peak = h.scene_peak_nits();
    assert!(peak.is_finite() && (0.0..=13_107.2).contains(&peak));
}

/// ST 2094-40 in the order `Hdr10Plus::parse` reads it.
fn write_hdr10plus(h: &Hdr10Plus) -> Vec<u8> {
    let mut w = BitWriter::default();
    let n = usize::from(h.num_windows);
    w.put(h.application_version.into(), 8).put(h.num_windows.into(), 2);
    for win in &h.windows[1..n] {
        let s = win.shape.expect("a window past the first without an ellipse");
        for v in [s.upper_left, s.lower_right, s.center_of_ellipse].iter().flatten() {
            w.put((*v).into(), 16);
        }
        w.put(s.rotation_angle.into(), 8);
        for v in [s.semimajor_axis_internal, s.semimajor_axis_external, s.semiminor_axis_external] {
            w.put(v.into(), 16);
        }
        w.put(s.overlap_process_option.into(), 1);
    }
    w.put(h.targeted_system_display_maximum_luminance, 27);
    put_grid(&mut w, h.targeted_system_display_actual_peak_luminance);
    for win in &h.windows[..n] {
        for v in win.maxscl {
            w.put(v, 17);
        }
        let pn = win.num_distribution_maxrgb_percentiles;
        w.put(win.average_maxrgb, 17).put(pn.into(), 4);
        for &(pct, v) in &win.distribution_maxrgb[..usize::from(pn)] {
            w.put(pct.into(), 7).put(v, 17);
        }
        w.put(win.fraction_bright_pixels.into(), 10);
    }
    put_grid(&mut w, h.mastering_display_actual_peak_luminance);
    for win in &h.windows[..n] {
        match win.tone_mapping {
            Some(t) => {
                let an = t.num_bezier_curve_anchors;
                w.put(1, 1)
                    .put(t.knee_point_x.into(), 12)
                    .put(t.knee_point_y.into(), 12)
                    .put(an.into(), 4);
                for &a in &t.bezier_curve_anchors[..usize::from(an)] {
                    w.put(a.into(), 10);
                }
            }
            None => {
                w.put(0, 1);
            }
        }
        match win.color_saturation_weight {
            Some(s) => w.put(1, 1).put(s.into(), 6),
            None => w.put(0, 1),
        };
    }
    w.into_bytes()
}

fn put_grid(w: &mut BitWriter, g: Option<PeakGrid>) {
    let Some(g) = g else {
        w.put(0, 1);
        return;
    };
    w.put(1, 1).put(g.rows.into(), 5).put(g.cols.into(), 5);
    for row in &g.values[..usize::from(g.rows)] {
        for &v in &row[..usize::from(g.cols)] {
            w.put(v.into(), 4);
        }
    }
}

fn t35_message(m: &[u8]) {
    let kind = t35::classify(m);
    let Some(t) = T35::split(m) else {
        assert!(m.is_empty() || m == [0xFF]);
        return assert_eq!(kind, T35Kind::Other);
    };
    assert_eq!(t.country, m[0]);
    assert_eq!(t.country_ext.is_some(), t.country == 0xFF);
    assert_eq!(t.rest, &m[1 + usize::from(t.country_ext.is_some())..]);
    assert_eq!(t.kind(), kind);
    let (want_plus, want_rpu) = match kind {
        T35Kind::Hdr10Plus { payload } => {
            assert!(m.starts_with(&HDR10PLUS_CODES) && payload == &m[6..] && !payload.is_empty());
            hdr10plus(payload);
            (Some(payload), None)
        }
        T35Kind::DoviRpu { payload } => {
            assert!(m.starts_with(&DOVI_CODES) && payload == &m[7..] && !payload.is_empty());
            check_rpu(RpuHeader::parse(RpuForm::Av1Emdf, payload));
            (None, Some((RpuForm::Av1Emdf, payload)))
        }
        T35Kind::Other => {
            assert!(!(m.starts_with(&HDR10PLUS_CODES) && m.len() > 6));
            assert!(!(m.starts_with(&DOVI_CODES) && m.len() > 7));
            (None, None)
        }
    };
    let mut buf = [0u8; SIDE_CAP];
    let mut w = Writer::new(&mut buf);
    if !w.push(Kind::T35, m) {
        return assert!(m.len() + 3 > SIDE_CAP && w.is_empty() && w.dropped == 1);
    }
    let meta = FrameMeta::read(w.bytes()).expect("one record the writer wrote");
    assert_eq!(meta.t35().collect::<Vec<_>>(), [m]);
    assert_eq!(meta.hdr10plus_payload(), want_plus);
    assert_eq!(meta.dovi_rpu(), want_rpu);
    assert!(!meta.dovi_el());
}

fn dovi_record(b: &[u8]) {
    let c = match DoviConfig::from_record(b) {
        Ok(c) => c,
        Err(e) => {
            assert!(b.len() < 4);
            assert_eq!(e, DoviError::Short { len: b.len(), need: 4 });
            assert!(
                DoviConfig::from_ts_descriptor(b).is_err()
                    && DoviConfig::ts_dependency_pid(b).is_none()
            );
            return;
        }
    };
    assert!(c.profile < 128 && c.level < 64 && c.compat_id < 16 && c.compression < 4);
    let rec = c.to_record();
    assert_eq!(rec[..4], b[..4], "the fixed part does not write back");
    assert_eq!(rec[4], b.get(4).map_or(0, |x| x & 0xFC));
    assert!(rec[5..].iter().all(|&x| x == 0));
    assert_eq!(DoviConfig::from_record(&rec), Ok(c));
    assert!(c.pack() >= 0);
    assert_eq!(DoviConfig::unpack(c.pack()), c);
    let want_box = match c.profile {
        0..=7 => *b"dvcC",
        8..=10 => *b"dvvC",
        _ => *b"dvwC",
    };
    assert_eq!(c.box_name(), want_box);
    let ts = DoviConfig::from_ts_descriptor(b).expect("four bytes");
    let tail_at = if !c.bl && b.len() >= 6 { 6 } else { 4 };
    let (compat_id, compression) = b.get(tail_at).map_or((0, 0), |x| (x >> 4, x >> 2 & 3));
    assert_eq!(ts, DoviConfig { compat_id, compression, ..c });
    match DoviConfig::ts_dependency_pid(b) {
        Some(pid) => assert!(!c.bl && b.len() >= 6 && pid < 8192),
        None => assert!(c.bl || b.len() < 6),
    }
}

fn check_rpu(r: Result<RpuHeader, DoviError>) {
    let Ok(h) = r else { return };
    assert!(
        h.rpu_type == 2 && h.rpu_format < 2048 && h.vdr_rpu_profile < 16 && h.vdr_rpu_level < 16
    );
    if let Some(s) = h.seq {
        assert!(s.coef_data_type <= 1 && s.vdr_rpu_normalized_idc < 4);
        match s.coef_data_type {
            0 => assert!((13..=32).contains(&s.coef_log2_denom)),
            _ => assert_eq!(s.coef_log2_denom, 32),
        }
        let depths = [s.bl_bit_depth, s.el_bit_depth, s.vdr_bit_depth];
        match h.rpu_format & 0x700 {
            0 => assert!(depths.iter().all(|d| (8..=16).contains(d)) && s.dm_compression < 8),
            _ => assert!(depths == [0; 3] && s.ext_mapping_idc == 0),
        }
    }
    let guess = h.guess_profile();
    assert!(matches!(guess, None | Some(4 | 5 | 7 | 8)));
    for el in [false, true] {
        let d = DoviConfig::derived(&h, el);
        assert_eq!(d.map(|d| (d.profile, d.el, d.rpu, d.bl)), guess.map(|p| (p, el, true, true)));
    }
}

fn rpu(b: &[u8]) {
    check_rpu(RpuHeader::parse(RpuForm::Nal, b));
    check_rpu(RpuHeader::parse(RpuForm::Av1Emdf, b));
    let rbsp = &b[..b.len().min(511)];
    if rbsp.len() < 6 {
        return;
    }
    let mut nal = vec![0x19];
    nal.extend(escape(rbsp));
    let mut w = BitWriter::default();
    w.put(EMDF_MAGIC, 27);
    match rbsp.len() {
        n @ 0..256 => w.put(n as u32, 8).put(0, 1),
        n => w.put(0, 8).put(1, 1).put(n as u32 - 256, 8).put(0, 1),
    };
    for &x in rbsp {
        w.put(x.into(), 8);
    }
    let emdf = w.into_bytes();
    let a = RpuHeader::parse(RpuForm::Nal, &nal);
    assert_eq!(
        a,
        RpuHeader::parse(RpuForm::Av1Emdf, &emdf),
        "NAL and EMDF forms of one RPU differ"
    );
    check_rpu(a);
}

fn escape(rbsp: &[u8]) -> Vec<u8> {
    let (mut out, mut zeros) = (Vec::with_capacity(rbsp.len() * 3 / 2), 0);
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn sei_number(out: &mut Vec<u8>, mut n: u32) {
    while n >= 255 {
        out.push(0xFF);
        n -= 255;
    }
    out.push(n as u8);
}

fn within(outer: &[u8], inner: &[u8]) -> bool {
    let (o, i) = (outer.as_ptr_range(), inner.as_ptr_range());
    inner.is_empty() || (o.start <= i.start && i.end <= o.end)
}

fn sei(rbsp: &[u8]) {
    let msgs: Vec<(u32, &[u8])> = h26x::sei_messages(rbsp).collect();
    let (mut mastering, mut light, mut t35s) = (None, None, Vec::new());
    for &(kind, p) in &msgs {
        assert!(within(rbsp, p));
        match kind {
            h26x::SEI_MASTERING_DISPLAY => mastering = Mastering::from_st2086(p).or(mastering),
            h26x::SEI_CONTENT_LIGHT => light = ContentLight::from_cta861(p).or(light),
            h26x::SEI_T35 if t35::classify(p) != T35Kind::Other => t35s.push(p),
            _ => {}
        }
    }
    let mut buf = [0u8; SIDE_CAP];
    let mut scan = Scan::new(Writer::new(&mut buf));
    h26x::sei(rbsp, &mut scan);
    assert_eq!((scan.mastering, scan.light), (mastering, light));
    let kept: Vec<(u8, &[u8])> = side::records(scan.side.bytes()).collect();
    assert_eq!(kept.len() + scan.side.dropped as usize, t35s.len());
    let mut rest = t35s.iter();
    for (k, b) in &kept {
        assert_eq!(*k, Kind::T35 as u8);
        assert!(rest.any(|m| m == b), "a T.35 record out of order or not in the SEI");
    }
    let mut re = Vec::with_capacity(rbsp.len() + 8);
    for &(kind, p) in &msgs {
        sei_number(&mut re, kind);
        sei_number(&mut re, p.len() as u32);
        re.extend_from_slice(p);
    }
    re.push(0x80);
    let again: Vec<(u32, &[u8])> = h26x::sei_messages(&re).collect();
    assert_eq!(again, msgs, "re-encoded SEI reads back different");
}

fn annexb(unit: &[u8], mode: u8) {
    let codec = if mode & 1 == 0 { Codec::H265 } else { Codec::H264 };
    let scratch_len = [0, 16, 256, 4096][usize::from(mode >> 1 & 3)];
    let side_len = if mode & 8 == 0 { SIDE_CAP } else { 64 };
    let (mut want_rpu, mut want_el) = (None, false);
    for nal in h26x::annexb_nals(unit) {
        assert!(within(unit, nal));
        assert!(!nal.windows(3).any(|w| w == [0, 0, 1]), "a start code inside a NAL");
        assert!(nal.last() != Some(&0), "a trailing zero kept");
        if let (Codec::H265, [b0, b1, ..]) = (codec, nal) {
            let (kind, layer, tid) = (b0 >> 1 & 0x3F, (b0 & 1) << 5 | b1 >> 3, b1 & 7);
            if kind == h26x::H265_DOVI_RPU && layer == 0 && tid == 1 && nal.len() > 2 {
                want_rpu = Some(&nal[2..]);
            }
            want_el |= kind == h26x::H265_DOVI_EL;
        }
    }
    let mut scratch = vec![0u8; scratch_len];
    let mut runs: [ScanOut; 2] = Default::default();
    for run in &mut runs {
        let mut side = vec![0u8; side_len];
        let mut scan = Scan::new(Writer::new(&mut side));
        h26x::scan_annexb(codec, unit, &mut scratch, &mut scan);
        let meta = FrameMeta::read(scan.side.bytes()).expect("scan wrote invalid side records");
        let (mut rpus, mut els) = (0, 0);
        for (k, b) in meta.records() {
            match k {
                Kind::T35 => assert_ne!(t35::classify(b), T35Kind::Other),
                Kind::DoviRpu => {
                    rpus += 1;
                    assert_eq!(Some(b), want_rpu, "not the last layer 0 RPU");
                }
                Kind::DoviEl => {
                    els += 1;
                    assert!(b.is_empty() && want_el);
                }
            }
        }
        let dropped = scan.side.dropped > 0;
        assert!(rpus <= 1 && els <= 1);
        assert!(rpus == 1 || want_rpu.is_none() || dropped, "the unit's RPU went missing");
        assert!(els == 1 || !want_el || dropped, "the unit's EL went missing");
        *run = (scan.mastering, scan.light, scan.side.bytes().to_vec());
    }
    assert!(runs[0] == runs[1], "two scans of one unit differ");
}

fn leb128_bytes(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        out.push(if v > 0 { b | 0x80 } else { b });
        if v == 0 {
            return;
        }
    }
}

fn obus(tu: &[u8]) {
    let mut at = 0;
    if let Some(v) = av1::leb128(tu, &mut at) {
        assert!(v <= u64::from(u32::MAX) && (1..=8).contains(&at));
        let mut enc = Vec::new();
        leb128_bytes(v, &mut enc);
        assert!(enc.len() <= at);
        let mut at2 = 0;
        assert_eq!((av1::leb128(&enc, &mut at2), at2), (Some(v), enc.len()));
    }
    let walk: Vec<(u8, &[u8])> = av1::obus(tu).collect();
    let mut re = Vec::with_capacity(tu.len() + 16);
    for &(kind, p) in &walk {
        assert!(kind < 16 && within(tu, p));
        re.push(kind << 3 | 0x02);
        leb128_bytes(p.len() as u64, &mut re);
        re.extend_from_slice(p);
    }
    assert_eq!(av1::obus(&re).collect::<Vec<_>>(), walk, "re-framed OBUs read back different");
    let mut outs: [ScanOut; 2] = Default::default();
    for (out, src) in outs.iter_mut().zip([tu, &re[..]]) {
        let mut side = [0u8; SIDE_CAP];
        let mut scan = Scan::new(Writer::new(&mut side));
        av1::scan(src, &mut scan);
        let meta = FrameMeta::read(scan.side.bytes()).expect("scan wrote invalid side records");
        for (k, b) in meta.records() {
            assert!(k == Kind::T35 && t35::classify(b) != T35Kind::Other);
        }
        if let Some(m) = scan.mastering {
            let xy = m.primaries.iter().chain([&m.white]).flatten();
            assert!(xy.into_iter().all(|&v| v <= 50_000), "an AV1 chromaticity past 1.0");
        }
        *out = (scan.mastering, scan.light, scan.side.bytes().to_vec());
    }
    assert!(outs[0] == outs[1], "a re-framed unit scans different");
}

fn vp9_colour(f: &[u8]) {
    let Some(c) = vp9::colour(f) else { return };
    assert!(c.profile <= 3 && c.color_space <= 7);
    let depth = if c.profile >= 2 { [10, 12].contains(&c.bit_depth) } else { c.bit_depth == 8 };
    assert!(depth, "profile {} at {} bits", c.profile, c.bit_depth);
    if c.color_space == 7 {
        assert!(c.full_range && !c.subsampling_x && !c.subsampling_y);
    } else if c.profile & 1 == 0 {
        assert!(c.subsampling_x && c.subsampling_y);
    }
    let s = c.stream_color();
    assert_eq!((s.range, s.matrix), (Some(Range::from_flag(c.full_range)), c.matrix()));
    assert_eq!(c.matrix().is_none(), matches!(c.color_space, 0 | 6));
    assert!(vp9::colour(&f[..f.len() - 1]).is_none_or(|d| d == c));
}

fn vui(b: &[u8]) {
    let mut r = Bits::new(b);
    let Some(c) = h26x::vui_colour(&mut r) else {
        return;
    };
    assert_eq!(r.left() + r.pos(), b.len() * 8);
    if c.primaries.is_some() || c.transfer.is_some() || c.matrix.is_some() {
        assert!(c.range.is_some(), "a colour description outside the video signal type");
    }
    assert!(c.hdr.is_empty() && c.dovi.is_none());
    assert!(c.matrix.is_none_or(|m| Matrix::from_h273(m.to_h273()) == Some(m)));
    assert!(c.chroma.is_none_or(|s| ChromaSite::from_h26x(s.to_h26x().into()) == Some(s)));
}

fn side_records(data: &[u8], mode: u8) {
    match FrameMeta::read(data) {
        Ok(meta) => {
            let total: usize = side::records(data).map(|(_, b)| 3 + b.len()).sum();
            assert_eq!(total, data.len());
            assert_eq!(meta.records().count(), side::records(data).count());
            assert_eq!(meta.is_empty(), data.is_empty());
            let mut buf = vec![0u8; data.len()];
            let mut w = Writer::new(&mut buf);
            for (k, b) in meta.records() {
                assert!(w.push(k, b));
            }
            assert_eq!(w.bytes(), data);
            let plus = meta.t35().find_map(|m| match t35::classify(m) {
                T35Kind::Hdr10Plus { payload } => Some(payload),
                _ => None,
            });
            assert_eq!(meta.hdr10plus_payload(), plus);
            if let Some(Ok(h)) = meta.hdr10plus() {
                check_hdr10plus(&h);
            }
            if let Some(r) = meta.dovi_rpu_header() {
                check_rpu(r);
            }
            assert_eq!(meta.dovi_el(), meta.records().any(|(k, _)| k == Kind::DoviEl));
        }
        Err(SideError::Truncated { at }) => {
            assert!(at < data.len() && FrameMeta::read(&data[..at]).is_ok());
        }
        Err(SideError::Kind { at, kind }) => {
            assert!(
                data[at] == kind
                    && Kind::from_u8(kind).is_none()
                    && FrameMeta::read(&data[..at]).is_ok()
            );
        }
    }
    let cut = usize::from(mode) * data.len() / 15;
    let (bitstream, container) = data.split_at(cut);
    let mut buf = vec![0u8; [SIDE_CAP, 64, 512, 4096][usize::from(mode & 3)]];
    let mut w = Writer::new(&mut buf);
    for (k, b) in side::records(bitstream) {
        if let Some(k) = Kind::from_u8(k) {
            w.push(k, b);
        }
    }
    let before = w.len();
    let is_plus = |k: u8, b: &[u8]| {
        k == Kind::T35 as u8 && matches!(t35::classify(b), T35Kind::Hdr10Plus { .. })
    };
    let had_plus = side::records(w.bytes()).any(|(k, b)| is_plus(k, b));
    w.merge_under(container);
    let merged = FrameMeta::read(w.bytes()).expect("merge wrote invalid side records");
    assert_eq!(merged.records().count(), side::records(w.bytes()).count());
    if had_plus {
        let added = side::records(&w.bytes()[before..]).any(|(k, b)| is_plus(k, b));
        assert!(!added, "a container HDR10+ over the bitstream's");
    }
}

fn fit(side: &[u8]) -> &[u8] {
    if side.len() <= SIDE_CAP {
        return side;
    }
    let mut end = 0;
    for (_, b) in side::records(side) {
        if end + 3 + b.len() > SIDE_CAP {
            break;
        }
        end += 3 + b.len();
    }
    &side[..end]
}

fn carry(data: &[u8]) {
    let mut c = Carry::new();
    let mut model: Vec<(u64, u64, Vec<u8>)> = Vec::with_capacity(ENTRIES);
    let (mut clock, mut evicted) = (0u64, 0u64);
    let mut at = 0;
    while at + 2 <= data.len() {
        let (op, arg) = (data[at], data[at + 1]);
        at += 2;
        let pts = match op & 0x3F {
            0x3F => u64::MAX,
            k => u64::from(k),
        };
        match op >> 6 {
            0 | 1 => {
                let len = (usize::from(arg) * 12).min(data.len() - at);
                let side = &data[at..at + len];
                at += len;
                c.put(pts, side);
                if side.is_empty() || pts == u64::MAX {
                    continue;
                }
                let slot = match model.iter().position(|e| e.0 == pts) {
                    Some(i) => i,
                    None if model.len() < ENTRIES => {
                        model.push((pts, 0, Vec::new()));
                        model.len() - 1
                    }
                    None => {
                        evicted += 1;
                        (0..model.len()).min_by_key(|&i| model[i].1).expect("a full table")
                    }
                };
                clock += 1;
                model[slot] = (pts, clock, fit(side).to_vec());
            }
            2 => {
                let want = model.iter().position(|e| e.0 == pts).map(|i| model.swap_remove(i).2);
                assert_eq!(c.take(pts), want.as_deref(), "take of pts {pts}");
            }
            _ => {
                c.clear();
                model.clear();
            }
        }
        assert_eq!((c.len(), c.evicted), (model.len(), evicted));
    }
}

fn static_colour(b: &[u8]) {
    for &cp in b.iter().take(8) {
        assert!(Matrix::from_h273(cp).is_none_or(|m| m.to_h273() == cp && m.class().and_then(Matrix::class) == m.class()));
        assert!(
            TransferFn::from_h273(cp)
                .is_none_or(|t| t.to_h273() == cp && t.class().class() == t.class())
        );
        assert!(
            Primaries::from_h273(cp)
                .is_none_or(|p| p.to_h273() == cp && p.class().class() == p.class())
        );
        assert!(Range::from_mkv(cp).is_none_or(|r| r.to_mkv() == cp));
        assert!(ChromaSite::from_h26x(cp.into()).is_none_or(|s| s.to_h26x() == cp));
        let (h, v) = (cp >> 4, cp & 15);
        assert!(ChromaSite::from_mkv(h, v).is_none_or(|s| s.to_mkv() == Some((h, v))));
    }
    let Some(p) = b.get(..34) else { return };
    let mastering = Mastering::from_st2086(&p[..24]).expect("24 bytes");
    let light = ContentLight::from_cta861(&p[24..28]).expect("4 bytes");
    let hdr = HdrStatic { mastering, light };
    let fields = hdr.fields();
    let mut back = HdrStatic::default();
    for (f, v) in fields {
        if let Some(v) = v {
            assert!(back.set_field(f, v));
        }
    }
    assert_eq!(back, hdr, "static HDR fields do not round trip");
    assert_eq!(hdr.is_empty(), fields.iter().all(|f| f.1.is_none()));
    assert!(!back.set_field("nope", 1));
    let av1 = Mastering::from_av1(
        mastering.primaries,
        mastering.white,
        mastering.max_luminance,
        mastering.min_luminance,
    );
    assert!(av1.primaries.iter().chain([&av1.white]).flatten().all(|&v| v <= 50_000));
    let signal = (p[28] & 1 == 1, (p[28] & 2 == 2).then_some((p[29], p[30], p[31])));
    let mut c = StreamColor::from_vui(Some(signal), Some(p[32].into()));
    c.hdr = hdr;
    c.dovi = DoviConfig::from_record(&p[29..34]).ok().filter(|_| p[33] & 1 == 1);
    assert_eq!(c.over(StreamColor::default()), c);
    assert_eq!(StreamColor::default().over(c), c);
    assert_eq!(c.over(c), c);
    let mut out = [("", ValueDesc::Int(0)); 10];
    let n = c.write_fields(&mut out);
    let ids = [
        c.matrix.is_some(),
        c.range.is_some(),
        c.transfer.is_some(),
        c.primaries.is_some(),
        c.chroma.is_some(),
    ];
    let ints = fields.iter().filter(|f| f.1.is_some()).count() + usize::from(c.dovi.is_some());
    assert_eq!(n, ids.iter().filter(|&&x| x).count() + ints);
    assert_eq!(c.is_complete(), ids[..4].iter().all(|&x| x));
}
