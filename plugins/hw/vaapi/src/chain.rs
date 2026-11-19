//! A chain spec's elements on VA-API.

use crate::vpp::{Target, VaapiPostProc};
use crate::{
    GpuFrameChannel, VaapiAv1Dec, VaapiH264Dec, VaapiH264Enc, VaapiH265Dec, VaapiH265Enc,
    VaapiVp9Dec,
};
use profluens_core::element::{Element, SchedHint};
use profluens_core::format::Value;
use profluens_core::pipeline::Pipeline;
use profluens_video::chain::{
    ChainError, ChainIds, ChainSpec, Decode, Handover, Picture, Rate, Scale, Shrink, SoftDecoder,
    udmabuf_allocator,
};
use std::sync::Arc;

fn target(p: Picture) -> Target {
    Target { max_width: p.width, max_height: p.height, depth: p.depth }
}

/// The device's zero-copy decoder of `family` writing to `out`, scaling in its pass with
/// `shrink`, None for a family it has none of.
// Cold, one boxing an element at a chain's build.
#[allow(clippy::disallowed_methods)]
fn decoder(
    family: &str,
    out: &Arc<GpuFrameChannel>,
    shrink: Option<Shrink>,
) -> Option<Box<dyn Element>> {
    let out = Arc::clone(out);
    Some(match (family, shrink) {
        ("h264/annexb", Some(s)) => {
            Box::new(VaapiH264Dec::new_zerocopy(out).with_shrink(target(s.to), s.fixed))
        }
        ("h264/annexb", None) => Box::new(VaapiH264Dec::new_zerocopy(out)),
        ("h265/annexb", Some(s)) => {
            Box::new(VaapiH265Dec::new_zerocopy(out).with_shrink(target(s.to), s.fixed))
        }
        ("h265/annexb", None) => Box::new(VaapiH265Dec::new_zerocopy(out)),
        ("vp9", Some(s)) => {
            Box::new(VaapiVp9Dec::new_zerocopy(out).with_shrink(target(s.to), s.fixed))
        }
        ("vp9", None) => Box::new(VaapiVp9Dec::new_zerocopy(out)),
        ("av1", None) => Box::new(VaapiAv1Dec::new_zerocopy(out)),
        _ => return None,
    })
}

/// How the scaler takes the decoder's pictures.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Device,
    Upload,
    Udmabuf,
}

/// vaapipostproc as `sc` states it, taking pictures from `a` as `input` has them and putting
/// them out on `b`.
pub fn scaler(
    sc: Scale,
    input: Input,
    a: &Arc<GpuFrameChannel>,
    b: &Arc<GpuFrameChannel>,
) -> VaapiPostProc {
    let mut v = match input {
        Input::Device => VaapiPostProc::new_zerocopy(target(sc.to), Arc::clone(a)),
        Input::Upload | Input::Udmabuf => VaapiPostProc::new(target(sc.to)),
    };
    if sc.fixed_size {
        v = v.with_fixed_size();
    }
    if let Some((rotation, par)) = sc.turn {
        v = v.with_rotation(rotation).with_par(par.0, par.1);
    }
    v = v.with_gpu_output(Arc::clone(b));
    if sc.warm_tone_map {
        v = v.with_warm_tone_map();
    }
    if input == Input::Udmabuf {
        v = v.with_dmabuf_input(Arc::clone(a));
    }
    v
}

/// Adds `s`'s decoder, vaapipostproc and encoder to `p`, pooled and scheduled as it states,
/// unlinked. A software decode is `soft`'s.
// Cold, one boxing an element at a chain's build.
#[allow(clippy::disallowed_methods)]
pub fn build(p: &mut Pipeline, s: &ChainSpec, soft: SoftDecoder) -> Result<ChainIds, ChainError> {
    // every stage is on the one GPU, a picture is handed on as queued and the dma-buf's
    // fence orders the next stage's reads, nobody waits for the GPU between stages
    let (a, b) = (GpuFrameChannel::queued(), GpuFrameChannel::queued());
    let out = if s.scale.is_some() { &a } else { &b };
    let (dec, input) = match s.decode {
        Decode::Device { shrink, .. } => (decoder(s.from, out, shrink), Input::Device),
        Decode::Software { handover: Handover::Upload, .. } => (soft(&a, None), Input::Upload),
        Decode::Software { handover: Handover::Udmabuf { row_align, .. }, .. } => {
            let formats =
                crate::vpp::linear_imports().map_err(|e| ChainError::Udmabuf(Box::new(e)))?;
            (soft(&a, Some((formats, row_align))), Input::Udmabuf)
        }
    };
    let dec = dec.ok_or(ChainError::NoDecoder(s.from))?;
    let vpp = s.scale.map(|sc| scaler(sc, input, &a, &b));
    let e = &s.encode;
    let (num, den) = e.fps;
    let enc: Box<dyn Element> = match (e.to, e.rate) {
        // VDEnc on the video engine, the shader encoders hold the render engine at 80% for one
        // 1080p60 stream. The session opens at start, every picture is the plan's box. Two
        // pictures on the GPU at once, the next queued behind the one coding. The reactor
        // wakes the encoder on each picture's fence, so it neither polls nor blocks.
        ("h265/annexb", Rate::Target { kbps, hrd_ms, max_fps, ahead, gpu_depth, low_power }) => {
            let enc = VaapiH265Enc::new_zerocopy(b)
                .with_fps(num, den)
                .with_max_fps(max_fps.0, max_fps.1)
                .with_gop(e.gop)
                .with_bitrate(kbps)
                .with_hrd_ms(hrd_ms)
                .with_low_power(low_power)
                .with_ahead(ahead.0, ahead.1)
                .with_colour(e.colour)
                .with_depth(gpu_depth);
            Box::new(match e.dovi {
                Some((src, quarters)) => enc.with_dovi(src, quarters),
                None => enc,
            })
        }
        (_, Rate::Target { kbps, hrd_ms, max_fps, ahead, gpu_depth, low_power }) => {
            let enc = VaapiH264Enc::new_zerocopy(b)
                .with_fps(num, den)
                .with_max_fps(max_fps.0, max_fps.1)
                .with_gop(e.gop)
                .with_bitrate(kbps)
                .with_hrd_ms(hrd_ms)
                .with_low_power(low_power)
                .with_ahead(ahead.0, ahead.1)
                .with_colour(e.colour)
                .with_depth(gpu_depth);
            Box::new(match &e.keys {
                Some((k, tol)) => enc.with_keys(Arc::clone(k), *tol),
                None => enc,
            })
        }
        (_, Rate::Constant { qp }) => Box::new(
            VaapiH264Enc::new_zerocopy(b)
                .with_fps(num, den)
                .with_gop(e.gop)
                .with_qp(qp)
                .with_colour(e.colour),
        ),
    };
    let name = dec.desc().name;
    let (dec, vpp, enc) = (p.add_boxed(dec), vpp.map(|v| p.add(v)), p.add_boxed(enc));
    if let Decode::Software { handover, threads } = s.decode {
        if let Handover::Udmabuf { align, .. } = handover {
            let (_, allocator) = udmabuf_allocator()?;
            p.set_element_allocator(dec, align, allocator);
        }
        p.set_element_pool(dec, s.dec_pool.0, s.dec_pool.1);
        // libav takes a frame thread count, dav1d keeps its own
        if let Some(n) = threads.filter(|_| name.starts_with("libav")) {
            p.set(dec, "threads", Value::Int(i64::from(n))).map_err(ChainError::Pipeline)?;
        }
    } else {
        p.set_element_pool(dec, s.dec_pool.0, s.dec_pool.1);
    }
    if let Some(vpp) = vpp {
        p.set_element_pool(vpp, s.vpp_pool.0, s.vpp_pool.1);
    }
    p.set_element_pool(enc, s.enc_pool.0, s.enc_pool.1);
    if s.one_group {
        if let Some(vpp) = vpp {
            p.set_element_sched(vpp, SchedHint::Passive);
        }
        p.set_element_sched(enc, SchedHint::Passive);
    }
    Ok(ChainIds { dec, vpp, enc })
}

#[cfg(test)]
mod tests {
    use super::*;
    use profluens_video::chain::{Dmabufs, Scale};
    use profluens_video::color::StreamColor;

    fn spec(shrink: Option<Shrink>, scale: Option<Scale>) -> ChainSpec {
        let to = Picture { width: 1280, height: 720, depth: 8 };
        ChainSpec {
            from: "h264/annexb",
            decode: Decode::Device { shrink, film_grain: true },
            scale,
            encode: profluens_video::chain::Encode {
                to: "h265/annexb",
                fps: (30, 1),
                gop: 60,
                rate: Rate::Target {
                    kbps: 3000,
                    hrd_ms: 1000,
                    max_fps: (0, 1),
                    ahead: (to.width, to.height),
                    gpu_depth: 2,
                    low_power: true,
                },
                colour: StreamColor::default(),
                dovi: None,
                keys: None,
            },
            dec_pool: (1 << 19, 32),
            vpp_pool: (1 << 18, 16),
            enc_pool: (1 << 20, 8),
            one_group: true,
        }
    }

    fn no_software(_: &Arc<GpuFrameChannel>, _: Dmabufs) -> Option<Box<dyn Element>> {
        None
    }

    /// The pools and the one group a spec states land on its elements, the scaler and the
    /// encoder inline behind the decoder.
    #[test]
    fn a_spec_pools_and_schedules_its_elements() {
        let to = Picture { width: 1280, height: 720, depth: 8 };
        let scale = Scale { to, fixed_size: true, turn: Some((90, (1, 1))), warm_tone_map: false };
        let mut p = Pipeline::new();
        let ids = build(&mut p, &spec(None, Some(scale)), &no_software).expect("a chain");
        let vpp = ids.vpp.expect("a scaler");
        assert_eq!(p.element_desc(ids.dec).map(|d| d.name), Some("vaapih264dec"));
        assert_eq!(p.element_desc(ids.enc).map(|d| d.name), Some("vaapih265enc"));
        assert_eq!(p.element_pool(ids.dec), Some((1 << 19, 32)));
        assert_eq!(p.element_pool(vpp), Some((1 << 18, 16)));
        assert_eq!(p.element_pool(ids.enc), Some((1 << 20, 8)));
        assert_eq!(p.element_sched(vpp), Some(SchedHint::Passive));
        assert_eq!(p.element_sched(ids.enc), Some(SchedHint::Passive));
    }

    /// A decoder that makes the picture in its pass has no scaler behind it, and a software
    /// decode with no decoder is refused before anything is added.
    #[test]
    fn a_shrinking_decoder_goes_without_a_scaler_and_a_missing_decoder_adds_nothing() {
        let shrink = Shrink { to: Picture { width: 1280, height: 720, depth: 8 }, fixed: true };
        let mut p = Pipeline::new();
        let ids = build(&mut p, &spec(Some(shrink), None), &no_software).expect("a chain");
        assert_eq!(ids.vpp, None);
        let soft = ChainSpec {
            decode: Decode::Software { handover: Handover::Upload, threads: None },
            ..spec(None, None)
        };
        let before = p.element_count();
        let refused = build(&mut p, &soft, &no_software);
        assert!(matches!(refused, Err(ChainError::NoDecoder("h264/annexb"))), "{refused:?}");
        assert_eq!(p.element_count(), before);
    }
}
