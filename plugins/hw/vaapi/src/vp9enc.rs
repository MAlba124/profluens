//! `vaapivp9enc`, VP9 Profile 0 and 2 through VA-API on the [`encshell`],
//! after ffmpeg's `vaapi_encode_vp9.c`. Not yet run on VP9 encode hardware.

// the elements are public over a codec trait and an engine that stay this crate's
#![allow(private_bounds, private_interfaces)]

use std::sync::Arc;

use profluens_core::element::{ElementDesc, PadDesc};
use profluens_core::format::OfferDesc;

use crate::enc::{EncEngine, InFormat};
use crate::encshell::{
    self, Codec, GPU_SINKS, MakeFn, OUT_FIELDS, Picture, RAW_SINKS, Settings, VaEnc,
};
use crate::error::VaapiErrorKind as K;
use crate::ffi::{
    self, VAEncPictureParameterBufferVP9 as PicParam, VAEncSequenceParameterBufferVP9 as SeqParam,
};
use crate::gpuframe::GpuFrameChannel;
use crate::va::{Buffer as VaBuffer, Display, VaResult};

const CTRL_LAST: u32 = 1;
const LOOP_FILTER: u8 = 16;
const SHARPNESS: u8 = 4;
const MAX_TILE_WIDTH: u32 = 4096;

// Cold: make_default boxes one element when the registry builds the default, not per-frame.
#[allow(clippy::disallowed_methods)]
fn make_default() -> Box<dyn profluens_core::element::Element> {
    Box::new(VaapiVp9Enc::new(Settings::default()))
}

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "vp9", fields: &OUT_FIELDS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static DESC: ElementDesc =
    encshell::desc("vaapivp9enc", &RAW_SINKS, &SRCS, Some(make_default as MakeFn));
static DESC_GPU: ElementDesc = encshell::desc("vaapivp9enc", &GPU_SINKS, &SRCS, None);

pub struct Vp9;

fn log2_tile_columns(width: u32) -> u8 {
    let cols = width.div_ceil(MAX_TILE_WIDTH);
    if cols <= 1 { 0 } else { (32 - (cols - 1).leading_zeros()) as u8 }
}

pub(crate) fn seq_param(fmt: InFormat, cfg: &Settings) -> SeqParam {
    SeqParam {
        max_frame_width: fmt.width,
        max_frame_height: fmt.height,
        kf_auto: 0,
        kf_min_dist: 1,
        kf_max_dist: cfg.gop,
        bits_per_second: cfg.bitrate_kbps * 1000,
        intra_period: cfg.gop,
        va_reserved: [0; ffi::VA_PADDING_LOW],
    }
}

pub(crate) fn pic_param(
    fmt: InFormat,
    cfg: &Settings,
    recon: ffi::VASurfaceID,
    reference: ffi::VASurfaceID,
    p: &Picture,
) -> PicParam {
    let mut reference_frames = [ffi::VA_INVALID_SURFACE; 8];
    let ref_flags = if p.key {
        ffi::VP9_ENC_REF_FORCE_KF
    } else {
        reference_frames[0] = reference;
        CTRL_LAST << ffi::VP9_ENC_REF_FRAME_CTRL_L0_SHIFT | ffi::VP9_ENC_REF_LAST_SIGN_BIAS
    };
    let pic_flags =
        ffi::VP9_ENC_PIC_SHOW_FRAME | if p.key { 0 } else { ffi::VP9_ENC_PIC_FRAME_TYPE_INTER };
    PicParam {
        frame_width_src: fmt.width,
        frame_height_src: fmt.height,
        frame_width_dst: fmt.width,
        frame_height_dst: fmt.height,
        reconstructed_frame: recon,
        reference_frames,
        coded_buf: p.coded,
        ref_flags,
        pic_flags,
        // one reference, every slot holds the last frame
        refresh_frame_flags: 0xFF,
        luma_ac_qindex: cfg.qindex,
        luma_dc_qindex_delta: 0,
        chroma_ac_qindex_delta: 0,
        chroma_dc_qindex_delta: 0,
        filter_level: LOOP_FILTER,
        sharpness_level: SHARPNESS,
        ref_lf_delta: [0; 4],
        mode_lf_delta: [0; 2],
        bit_offset_ref_lf_delta: 0,
        bit_offset_mode_lf_delta: 0,
        bit_offset_lf_level: 0,
        bit_offset_qindex: 0,
        bit_offset_first_partition_size: 0,
        bit_offset_segmentation: 0,
        bit_size_segmentation: 0,
        log2_tile_rows: 0,
        log2_tile_columns: log2_tile_columns(fmt.width),
        skip_frame_flag: 0,
        number_skip_frames: 0,
        skip_frames_size: 0,
        seg_id_block_size: 0,
        va_reserved8: [0; 3],
        va_reserved: [0; 7],
    }
}

impl Codec for Vp9 {
    const FAMILY: &'static str = "vp9";
    const ALIGN: u32 = 64;
    type State = ();

    fn desc(gpu: bool) -> &'static ElementDesc {
        if gpu { &DESC_GPU } else { &DESC }
    }

    fn profile(ten: bool) -> (ffi::VAProfile, &'static str) {
        if ten {
            (ffi::VAProfileVP9Profile2, "VP9Profile2")
        } else {
            (ffi::VAProfileVP9Profile0, "VP9Profile0")
        }
    }

    fn begin(
        _: &Display,
        _: ffi::VAProfile,
        _: ffi::VAEntrypoint,
        _: &EncEngine,
        _: &Settings,
        _: InFormat,
    ) -> Result<(), K> {
        Ok(())
    }

    fn buffers(
        _: &mut (),
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
                &seq_param(fmt, cfg),
            )?);
            if cfg.bitrate_kbps > 0 {
                encshell::rc_buffers(display, engine, cfg.bitrate_kbps, 255, fmt.fps, out)?;
            }
        }
        let pic = pic_param(fmt, cfg, engine.recon_cur(), engine.recon_ref(), p);
        out.push(VaBuffer::new_struct(display, ctx, ffi::VAEncPictureParameterBufferType, &pic)?);
        Ok(())
    }
}

pub type VaapiVp9Enc = VaEnc<Vp9>;

impl VaEnc<Vp9> {
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
    use crate::enc::RawPixFmt;

    fn fmt() -> InFormat {
        InFormat { width: 1920, height: 1080, pixfmt: RawPixFmt::Nv12, fps: (60, 1) }
    }

    #[test]
    fn a_key_frame_refreshes_every_slot_and_references_nothing() {
        let p = Picture { key: true, frame_num: 0, coded: 9 };
        let pic = pic_param(fmt(), &Settings::default(), 5, 6, &p);
        assert_eq!(pic.ref_flags, 1, "force_kf alone");
        assert_eq!(pic.pic_flags, 0b10, "a shown key frame");
        assert_eq!(pic.refresh_frame_flags, 0xFF);
        assert!(pic.reference_frames.iter().all(|&s| s == ffi::VA_INVALID_SURFACE));
        assert_eq!((pic.reconstructed_frame, pic.coded_buf, pic.luma_ac_qindex), (5, 9, 100));
    }

    #[test]
    fn a_p_frame_predicts_from_last_in_slot_zero() {
        let p = Picture { key: false, frame_num: 3, coded: 9 };
        let pic = pic_param(fmt(), &Settings::default(), 5, 6, &p);
        // ref_frame_ctrl_l0 = LAST at bit 1, ref_last_idx 0 at bit 7, sign bias at bit 10
        assert_eq!(pic.ref_flags, (1 << 1) | (1 << 10));
        assert_eq!(pic.pic_flags, 0b11, "a shown inter frame");
        assert_eq!(pic.reference_frames[0], 6);
        assert!(pic.reference_frames[1..].iter().all(|&s| s == ffi::VA_INVALID_SURFACE));
    }

    #[test]
    fn tiles_stay_within_the_widest() {
        assert_eq!(log2_tile_columns(1920), 0);
        assert_eq!(log2_tile_columns(4096), 0);
        assert_eq!(log2_tile_columns(7680), 1);
        assert_eq!(log2_tile_columns(16384), 2);
    }

    #[test]
    fn the_sequence_states_the_gop_and_the_target() {
        let cfg = Settings { gop: 120, bitrate_kbps: 4000, ..Settings::default() };
        let s = seq_param(fmt(), &cfg);
        assert_eq!(
            (
                s.max_frame_width,
                s.max_frame_height,
                s.kf_max_dist,
                s.intra_period,
                s.bits_per_second
            ),
            (1920, 1080, 120, 120, 4_000_000)
        );
    }
}
