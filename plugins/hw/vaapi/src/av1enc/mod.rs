//! `vaapiav1enc`, AV1 Main through VA-API on the [`encshell`], after
//! ffmpeg's `vaapi_encode_av1.c`. Not yet run on AV1 encode hardware.

// the elements are public over a codec trait and an engine that stay this crate's
#![allow(private_bounds, private_interfaces)]

pub mod layout;
pub mod obu;

use std::sync::Arc;

use profluens_core::element::{ElementDesc, PadDesc};
use profluens_core::format::OfferDesc;

use crate::enc::{EncEngine, InFormat, RawPixFmt};
use crate::encshell::{
    self, Codec, GPU_SINKS, MakeFn, OUT_FIELDS, Picture, RAW_SINKS, Settings, VaEnc,
};
use crate::error::VaapiErrorKind as K;
use crate::ffi;
use crate::gpuframe::GpuFrameChannel;
use crate::va::{self, Buffer as VaBuffer, Display, VaResult};
use layout as l;
use obu::{Frame, Sequence, Tiles};

const REF_FRAME_LAST: u32 = 1;
const TX_MODE_LARGEST: u32 = 1;
const TX_MODE_SELECT: u32 = 2;
const KEY_FRAME: u32 = 0;
const INTER_FRAME: u32 = 1;

#[derive(Clone, Copy)]
pub(crate) struct Raw<const N: usize>(pub [u8; N]);

impl<const N: usize> Raw<N> {
    fn zero() -> Self {
        Raw([0; N])
    }

    fn u8(&mut self, at: usize, v: u8) {
        self.0[at] = v;
    }

    fn u16(&mut self, at: usize, v: u16) {
        self.0[at..at + 2].copy_from_slice(&v.to_ne_bytes());
    }

    fn u32(&mut self, at: usize, v: u32) {
        self.0[at..at + 4].copy_from_slice(&v.to_ne_bytes());
    }

    pub fn get_u32(&self, at: usize) -> u32 {
        u32::from_ne_bytes([self.0[at], self.0[at + 1], self.0[at + 2], self.0[at + 3]])
    }

    fn bits(&mut self, at: usize, (shift, width): (u32, u32), v: u32) {
        let mask = ((1u64 << width) - 1) as u32;
        let cur = self.get_u32(at) & !(mask << shift);
        self.u32(at, cur | (v & mask) << shift);
    }

    fn bits8(&mut self, at: usize, (shift, width): (u32, u32), v: u8) {
        let mask = ((1u16 << width) - 1) as u8;
        self.0[at] = self.0[at] & !(mask << shift) | (v & mask) << shift;
    }
}

pub(crate) type SeqParam = Raw<{ l::SEQ_SIZE }>;
pub(crate) type PicParam = Raw<{ l::PIC_SIZE }>;
pub(crate) type TileGroup = Raw<{ l::TG_SIZE }>;

fn field(v: u32, (shift, width): (u32, u32)) -> u32 {
    v >> shift & ((1u64 << width) - 1) as u32
}

// Cold: make_default boxes one element when the registry builds the default, not per-frame.
#[allow(clippy::disallowed_methods)]
fn make_default() -> Box<dyn profluens_core::element::Element> {
    Box::new(VaapiAv1Enc::new(Settings::default()))
}

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "av1", fields: &OUT_FIELDS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
pub(crate) static DESC: ElementDesc =
    encshell::desc("vaapiav1enc", &RAW_SINKS, &SRCS, Some(make_default as MakeFn));
static DESC_GPU: ElementDesc = encshell::desc("vaapiav1enc", &GPU_SINKS, &SRCS, None);

#[derive(Debug, Clone)]
pub(crate) struct State {
    pub seq: Sequence,
    pub tiles: Tiles,
    pub size_len: usize,
    pub tx_select: bool,
    pub seq_obu: Vec<u8>,
    pub slot: u8,
    /// The slot the picture in flight refreshes, `slot` once it is coded.
    pub next_slot: u8,
}

pub struct Av1;

pub(crate) fn state(
    ext2: u32,
    cfg: &Settings,
    fmt: InFormat,
    coded: (u32, u32),
) -> Result<State, K> {
    let size_len = field(ext2, l::EXT2_OBU_SIZE_BYTES_MINUS1) as usize + 1;
    let tx = field(ext2, l::EXT2_TX_MODE_SUPPORT);
    // ONLY_4X4 left out, coded lossless is not done
    let tx_select = match (tx & 0x04 != 0, tx & 0x02 != 0) {
        (true, _) => true,
        (false, true) => false,
        _ => return Err(K::DriverLacks("an AV1 transform mode this encoder writes")),
    };
    let tiles = Tiles::single(fmt.width, fmt.height)
        .ok_or(K::DriverLacks("an AV1 frame of more than one tile"))?;
    let fps = if fmt.fps.0 > 0 { f64::from(fmt.fps.0) / f64::from(fmt.fps.1.max(1)) } else { 0.0 };
    let level_idx = obu::level_idx(coded.0, coded.1, fps);
    let seq = Sequence {
        level_idx,
        // past the table, the maximum parameters level at the high tier, as ffmpeg has it
        tier: u8::from(level_idx == 31),
        width: fmt.width,
        height: fmt.height,
        ten_bit: fmt.pixfmt == RawPixFmt::P010,
        enable_cdef: cfg.bitrate_kbps > 0,
        color: None,
        full_range: false,
    };
    let seq_obu = obu::sequence_header(&seq, size_len);
    Ok(State { seq, tiles, size_len, tx_select, seq_obu, slot: 0, next_slot: 0 })
}

pub(crate) fn seq_param(st: &State, cfg: &Settings) -> SeqParam {
    let mut s = SeqParam::zero();
    s.u8(l::SEQ_PROFILE, 0);
    s.u8(l::SEQ_LEVEL_IDX, st.seq.level_idx);
    s.u8(l::SEQ_TIER, st.seq.tier);
    s.u32(l::SEQ_INTRA_PERIOD, cfg.gop);
    s.u32(l::SEQ_IP_PERIOD, 1);
    s.u32(l::SEQ_BITS_PER_SECOND, cfg.bitrate_kbps.saturating_mul(1000));
    s.bits(l::SEQ_FIELDS, l::SEQ_ENABLE_ORDER_HINT, 1);
    s.bits(l::SEQ_FIELDS, l::SEQ_ENABLE_CDEF, u32::from(st.seq.enable_cdef));
    s.bits(l::SEQ_FIELDS, l::SEQ_BIT_DEPTH_MINUS8, if st.seq.ten_bit { 2 } else { 0 });
    s.bits(l::SEQ_FIELDS, l::SEQ_SUBSAMPLING_X, 1);
    s.bits(l::SEQ_FIELDS, l::SEQ_SUBSAMPLING_Y, 1);
    s.u8(l::SEQ_ORDER_HINT_BITS_MINUS_1, 7);
    s
}

pub(crate) fn frame(p: &Picture, prev_slot: u8, st: &State, cfg: &Settings) -> (Frame, u8) {
    let slot = if p.key { 0 } else { 1 - prev_slot };
    let f = Frame {
        key: p.key,
        order_hint: p.frame_num as u8,
        refresh_frame_flags: if p.key { 0xFF } else { 1 << slot },
        primary_ref_frame: prev_slot,
        ref_frame_idx: [prev_slot; 7],
        base_q_idx: cfg.qindex.max(1),
        tx_mode_select: st.tx_select,
    };
    (f, slot)
}

pub(crate) fn pic_param(
    f: &Frame,
    h: &obu::FrameHeader,
    st: &State,
    cfg: &Settings,
    recon: ffi::VASurfaceID,
    reference: ffi::VASurfaceID,
    coded: ffi::VABufferID,
) -> PicParam {
    let mut p = PicParam::zero();
    p.u16(l::PIC_FRAME_WIDTH_MINUS_1, (st.seq.width - 1) as u16);
    p.u16(l::PIC_FRAME_HEIGHT_MINUS_1, (st.seq.height - 1) as u16);
    p.u32(l::PIC_RECONSTRUCTED_FRAME, recon);
    p.u32(l::PIC_CODED_BUF, coded);
    for i in 0..8 {
        p.u32(l::PIC_REFERENCE_FRAMES + 4 * i, ffi::VA_INVALID_SURFACE);
    }
    if !f.key {
        p.u32(l::PIC_REFERENCE_FRAMES + 4 * usize::from(f.ref_frame_idx[0]), reference);
        p.bits(l::PIC_REF_FRAME_CTRL_L0, l::REF_CTRL_SEARCH_IDX0, REF_FRAME_LAST);
    }
    for (i, &r) in f.ref_frame_idx.iter().enumerate() {
        p.u8(l::PIC_REF_FRAME_IDX + i, r);
    }
    p.u8(l::PIC_PRIMARY_REF_FRAME, f.primary());
    p.u8(l::PIC_ORDER_HINT, f.order_hint);
    p.u8(l::PIC_REFRESH_FRAME_FLAGS, f.refresh_frame_flags);
    p.bits(
        l::PIC_PICTURE_FLAGS,
        l::PIC_FLAG_FRAME_TYPE,
        if f.key { KEY_FRAME } else { INTER_FRAME },
    );
    // a shown key frame is error resilient, the header says so by not saying
    p.bits(l::PIC_PICTURE_FLAGS, l::PIC_FLAG_ERROR_RESILIENT_MODE, u32::from(f.key));
    p.bits(l::PIC_PICTURE_FLAGS, l::PIC_FLAG_ENABLE_FRAME_OBU, 0);
    p.bits(
        l::PIC_MODE_CONTROL_FLAGS,
        l::PIC_MODE_TX_MODE,
        if f.tx_mode_select { TX_MODE_SELECT } else { TX_MODE_LARGEST },
    );
    p.bits(l::PIC_MODE_CONTROL_FLAGS, l::PIC_MODE_REFERENCE_MODE, 0);
    p.u8(l::PIC_BASE_QINDEX, f.base_q_idx);
    p.u8(l::PIC_TILE_COLS, 1);
    p.u8(l::PIC_TILE_ROWS, 1);
    p.u16(l::PIC_WIDTH_IN_SBS_MINUS_1, (st.tiles.sb_cols - 1) as u16);
    p.u16(l::PIC_HEIGHT_IN_SBS_MINUS_1, (st.tiles.sb_rows - 1) as u16);
    p.bits8(l::PIC_TILE_GROUP_OBU_HDR_INFO, l::PIC_TG_OBU_HAS_SIZE_FIELD, 1);
    if cfg.bitrate_kbps > 0 {
        p.u8(l::PIC_MIN_BASE_QINDEX, 1);
        p.u8(l::PIC_MAX_BASE_QINDEX, 255);
        p.u32(l::PIC_BIT_OFFSET_QINDEX, h.qindex_offset as u32);
        p.u32(l::PIC_BIT_OFFSET_LOOPFILTER_PARAMS, h.loopfilter_offset as u32);
        p.u32(l::PIC_BIT_OFFSET_CDEF_PARAMS, h.cdef_offset as u32);
        p.u32(l::PIC_SIZE_IN_BITS_CDEF_PARAMS, h.cdef_size as u32);
        p.u32(l::PIC_SIZE_IN_BITS_FRAME_HDR_OBU, h.bits as u32);
        // the frame header's size field, behind the sequence header on a key frame and the OBU
        // header
        let behind = if f.key { st.seq_obu.len() } else { 0 };
        p.u32(l::PIC_BYTE_OFFSET_FRAME_HDR_OBU_SIZE, (behind + 1) as u32);
    }
    p
}

fn packed(
    display: &Display,
    engine: &EncEngine,
    kind: u32,
    data: &[u8],
    out: &mut Vec<VaBuffer>,
) -> VaResult<()> {
    let ctx = engine.context();
    let hdr = ffi::VAEncPackedHeaderParameterBuffer {
        type_: kind,
        bit_length: (data.len() * 8) as u32,
        has_emulation_bytes: 1,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    };
    out.push(VaBuffer::new_struct(display, ctx, ffi::VAEncPackedHeaderParameterBufferType, &hdr)?);
    out.push(VaBuffer::new_typed_bytes(display, ctx, ffi::VAEncPackedHeaderDataBufferType, data)?);
    Ok(())
}

impl Codec for Av1 {
    const FAMILY: &'static str = "av1";
    const ALIGN: u32 = 64;
    const PACKED: u32 = ffi::VA_ENC_PACKED_HEADER_SEQUENCE | ffi::VA_ENC_PACKED_HEADER_PICTURE;
    type State = State;

    fn desc(gpu: bool) -> &'static ElementDesc {
        if gpu { &DESC_GPU } else { &DESC }
    }

    fn profile(_ten: bool) -> (ffi::VAProfile, &'static str) {
        (ffi::VAProfileAV1Profile0, "AV1Profile0")
    }

    fn begin(
        display: &Display,
        profile: ffi::VAProfile,
        entrypoint: ffi::VAEntrypoint,
        engine: &EncEngine,
        cfg: &Settings,
        fmt: InFormat,
    ) -> Result<State, K> {
        let attr =
            |a| va::config_attribute(display, profile, entrypoint, a).map_err(K::SessionSetup);
        let packed = attr(ffi::VAConfigAttribEncPackedHeaders)?.unwrap_or(0);
        if packed & Self::PACKED != Self::PACKED {
            return Err(K::DriverLacks("packed AV1 sequence and frame headers"));
        }
        let ext2 = attr(l::CONFIG_ATTRIB_ENC_AV1_EXT2)?
            .ok_or(K::DriverLacks("VAConfigAttribEncAV1Ext2"))?;
        state(ext2, cfg, fmt, (engine.coded_w, engine.coded_h))
    }

    fn buffers(
        st: &mut State,
        display: &Display,
        engine: &EncEngine,
        cfg: &Settings,
        fmt: InFormat,
        p: &Picture,
        out: &mut Vec<VaBuffer>,
    ) -> VaResult<()> {
        let ctx = engine.context();
        if p.key {
            out.push(VaBuffer::new_struct(
                display,
                ctx,
                ffi::VAEncSequenceParameterBufferType,
                &seq_param(st, cfg).0,
            )?);
            if cfg.bitrate_kbps > 0 {
                encshell::rc_buffers(display, engine, cfg.bitrate_kbps, 255, fmt.fps, out)?;
            }
        }
        let (f, slot) = frame(p, st.slot, st, cfg);
        let h = obu::frame_header(&st.seq, &f, &st.tiles, st.size_len);
        let pic = pic_param(&f, &h, st, cfg, engine.recon_cur(), engine.recon_ref(), p.coded);
        out.push(VaBuffer::new_struct(display, ctx, ffi::VAEncPictureParameterBufferType, &pic.0)?);
        if p.key {
            packed(display, engine, ffi::VAEncPackedHeaderSequence, &st.seq_obu, out)?;
        }
        packed(display, engine, ffi::VAEncPackedHeaderPicture, &h.bytes, out)?;
        let mut tg = TileGroup::zero();
        tg.u8(l::TG_START, 0);
        tg.u8(l::TG_END, 0);
        out.push(VaBuffer::new_struct(display, ctx, ffi::VAEncSliceParameterBufferType, &tg.0)?);
        st.next_slot = slot;
        Ok(())
    }

    fn commit(st: &mut State) {
        st.slot = st.next_slot;
    }
}

pub type VaapiAv1Enc = VaEnc<Av1>;

impl VaEnc<Av1> {
    pub fn new(cfg: Settings) -> Self {
        VaEnc::with_settings(cfg)
    }

    pub fn new_zerocopy(cfg: Settings, channel: Arc<GpuFrameChannel>) -> Self {
        VaEnc::with_settings(cfg).zerocopy(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt() -> InFormat {
        InFormat { width: 1920, height: 1080, pixfmt: RawPixFmt::Nv12, fps: (60, 1) }
    }

    fn ext2() -> u32 {
        3 << l::EXT2_OBU_SIZE_BYTES_MINUS1.0 | 0b110 << l::EXT2_TX_MODE_SUPPORT.0
    }

    #[test]
    fn the_session_reads_the_drivers_obu_size_and_transform_modes() {
        let st = state(ext2(), &Settings::default(), fmt(), (1920, 1088)).unwrap();
        assert_eq!((st.size_len, st.tx_select), (4, true));
        assert_eq!(st.seq.level_idx, 9, "1080p60 is 4.1");
        let largest =
            state(0b010 << l::EXT2_TX_MODE_SUPPORT.0, &Settings::default(), fmt(), (1920, 1088))
                .unwrap();
        assert!(!largest.tx_select);
        assert!(state(0, &Settings::default(), fmt(), (1920, 1088)).is_err(), "no transform mode");
    }

    #[test]
    fn p_frames_take_the_other_slot_and_predict_from_the_last() {
        let st = state(ext2(), &Settings::default(), fmt(), (1920, 1088)).unwrap();
        let cfg = Settings::default();
        let key = Picture { key: true, frame_num: 0, coded: 1 };
        let (f, s0) = frame(&key, 1, &st, &cfg);
        assert_eq!((f.refresh_frame_flags, s0, f.primary()), (0xFF, 0, 7));
        let p1 = Picture { key: false, frame_num: 1, coded: 1 };
        let (f1, s1) = frame(&p1, s0, &st, &cfg);
        assert_eq!(
            (f1.refresh_frame_flags, s1, f1.ref_frame_idx, f1.order_hint),
            (0b10, 1, [0; 7], 1)
        );
        let (f2, s2) = frame(&Picture { key: false, frame_num: 2, coded: 1 }, s1, &st, &cfg);
        assert_eq!((f2.refresh_frame_flags, s2, f2.ref_frame_idx), (0b01, 0, [1; 7]));
    }

    #[test]
    fn the_picture_parameters_land_at_libvas_offsets() {
        let st = state(
            ext2(),
            &Settings { bitrate_kbps: 4000, ..Settings::default() },
            fmt(),
            (1920, 1088),
        )
        .unwrap();
        let cfg = Settings { bitrate_kbps: 4000, ..Settings::default() };
        let (f, _) = frame(&Picture { key: false, frame_num: 3, coded: 9 }, 0, &st, &cfg);
        let h = obu::frame_header(&st.seq, &f, &st.tiles, st.size_len);
        let p = pic_param(&f, &h, &st, &cfg, 5, 6, 9);
        assert_eq!(u16::from_ne_bytes([p.0[0], p.0[1]]), 1919);
        assert_eq!(p.get_u32(l::PIC_CODED_BUF), 9);
        assert_eq!(p.get_u32(l::PIC_REFERENCE_FRAMES), 6, "the last frame in slot 0");
        assert_eq!(p.get_u32(l::PIC_REFERENCE_FRAMES + 4), ffi::VA_INVALID_SURFACE);
        assert_eq!(field(p.get_u32(l::PIC_PICTURE_FLAGS), l::PIC_FLAG_FRAME_TYPE), INTER_FRAME);
        assert_eq!(
            field(p.get_u32(l::PIC_MODE_CONTROL_FLAGS), l::PIC_MODE_TX_MODE),
            TX_MODE_SELECT
        );
        assert_eq!(
            field(p.get_u32(l::PIC_REF_FRAME_CTRL_L0), l::REF_CTRL_SEARCH_IDX0),
            REF_FRAME_LAST
        );
        assert_eq!(p.get_u32(l::PIC_BIT_OFFSET_QINDEX), h.qindex_offset as u32);
        assert_eq!(
            p.get_u32(l::PIC_BYTE_OFFSET_FRAME_HDR_OBU_SIZE),
            1,
            "an inter frame's header leads the packed data"
        );
        assert_eq!(p.0[l::PIC_TILE_GROUP_OBU_HDR_INFO], 0b10, "obu_has_size_field");
    }
}
