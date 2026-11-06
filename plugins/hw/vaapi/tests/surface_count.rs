//! The VA surfaces a zero-copy transcode makes, decoder, twins and encoder. A same-size plan
//! without bars makes no twins, a zero-copy encoder no input surface. `SURFACES_STREAM=FILE`
//! runs a 1920x1080 H.264 Annex B file through the streamer's chain and prints what each DRM
//! client of the process holds, from `/proc/self/fdinfo`. Skips without the hardware.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::stats::Snapshot;
use pf_vaapi::vpp::Target;
use pf_vaapi::{VaapiH264Dec, VaapiH264Enc};

use chain::{Codec, UnitSrc};

const IPB: &[u8] = include_bytes!("fixtures/ipb_320x240.h264");
/// The picture the probe reads the clients at, every session open by then.
const PROBE_AT: usize = 10;

static TAKEN: [OfferDesc; 1] = [OfferDesc::any("h264/annexb")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &TAKEN }];
static PROBE_DESC: ElementDesc = ElementDesc {
    name: "probe",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

/// Counts the coded pictures and reads the DRM clients once, at [`PROBE_AT`].
struct Probe {
    seen: usize,
    tx: mpsc::Sender<Vec<Client>>,
}

impl Element for Probe {
    fn desc(&self) -> &'static ElementDesc {
        &PROBE_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {
            self.seen += 1;
            if self.seen == PROBE_AT {
                let _ = self.tx.send(clients());
            }
        }
        Ok(Flow::Ok)
    }
}

/// One DRM client's memory, KiB, from its fdinfo.
#[derive(Debug, Clone, Copy)]
struct Client {
    fd: u32,
    total: u64,
    shared: u64,
}

fn kib(v: &str) -> Option<u64> {
    let mut it = v.split_whitespace();
    let n: u64 = it.next()?.parse().ok()?;
    Some(match it.next() {
        Some("KiB") => n,
        Some("MiB") => n << 10,
        Some("GiB") => n << 20,
        _ => n / 1024,
    })
}

/// The process's render node clients, an element's display each.
fn clients() -> Vec<Client> {
    let Ok(dir) = std::fs::read_dir("/proc/self/fd") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in dir.flatten() {
        let link = std::fs::read_link(e.path()).unwrap_or_default();
        if !link.to_string_lossy().starts_with("/dev/dri/") {
            continue;
        }
        let Some(fd) = e.file_name().to_str().and_then(|s| s.parse().ok()) else { continue };
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap_or_default();
        let field =
            |name: &str| info.lines().find_map(|l| l.strip_prefix(name)).and_then(kib).unwrap_or(0);
        let (total, shared) = (field("drm-total-system0:"), field("drm-shared-system0:"));
        if info.contains("drm-client-id") {
            out.push(Client { fd, total, shared });
        }
    }
    out.sort_by_key(|c| c.fd);
    out
}

struct Run {
    dec: Snapshot,
    enc: Snapshot,
    coded: usize,
    clients: Vec<Client>,
}

/// `units` through a zero-copy H.264 decoder that plans into `target`'s fixed box, and a
/// zero-copy H.264 encoder two pictures deep, the streamer's chain.
fn transcode(units: Vec<Vec<u8>>, target: Target) -> Run {
    let n = units.len();
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(units));
    let ch = GpuFrameChannel::queued();
    let dec = VaapiH264Dec::new_zerocopy(Arc::clone(&ch)).with_shrink(target, true);
    let enc =
        VaapiH264Enc::new_zerocopy(ch).with_qp(30).with_gop(30).with_low_power(true).with_depth(2);
    let (dec_stats, enc_stats) = (dec.stats(), enc.stats());
    let dec = p.add(dec);
    let enc = p.add(enc);
    let sink = p.add(Probe { seen: 0, tx });
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), enc.sink(SinkPad(0))).expect("decoder to encoder");
    p.link(enc.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("encoder to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    let (w, h) = (target.max_width as usize, target.max_height as usize);
    p.set_element_pool(enc, w * h * 3 / 2 + (64 << 10), 8);
    p.run().expect("the run ends");
    let warnings = chain::warnings(&p);
    assert!(warnings.is_empty(), "warned {warnings:?}");
    drop(p);
    let clients = rx.try_iter().next().unwrap_or_default();
    let (dec, enc) = (dec_stats.snapshot(), enc_stats.snapshot());
    assert_eq!(enc.frames as usize, n, "every picture coded");
    Run { dec, enc, coded: n, clients }
}

fn hardware(test: &str) -> bool {
    let ok = pf_vaapi::probe().is_some_and(|c| {
        c.shrinks("h264/annexb") && c.supports_encode("h264/annexb") && c.video_proc
    });
    if !ok {
        eprintln!("skip {test}: no VA-API H.264 decode processing and encode");
    }
    ok
}

fn t(max_width: u32, max_height: u32) -> Target {
    Target { max_width, max_height, depth: 8 }
}

#[test]
fn a_streamer_transcode_at_1080p_prints_its_surfaces_and_gpu_memory() {
    let test = "surfaces_1080p";
    let Some(path) = std::env::var_os("SURFACES_STREAM") else {
        eprintln!("skip {test}: SURFACES_STREAM unset");
        return;
    };
    if !hardware(test) {
        return;
    }
    let stream = std::fs::read(&path).expect("the stream");
    let run = transcode(Codec::H264.units(&stream), t(1920, 1080));
    let total: u64 = run.clients.iter().map(|c| c.total).sum();
    for c in &run.clients {
        eprintln!("{test}: fd {} total {} KiB shared {} KiB", c.fd, c.total, c.shared);
    }
    eprintln!(
        "{test}: {} pictures, decoder {:?}, encoder {:?}, clients total {total} KiB",
        run.coded, run.dec, run.enc
    );
}

/// Raw pictures through an H.264 encoder, its stats after.
fn raw_encode(w: u32, h: u32, n: u64) -> Snapshot {
    use profluens_core::format::ValueDesc;
    let enc = VaapiH264Enc::new().with_qp(30).with_gop(30).with_low_power(true).with_depth(2);
    let stats = enc.stats();
    let mut hx = pf_harness::Harness::with_pool(enc, 256 * 1024, 16);
    let raw = [
        ("width", ValueDesc::Int(w.into())),
        ("height", ValueDesc::Int(h.into())),
        ("pixfmt", ValueDesc::Id("nv12")),
        ("fps", ValueDesc::Rat(25, 1)),
    ];
    hx.fix_sink_format(SinkPad(0), "video/raw", &raw);
    let picture = vec![128u8; (w * h * 3 / 2) as usize];
    for i in 0..n {
        let mut b = hx.alloc(&picture);
        b.pts = profluens_core::time::Timestamp::from_nanos(i * chain::FRAME_NS);
        hx.push(b).expect("pushed");
        hx.drain_outputs();
    }
    hx.eos().expect("the end");
    stats.snapshot()
}

/// The references and reorder the decoder reads from the fixture's SPS.
const IPB_REFS: u32 = 4;
const IPB_REORDER: u32 = 2;

/// At the decoded size without bars the decoder hands on its own surfaces and makes no twins,
/// a smaller or barred box a twin each, and the zero-copy encoder makes its references alone,
/// one surface fewer than the same encoder fed raw pictures.
#[test]
fn a_same_size_transcode_makes_no_twins_and_no_encoder_input() {
    if !hardware("same_size_surfaces") {
        return;
    }
    let decoded = pf_vaapi::va::decode_surface_count(IPB_REFS, IPB_REORDER, true);
    let same = transcode(Codec::H264.units(IPB), t(320, 240));
    assert_eq!((same.dec.surfaces, same.dec.twins), (u64::from(decoded), 0), "same size");
    for (box_, what) in [(t(160, 120), "smaller"), (t(320, 320), "barred")] {
        let run = transcode(Codec::H264.units(IPB), box_);
        let made = (run.dec.surfaces, run.dec.twins);
        assert_eq!(made, (2 * u64::from(decoded), u64::from(decoded)), "{what}");
    }
    let raw = raw_encode(320, 240, 4);
    assert_eq!(same.enc.surfaces + 1, raw.surfaces, "the raw encoder's upload surface alone");
}
