//! A zero-copy decoder whose surfaces are all held downstream parks until one comes back.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pf_harness::Harness;
use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::ffi::DRM_FORMAT_NV12;
use pf_vaapi::gpuframe::{self, DmabufFormat, FrameRx, GpuFrame, GpuFrameChannel, GpuFrameHeader};
use pf_vaapi::va::{Display, ExportedSurface};
use pf_vaapi::{VaapiH264Dec, VaapiH265Dec};

const H264: &[u8] = include_bytes!("fixtures/ipb_320x240.h264");
const H265: &[u8] = include_bytes!("fixtures/ipb_320x240.hevc");
const H264_ORDER: [u64; 20] =
    [0, 4, 2, 1, 3, 8, 6, 5, 7, 12, 10, 9, 11, 15, 13, 14, 16, 19, 17, 18];
const H265_ORDER: [u64; 20] =
    [0, 4, 2, 1, 3, 8, 6, 5, 7, 12, 10, 9, 11, 15, 14, 13, 16, 19, 18, 17];
const FRAME_NS: u64 = 40_000_000;
const LOOPS: u64 = 3;
const PICTURES: u64 = 20 * LOOPS;
const SLOTS: u32 = 64;
const SLOT: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Codec {
    H264,
    H265,
}

impl Codec {
    fn family(self) -> &'static str {
        if self == Codec::H264 { "h264/annexb" } else { "h265/annexb" }
    }

    fn order(self) -> &'static [u64; 20] {
        if self == Codec::H264 { &H264_ORDER } else { &H265_ORDER }
    }

    fn opens_picture(self, data: &[u8], at: usize) -> bool {
        match self {
            Codec::H264 => matches!(data[at] & 0x1F, 1 | 5) && data[at + 1] & 0x80 != 0,
            Codec::H265 => (data[at] >> 1) & 0x3F < 32 && data[at + 2] & 0x80 != 0,
        }
    }

    fn units(self) -> Vec<Vec<u8>> {
        let data = if self == Codec::H264 { H264 } else { H265 };
        let mut cuts = vec![0];
        let (mut open, mut head) = (false, 0);
        for i in 0..data.len().saturating_sub(3) {
            if data[i..i + 3] != [0, 0, 1] {
                continue;
            }
            let code = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            if self.opens_picture(data, i + 3) {
                if open {
                    cuts.push(head);
                }
                (open, head) = (true, usize::MAX);
            } else if head == usize::MAX {
                head = code;
            }
        }
        let ends = cuts.iter().skip(1).copied().chain([data.len()]);
        let units: Vec<Vec<u8>> =
            cuts.iter().zip(ends).map(|(&a, b)| data[a..b].to_vec()).collect();
        assert_eq!(units.len(), 20, "{self:?}");
        units
    }

    fn stream(self) -> Vec<(Vec<u8>, Timestamp)> {
        let units = self.units();
        let pts =
            |round: u64, i: usize| Timestamp::from_nanos((round * 20 + self.order()[i]) * FRAME_NS);
        (0..LOOPS)
            .flat_map(|round| {
                units.iter().enumerate().map(move |(i, u)| (u.clone(), pts(round, i)))
            })
            .collect()
    }

    fn decoder(self, channel: &Arc<GpuFrameChannel>) -> Box<dyn Element> {
        match self {
            Codec::H264 => Box::new(VaapiH264Dec::new_zerocopy(Arc::clone(channel))),
            Codec::H265 => Box::new(VaapiH265Dec::new_zerocopy(Arc::clone(channel))),
        }
    }
}

fn device(codec: Codec, test: &str) -> Option<u64> {
    let found = pf_vaapi::probe().filter(|caps| caps.supports(codec.family())).and_then(|caps| {
        let display = Display::open(&caps.device).ok()?;
        ExportedSurface::probe(&display).ok()
    });
    if found.is_none() {
        eprintln!("skip {test} for {codec:?}, no VA-API device decodes and exports it");
    }
    found.map(|(_, modifier)| modifier)
}

fn token_of(bytes: &[u8]) -> u64 {
    GpuFrameHeader::from_bytes(bytes).expect("a video/gpu header").token
}

fn take(
    h: &mut Harness,
    frames: &mut FrameRx,
    shown: &mut Vec<(Timestamp, u64)>,
    held: &mut VecDeque<GpuFrame>,
) {
    for buf in h.drain_outputs() {
        let token = token_of(buf.memory.data());
        let mut frame = frames.take(token).expect("the frame of a token put out");
        frame.close_fds();
        shown.push((buf.pts, token));
        held.push_back(frame);
    }
}

#[test]
fn a_decoder_with_every_surface_out_takes_no_input_and_drops_nothing() {
    for codec in [Codec::H264, Codec::H265] {
        let test = "a_decoder_with_every_surface_out";
        if device(codec, test).is_none() {
            continue;
        }
        let channel = GpuFrameChannel::new();
        let mut h = match codec {
            Codec::H264 => {
                Harness::with_pool(VaapiH264Dec::new_zerocopy(Arc::clone(&channel)), SLOT, SLOTS)
            }
            Codec::H265 => {
                Harness::with_pool(VaapiH265Dec::new_zerocopy(Arc::clone(&channel)), SLOT, SLOTS)
            }
        };
        let mut frames = channel.take_rx().expect("the sink's end");
        let mut shown: Vec<(Timestamp, u64)> = Vec::new();
        let mut held: VecDeque<GpuFrame> = VecDeque::new();
        let mut most_waiting = 0;
        for (unit, pts) in codec.stream() {
            let mut buf = h.alloc(&unit);
            buf.pts = pts;
            h.push(buf).expect("push");
            take(&mut h, &mut frames, &mut shown, &mut held);
            most_waiting = most_waiting.max(h.input_len());
        }
        assert!(most_waiting > 0, "{codec:?} never ran out of surfaces, {} held", held.len());
        let out_at_the_stall = held.len();
        let mut rounds = 0;
        while h.input_len() > 0 {
            drop(held.pop_front().expect("input waits only for a surface that is out"));
            h.crank().expect("crank");
            take(&mut h, &mut frames, &mut shown, &mut held);
            rounds += 1;
            assert!(rounds < 1_000, "{codec:?} takes no input though surfaces came back");
        }
        held.clear();
        for buf in h.eos().expect("eos") {
            shown.push((buf.pts, token_of(buf.memory.data())));
        }
        let warnings: Vec<String> = h
            .bus_messages()
            .into_iter()
            .filter_map(|m| match m {
                BusMessage::Warning { error, .. } => Some(format!("{error}")),
                _ => None,
            })
            // The harness runs no preroll so the video/gpu announcement warns once.
            .filter(|warning| !warning.starts_with("format announcement 'video/gpu'"))
            .collect();
        assert!(warnings.is_empty(), "{codec:?} warned, {warnings:?}");
        let want: Vec<Timestamp> =
            (0..PICTURES).map(|n| Timestamp::from_nanos(n * FRAME_NS)).collect();
        let got: Vec<Timestamp> = shown.iter().map(|(pts, _)| *pts).collect();
        assert_eq!(got, want, "{codec:?}, every picture once and in display order");
        eprintln!(
            "{codec:?}: {out_at_the_stall} surfaces out when the decoder stopped, {most_waiting} units waited"
        );
    }
}

static SRC_OFFERS: [OfferDesc; 2] = [OfferDesc::any("h264/annexb"), OfferDesc::any("h265/annexb")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "unitsrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct UnitSrc {
    units: Vec<(Vec<u8>, Timestamp)>,
    next: usize,
}

impl Element for UnitSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let Some((unit, pts)) = self.units.get(self.next) else {
            return Ok(Flow::Eos);
        };
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.as_mut_full()[..unit.len()].copy_from_slice(unit);
        buf.memory.set_len(unit.len());
        buf.pts = *pts;
        ctx.out(SrcPad(0)).push(buf);
        self.next += 1;
        Ok(Flow::Ok)
    }
}

static HOLD_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: OfferDesc::PROBED }];
static HOLD_DESC: ElementDesc = ElementDesc {
    name: "holdsink",
    sinks: &HOLD_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct HoldSink {
    modifier: u64,
    channel: Arc<GpuFrameChannel>,
    frames: Option<FrameRx>,
    taken: mpsc::Sender<(Timestamp, GpuFrame)>,
}

impl Element for HoldSink {
    fn desc(&self) -> &'static ElementDesc {
        &HOLD_DESC
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let formats = [DmabufFormat { fourcc: DRM_FORMAT_NV12, modifiers: vec![self.modifier] }];
        gpuframe::with_gpu_offers(&formats, |offers| ctx.offer_sink(SinkPad(0), offers))
    }

    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.frames = self.channel.take_rx();
        Ok(())
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let token = token_of(buf.memory.data());
            let frame = self.frames.as_mut().and_then(|frames| frames.take(token));
            let mut frame = frame.expect("the frame of a token put out");
            frame.close_fds();
            let _ = self.taken.send((buf.pts, frame));
        }
        Ok(Flow::Ok)
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        if let Some(frames) = self.frames.take() {
            self.channel.put_rx(frames);
        }
    }
}

const HOLD: Duration = Duration::from_millis(400);

#[test]
fn a_decoder_waiting_for_a_surface_is_woken_by_its_release() {
    for codec in [Codec::H264, Codec::H265] {
        let Some(modifier) = device(codec, "a_decoder_waiting_for_a_surface") else {
            continue;
        };
        let channel = GpuFrameChannel::new();
        let (taken, tokens) = mpsc::channel::<(Timestamp, GpuFrame)>();
        let mut p = Pipeline::new();
        p.set_pool(64 * 1024, 16);
        let src = p.add(UnitSrc { units: codec.stream(), next: 0 });
        let dec = p.add_boxed(codec.decoder(&channel));
        let sink = p.add(HoldSink { modifier, channel: Arc::clone(&channel), frames: None, taken });
        p.preroll().expect("preroll");
        p.link(dec.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("decoder -> sink");
        p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src -> decoder");
        p.set_element_pool(dec, SLOT, SLOTS);

        let started = Instant::now();
        let releaser = {
            std::thread::spawn(move || {
                let mut shown = Vec::new();
                let mut held = 0usize;
                let mut most = 0usize;
                let mut pending: VecDeque<GpuFrame> = VecDeque::new();
                loop {
                    match tokens.recv_timeout(Duration::from_millis(1)) {
                        Ok((pts, frame)) => {
                            shown.push(pts);
                            pending.push_back(frame);
                            held += 1;
                            most = most.max(held);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    if started.elapsed() >= HOLD && pending.pop_front().is_some() {
                        held -= 1;
                    }
                }
                (shown, most)
            })
        };
        p.run().expect("the run ends");
        let waited = started.elapsed();
        let sched = p.control().scheduler_stats();
        let mut warnings = Vec::new();
        while let Some(message) = p.bus().try_recv() {
            if let BusMessage::Warning { error, .. } = message {
                warnings.push(format!("{error}"));
            }
        }
        drop(p);
        let (shown, most) = releaser.join().expect("the releasing thread");
        assert!(warnings.is_empty(), "{codec:?} warned, {warnings:?}");
        let want: Vec<Timestamp> =
            (0..PICTURES).map(|n| Timestamp::from_nanos(n * FRAME_NS)).collect();
        assert_eq!(shown, want, "{codec:?}, every picture once and in display order");
        assert!(waited >= HOLD, "{codec:?} ended in {waited:?}, before a surface came back");
        // Each park has a 10 ms backstop. A pass after one that finds work is an unsignaled wake.
        assert!(
            sched.late_wakes <= 3,
            "{codec:?} found {} wakes at the backstop, {sched:?}",
            sched.late_wakes
        );
        eprintln!("{codec:?}: {most} surfaces out at once, the run took {waited:?}, {sched:?}");
    }
}

fn surface_of(frame: &GpuFrame) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let fd = frame.fd(0).and_then(|fd| fd.try_clone_to_owned().ok()).expect("an open dmabuf");
    std::fs::File::from(fd).metadata().expect("fstat of a dmabuf").ino()
}

#[test]
fn a_flush_leaves_the_surface_of_a_frame_on_screen_alone() {
    for codec in [Codec::H264, Codec::H265] {
        if device(codec, "a_flush_leaves_the_surface_of_a_frame_on_screen_alone").is_none() {
            continue;
        }
        let channel = GpuFrameChannel::new();
        let mut h = match codec {
            Codec::H264 => {
                Harness::with_pool(VaapiH264Dec::new_zerocopy(Arc::clone(&channel)), SLOT, SLOTS)
            }
            Codec::H265 => {
                Harness::with_pool(VaapiH265Dec::new_zerocopy(Arc::clone(&channel)), SLOT, SLOTS)
            }
        };
        let mut frames = channel.take_rx().expect("the sink's end");
        let units = codec.units();
        let feed = |h: &mut Harness, frames: &mut FrameRx, out: &mut Vec<GpuFrame>| {
            for (unit, at) in units.iter().zip(codec.order()) {
                let mut buf = h.alloc(unit);
                buf.pts = Timestamp::from_nanos(at * FRAME_NS);
                h.push(buf).expect("push");
                for buf in h.drain_outputs() {
                    out.push(
                        frames.take(token_of(buf.memory.data())).expect("the frame of a token"),
                    );
                }
            }
        };
        let mut before = Vec::new();
        feed(&mut h, &mut frames, &mut before);
        let shown = before.pop().expect("pictures before the seek");
        drop(before);
        let held = surface_of(&shown);
        h.seek(0, Timestamp::ZERO).expect("seek");
        let mut after = Vec::new();
        feed(&mut h, &mut frames, &mut after);
        let on_it = after.iter().filter(|f| surface_of(f) == held).count();
        assert_eq!(
            on_it,
            0,
            "{codec:?}, {on_it} of {} pictures decoded into the surface on screen",
            after.len()
        );
        drop(shown);
        drop(after);
        let mut again = Vec::new();
        let mut served = false;
        for _ in 0..4 {
            h.seek(0, Timestamp::ZERO).expect("seek");
            feed(&mut h, &mut frames, &mut again);
            served |= again.iter().any(|f| surface_of(f) == held);
            again.clear();
        }
        assert!(served, "{codec:?}, the surface came back to the decoder");
    }
}
