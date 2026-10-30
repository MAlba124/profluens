//! A zero-copy decoder offers what its driver exports, asked at preroll. Skips without a device.

// Test code.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::sync::Arc;

use pf_harness::Harness;
use pf_mkv::{MatroskaReader, Reframer, nal_head_from_config};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, LinkError};
use profluens_core::format::{OfferDesc, Value, ValueDesc};
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

use pf_vaapi::ffi::DRM_FORMAT_NV12;
use pf_vaapi::gpuframe::{self, DmabufFormat, F_MODIFIER, FAMILY_VIDEO_GPU, GpuFrameChannel};
use pf_vaapi::va::{Display, ExportedSurface};
use pf_vaapi::{VaapiError, VaapiErrorKind};

const DEC_OUT: SrcPad = SrcPad(0);
const MADE_UP: u64 = 0x7700_0000_0000_0042;

static IMPORT_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: OfferDesc::PROBED }];
static IMPORTER: ElementDesc = ElementDesc {
    name: "importer",
    sinks: &IMPORT_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct Importer(Vec<u64>);

impl Element for Importer {
    fn desc(&self) -> &'static ElementDesc {
        &IMPORTER
    }
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let formats = [DmabufFormat { fourcc: DRM_FORMAT_NV12, modifiers: self.0.clone() }];
        gpuframe::with_gpu_offers(&formats, |offers| ctx.offer_sink(SinkPad(0), offers))
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {}
        Ok(Flow::Ok)
    }
}

fn device() -> Option<(u64, Vec<&'static str>)> {
    let caps = pf_vaapi::probe()?;
    let display = Display::open(&caps.device).ok()?;
    let (fourcc, modifier) = ExportedSurface::probe(&display).ok()?;
    assert_eq!(fourcc, DRM_FORMAT_NV12, "a decode surface exports as NV12");
    let families = ["h264/annexb", "h265/annexb"].into_iter().filter(|f| caps.supports(f));
    Some((modifier, families.collect()))
}

fn add_decoder(p: &mut Pipeline, family: &str) -> ElementId {
    let decoder = pf_vaapi::video_decoder_zerocopy_for(family, GpuFrameChannel::new());
    p.add_boxed(decoder.expect("the probe says the family decodes"))
}

#[test]
fn a_zero_copy_decoder_offers_the_modifier_its_driver_exports() {
    let Some((exported, families)) = device() else {
        eprintln!("skip: no VA-API device that exports");
        return;
    };
    eprintln!("driver exports NV12 with modifier {exported:#018x}");
    for family in families {
        let mut p = Pipeline::new();
        let dec = add_decoder(&mut p, family);
        let importer = p.add(Importer(vec![MADE_UP, exported]));
        let pending = p.link(dec.src(DEC_OUT), importer.sink(SinkPad(0)));
        assert!(
            matches!(&pending, Err(Error::Link(LinkError::OffersPending { element, pad, .. }))
                if *element == dec && &**pad == "src"),
            "{family}: {pending:?}"
        );

        p.preroll().expect("preroll");
        let link = p.link(dec.src(DEC_OUT), importer.sink(SinkPad(0))).expect("links");
        let format = p.negotiated(link).expect("fixed");
        assert_eq!(p.family_name(format.family), Some(FAMILY_VIDEO_GPU));
        let modifier = format.get(p.field_id(F_MODIFIER).expect("interned"));
        assert_eq!(modifier, Some(Value::Int(exported as i64)), "{family}");
        let dot = p.dump_dot();
        assert!(dot.contains(&format!("modifier={}", exported as i64)), "{dot}");
    }
}

#[test]
fn an_importer_without_the_exported_modifier_is_refused_at_link() {
    let Some((exported, families)) = device() else {
        eprintln!("skip: no VA-API device that exports");
        return;
    };
    for family in families {
        let mut p = Pipeline::new();
        let dec = add_decoder(&mut p, family);
        let importer = p.add(Importer(vec![MADE_UP]));
        p.preroll().expect("preroll");
        let refused = p.link(dec.src(DEC_OUT), importer.sink(SinkPad(0)));
        let Err(Error::Link(LinkError::NoCommonFormat(ends))) = &refused else {
            panic!("{family}: expected no common format, got {refused:?}");
        };
        let want = |m: u64| format!("layout=nv12-dmabuf, modifier={{{}}}", m as i64);
        assert!(!ends.src.offers.is_empty() && ends.src.offers.len() <= 2, "{:?}", ends.src.offers);
        assert!(ends.src.offers[0].contains(&want(exported)), "{:?}", ends.src.offers);
        for later in &ends.src.offers[1..] {
            assert!(later.contains("layout=p010-dmabuf, modifier={"), "{later}");
        }
        assert!(ends.sink.offers[0].contains(&want(MADE_UP)), "{:?}", ends.sink.offers);
    }
}

#[test]
fn a_readback_decoder_still_links_without_preroll() {
    if pf_vaapi::probe().is_none_or(|caps| !caps.supports("h264/annexb")) {
        eprintln!("skip: no VA-API H.264 decode");
        return;
    }
    static RAW: [OfferDesc; 1] = [OfferDesc::any("video/raw")];
    static RAW_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW }];
    static RAW_SINK: ElementDesc = ElementDesc {
        name: "rawsink",
        sinks: &RAW_PADS,
        srcs: &[],
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    };
    struct RawSink;
    impl Element for RawSink {
        fn desc(&self) -> &'static ElementDesc {
            &RAW_SINK
        }
        fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
            Ok(Flow::Ok)
        }
    }
    let mut p = Pipeline::new();
    let dec = p.add(pf_vaapi::VaapiH264Dec::new());
    let sink = p.add(RawSink);
    p.link(dec.src(DEC_OUT), sink.sink(SinkPad(0))).expect("a static menu needs no preroll");
}

const FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/out/mkv_h264_aac.mkv");

fn annexb_aus() -> Option<Vec<Vec<u8>>> {
    let data = std::fs::read(Path::new(FIXTURE)).ok()?;
    let mut reader = MatroskaReader::new();
    reader.push(&data).ok()?;
    let track = reader.tracks().iter().find(|t| t.codec_id == "V_MPEG4/ISO/AVC")?.clone();
    let (head, length_size) = nal_head_from_config(&track.codec_private, false).ok()?;
    let reframer = Reframer::Nal { length_size };
    let (mut aus, mut scratch) = (Vec::new(), Vec::new());
    while let Some(frame) = reader.next_frame() {
        if frame.track_number != track.track_number {
            continue;
        }
        let mut au = if aus.is_empty() { head.clone() } else { Vec::new() };
        au.extend_from_slice(reframer.reframe_into(&frame.data, &mut scratch).ok()?);
        aus.push(au);
    }
    (!aus.is_empty()).then_some(aus)
}

#[test]
fn a_surface_exported_with_another_modifier_than_the_link_fixed_fails_the_decoder() {
    let Some((exported, families)) = device() else {
        eprintln!("skip: no VA-API device that exports");
        return;
    };
    let Some(aus) = families.contains(&"h264/annexb").then(annexb_aus).flatten() else {
        eprintln!("skip: no H.264 decode, or fixture {FIXTURE} absent (cargo xtask fixtures)");
        return;
    };
    let run = |negotiated: u64| -> Result<usize, Error> {
        let dec = pf_vaapi::VaapiH264Dec::new_zerocopy(Arc::clone(&GpuFrameChannel::new()));
        let mut h = Harness::new(dec);
        let fixed = [(F_MODIFIER, ValueDesc::Int(negotiated as i64))];
        h.fix_src_format(DEC_OUT, FAMILY_VIDEO_GPU, &fixed);
        h.start()?;
        let mut frames = 0;
        for au in aus.iter().take(12) {
            let buf = h.alloc(au);
            h.push(buf)?;
            frames += std::iter::from_fn(|| h.pull(DEC_OUT)).count();
        }
        Ok(frames + h.eos()?.len())
    };

    assert!(run(exported).expect("the driver kept its word") > 0, "frames were exported");
    let failed = run(MADE_UP).expect_err("the first export has the wrong modifier");
    let Error::ElementFailed { source, .. } = &failed else { panic!("{failed:?}") };
    let kind = source.downcast_ref::<VaapiError>().map(|e| e.kind);
    let want = VaapiErrorKind::ModifierChanged { negotiated: MADE_UP, exported };
    assert_eq!(kind, Some(want), "{failed}");
}
