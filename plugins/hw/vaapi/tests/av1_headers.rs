//! AV1 sequence and frame header OBUs from vaapiav1enc read back by dav1d. No device needed.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/dav1d_layout.rs"]
mod d;

use std::ffi::c_int;

use libdav1d_sys as sys;
use pf_vaapi::av1enc::obu::{self, Frame, Sequence, Tiles};

unsafe extern "C" {
    // in dav1d.h, not bound by libdav1d-sys
    fn dav1d_parse_sequence_header(out: *mut u8, buf: *const u8, sz: usize) -> c_int;
}

const SIZE_LEN: usize = 4;

fn at<T: Copy>(base: &[u8], off: usize) -> T {
    assert!(off + std::mem::size_of::<T>() <= base.len());
    // SAFETY: in bounds, T a plain integer read unaligned
    unsafe { std::ptr::read_unaligned(base.as_ptr().add(off).cast::<T>()) }
}

fn seq(
    width: u32,
    height: u32,
    ten_bit: bool,
    enable_cdef: bool,
    color: Option<(u8, u8, u8)>,
    level_idx: u8,
) -> Sequence {
    Sequence { level_idx, tier: 0, width, height, ten_bit, enable_cdef, color, full_range: false }
}

fn parse(s: &Sequence) -> Vec<u8> {
    let bytes = obu::sequence_header(s, SIZE_LEN);
    let mut out = vec![0u8; d::SEQ_SIZE];
    // SAFETY: out holds a Dav1dSequenceHeader, bytes its OBUs
    let r = unsafe { dav1d_parse_sequence_header(out.as_mut_ptr(), bytes.as_ptr(), bytes.len()) };
    assert_eq!(r, 0, "dav1d refused the sequence header {bytes:02x?}");
    out
}

#[test]
fn dav1d_reads_the_sequence_headers_as_written() {
    // Keeps the decoder linked, the parser is in it.
    // SAFETY: a static NUL-terminated string
    assert!(!unsafe { sys::dav1d_version() }.is_null());
    let cases: [(Sequence, (u8, u8, u8)); 2] = [
        (seq(1920, 1080, false, false, None, 9), (2, 2, 2)),
        (seq(3840, 2160, true, true, Some((9, 16, 9)), 13), (9, 16, 9)),
    ];
    for (s, chr) in cases {
        let h = parse(&s);
        assert_eq!(at::<u8>(&h, d::SEQ_PROFILE), 0, "Main");
        assert_eq!(
            (at::<i32>(&h, d::SEQ_MAX_WIDTH), at::<i32>(&h, d::SEQ_MAX_HEIGHT)),
            (s.width as i32, s.height as i32)
        );
        assert_eq!(at::<i32>(&h, d::SEQ_LAYOUT), d::DAV1D_PIXEL_LAYOUT_I420);
        assert_eq!(at::<u8>(&h, d::SEQ_HBD), u8::from(s.ten_bit));
        assert_eq!(
            (at::<i32>(&h, d::SEQ_PRI), at::<i32>(&h, d::SEQ_TRC), at::<i32>(&h, d::SEQ_MTRX)),
            (i32::from(chr.0), i32::from(chr.1), i32::from(chr.2))
        );
        assert_eq!(at::<u8>(&h, d::SEQ_COLOR_RANGE), 0);
        assert_eq!(at::<u8>(&h, d::SEQ_NUM_OPERATING_POINTS), 1);
        // seq_level_idx is 4 * (major - 2) + minor
        let (major, minor) =
            (at::<u8>(&h, d::SEQ_OP0_MAJOR_LEVEL), at::<u8>(&h, d::SEQ_OP0_MINOR_LEVEL));
        assert_eq!(4 * (major - 2) + minor, s.level_idx);
        assert_eq!(at::<u8>(&h, d::SEQ_STILL_PICTURE), 0);
        assert_eq!(at::<u8>(&h, d::SEQ_SB128), 0, "64 pixel superblocks");
        assert_eq!(
            (at::<u8>(&h, d::SEQ_ORDER_HINT), at::<u8>(&h, d::SEQ_ORDER_HINT_N_BITS)),
            (1, 8)
        );
        assert_eq!(at::<u8>(&h, d::SEQ_CDEF), u8::from(s.enable_cdef));
        assert_eq!(
            (at::<u8>(&h, d::SEQ_RESTORATION), at::<u8>(&h, d::SEQ_FILM_GRAIN_PRESENT)),
            (0, 0)
        );
    }
}

struct Decoder(*mut sys::Dav1dContext);

impl Decoder {
    fn new() -> Decoder {
        // SAFETY: zero is a valid settings struct for dav1d_default_settings to fill
        let mut s: sys::Dav1dSettings = unsafe { std::mem::zeroed() };
        // SAFETY: s is a valid out pointer
        unsafe { sys::dav1d_default_settings(&mut s) };
        (s.n_threads, s.max_frame_delay) = (1, 1);
        s.logger.callback = None;
        let mut c = std::ptr::null_mut();
        // SAFETY: c and s valid
        assert_eq!(unsafe { sys::dav1d_open(&mut c, &s) }, 0);
        Decoder(c)
    }

    fn try_frame_header(&mut self, tu: &[u8]) -> Option<Vec<u8>> {
        // SAFETY: zero is a valid empty Dav1dData
        let mut data: sys::Dav1dData = unsafe { std::mem::zeroed() };
        // SAFETY: dav1d allocates tu.len() bytes and hands them back
        let buf = unsafe { sys::dav1d_data_create(&mut data, tu.len()) };
        assert!(!buf.is_null());
        // SAFETY: buf holds tu.len() bytes
        unsafe { std::ptr::copy_nonoverlapping(tu.as_ptr(), buf, tu.len()) };
        // SAFETY: the context and the data are valid, dav1d takes the data
        let sent = unsafe { sys::dav1d_send_data(self.0, &mut data) };
        if sent != 0 {
            // SAFETY: what dav1d did not take is ours
            unsafe { sys::dav1d_data_unref(&mut data) };
        }
        // SAFETY: zero is a valid empty picture
        let mut pic: sys::Dav1dPicture = unsafe { std::mem::zeroed() };
        // SAFETY: the context and the picture are valid
        if unsafe { sys::dav1d_get_picture(self.0, &mut pic) } != 0 {
            return None;
        }
        assert!(!pic.frame_hdr.is_null());
        // SAFETY: frame_hdr points at a live Dav1dFrameHeader for as long as pic
        let fh = unsafe {
            std::slice::from_raw_parts(pic.frame_hdr.cast::<u8>(), d::FH_REDUCED_TXTP_SET + 1)
        }
        .to_vec();
        // SAFETY: pic is ours, released once
        unsafe { sys::dav1d_picture_unref(&mut pic) };
        Some(fh)
    }

    /// A filler that breaks dav1d's block decoding gives no picture, so a few are tried.
    fn frame_header(&mut self, head: &[u8]) -> Vec<u8> {
        for fill in fillers() {
            let mut tu = head.to_vec();
            tu.extend_from_slice(&obu::tile_group(&fill, SIZE_LEN));
            if let Some(fh) = self.try_frame_header(&tu) {
                return fh;
            }
        }
        panic!("dav1d decoded no picture of the header with any filler");
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the context is ours, closed once
        unsafe { sys::dav1d_close(&mut self.0) };
    }
}

fn fillers() -> [Vec<u8>; 4] {
    [
        vec![0; 4000],
        vec![0xFF; 4000],
        (0..3000).map(|i| (i * 131 % 251) as u8).collect(),
        vec![0; 30000],
    ]
}

#[test]
fn dav1d_reads_the_frame_headers_as_written() {
    for (w, h, ten) in [(320u32, 240u32, false), (1920, 1080, true)] {
        let s = seq(
            w,
            h,
            ten,
            false,
            None,
            obu::level_idx(w.next_multiple_of(64), h.next_multiple_of(64), 30.0),
        );
        let t = Tiles::single(w, h).unwrap();
        let mut dec = Decoder::new();
        let mut slot = 0u8;
        for n in 0..4u8 {
            let key = n == 0;
            let next = if key { 0 } else { 1 - slot };
            let f = Frame {
                key,
                order_hint: n,
                refresh_frame_flags: if key { 0xFF } else { 1 << next },
                primary_ref_frame: slot,
                ref_frame_idx: [slot; 7],
                base_q_idx: 40 + n,
                tx_mode_select: n % 2 == 0,
            };
            let mut tu = if key { obu::sequence_header(&s, SIZE_LEN) } else { Vec::new() };
            let fh = obu::frame_header(&s, &f, &t, SIZE_LEN);
            tu.extend_from_slice(&fh.bytes);
            let got = dec.frame_header(&tu);
            let ty = if key { d::DAV1D_FRAME_TYPE_KEY } else { d::DAV1D_FRAME_TYPE_INTER };
            assert_eq!(at::<i32>(&got, d::FH_FRAME_TYPE), ty, "{w}x{h} frame {n} type");
            assert_eq!(
                (at::<i32>(&got, d::FH_WIDTH), at::<i32>(&got, d::FH_HEIGHT)),
                (w as i32, h as i32)
            );
            assert_eq!(at::<i32>(&got, d::FH_RENDER_WIDTH), w as i32);
            assert_eq!(at::<u8>(&got, d::FH_FRAME_OFFSET), n, "order hint");
            assert_eq!(at::<u8>(&got, d::FH_SHOW_FRAME), 1);
            assert_eq!(at::<u8>(&got, d::FH_ERROR_RESILIENT_MODE), u8::from(key));
            assert_eq!(at::<u8>(&got, d::FH_REFRESH_FRAME_FLAGS), f.refresh_frame_flags);
            assert_eq!(at::<u8>(&got, d::FH_PRIMARY_REF_FRAME), f.primary());
            assert_eq!(at::<u8>(&got, d::FH_QUANT_YAC), f.base_q_idx);
            assert_eq!(
                (
                    at::<u8>(&got, d::FH_QUANT_QM),
                    at::<u8>(&got, d::FH_SEGMENTATION_ENABLED),
                    at::<u8>(&got, d::FH_DELTA_Q_PRESENT)
                ),
                (0, 0, 0)
            );
            assert_eq!(
                (
                    at::<u8>(&got, d::FH_TILING_UNIFORM),
                    at::<u8>(&got, d::FH_TILING_COLS),
                    at::<u8>(&got, d::FH_TILING_ROWS)
                ),
                (1, 1, 1)
            );
            assert_eq!(at::<[u8; 2]>(&got, d::FH_LOOPFILTER_LEVEL_Y), [0, 0]);
            let tx = if f.tx_mode_select { d::DAV1D_TX_SWITCHABLE } else { d::DAV1D_TX_LARGEST };
            assert_eq!(at::<i32>(&got, d::FH_TXFM_MODE), tx, "{w}x{h} frame {n} tx mode");
            assert_eq!(at::<u8>(&got, d::FH_REDUCED_TXTP_SET), 0);
            if !key {
                assert_eq!(
                    at::<[i8; 7]>(&got, d::FH_REFIDX),
                    [slot as i8; 7],
                    "every reference the last frame"
                );
                assert_eq!(at::<u8>(&got, d::FH_HP), 0);
                assert_eq!(at::<i32>(&got, d::FH_SUBPEL_FILTER_MODE), d::DAV1D_FILTER_8TAP_REGULAR);
                assert_eq!(at::<u8>(&got, d::FH_SWITCHABLE_COMP_REFS), 0, "single reference");
            }
            slot = next;
        }
        eprintln!(
            "{w}x{h} at {} bits, a key frame and three P frames read back by dav1d",
            if ten { 10 } else { 8 }
        );
    }
}

#[test]
fn the_cdef_and_quantizer_offsets_are_where_dav1d_reads_them() {
    let s = seq(640, 360, false, true, None, 4);
    let t = Tiles::single(640, 360).unwrap();
    let f = Frame {
        key: true,
        order_hint: 0,
        refresh_frame_flags: 0xFF,
        primary_ref_frame: 0,
        ref_frame_idx: [0; 7],
        base_q_idx: 100,
        tx_mode_select: true,
    };
    let mut fh = obu::frame_header(&s, &f, &t, SIZE_LEN);
    for i in 0..8 {
        let bit = fh.qindex_offset + i;
        let v = 0xC3u8 >> (7 - i) & 1;
        fh.bytes[bit / 8] = fh.bytes[bit / 8] & !(0x80 >> (bit % 8)) | v << (7 - bit % 8);
    }
    let mut tu = obu::sequence_header(&s, SIZE_LEN);
    tu.extend_from_slice(&fh.bytes);
    let got = Decoder::new().frame_header(&tu);
    assert_eq!(at::<u8>(&got, d::FH_QUANT_YAC), 0xC3, "the rewritten quantizer");
    assert_eq!(at::<u8>(&got, d::FH_CDEF_DAMPING), 3, "cdef_damping_minus_3 0 at its offset");
}
