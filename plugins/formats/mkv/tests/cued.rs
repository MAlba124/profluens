//! A selection of subtitle tracks alone is read where the Cues place its blocks.

// Test code. The allocation ban is for element hot paths.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pf_mkv::ebml::id;
use pf_mkv::{MatroskaWriter, MkvDemux, TrackConfig, parse_cue_tracks, parse_seek_head};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pull::Ask;

const MS: u64 = 1_000_000;
const PICTURE: usize = 20_000;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SRC: ElementDesc = ElementDesc {
    name: "memsrc",
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
static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static REC_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static REC: ElementDesc = ElementDesc {
    name: "rec",
    sinks: &REC_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct MemSrc {
    data: Vec<u8>,
    pull: bool,
    pos: usize,
    served: Arc<AtomicU64>,
}

impl Element for MemSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.pull {
            ctx.offer_ranges(SrcPad(0));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pull {
            while let Some(Ask { offset, len, tag, mut buf }) = ctx.next_ask(SrcPad(0)) {
                let from = (offset as usize).min(self.data.len());
                let to = (from + len as usize).min(self.data.len());
                buf.memory.as_mut_full()[..to - from].copy_from_slice(&self.data[from..to]);
                buf.memory.set_len(to - from);
                self.served.fetch_add((to - from) as u64, Ordering::Relaxed);
                ctx.serve(SrcPad(0), tag, buf);
            }
            return Ok(if ctx.asks_over(SrcPad(0)) { Flow::Eos } else { Flow::Ok });
        }
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = buf.memory.capacity().min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        self.served.fetch_add(n as u64, Ordering::Relaxed);
        ctx.out(SrcPad(0)).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }
}

type Frames = Vec<(u64, Vec<u8>)>;
type Got = Arc<Mutex<Frames>>;

struct Rec(Got);

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &REC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.0.lock().unwrap().push((buf.pts.0, buf.memory.data().to_vec()));
        }
        Ok(Flow::Ok)
    }
}

fn film() -> Vec<u8> {
    film_of(1_000, 1_200, PICTURE)
}

fn film_of(key: u64, every: u64, picture: usize) -> Vec<u8> {
    let text = TrackConfig::audio(2, "S_TEXT/UTF8", Vec::new(), 0.0, 0, 0);
    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240), text]);
    w.enable_cues();
    w.set_duration_ns(10_000 * MS);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    let picture = vec![7u8; picture];
    for t in (0..10_000u64).step_by(40) {
        w.write_frame(&mut out, 1, t * MS, &picture, t % key == 0).unwrap();
        if t % every == 0 {
            w.write_frame(&mut out, 2, t * MS, format!("caption {}", t / every).as_bytes(), true)
                .unwrap();
        }
    }
    w.finalize(&mut out);
    out
}

fn header(file: &[u8]) -> Vec<u8> {
    file[..file.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster")].to_vec()
}

fn run(file: &[u8], pull: bool, tracks: &[u64]) -> (Vec<Frames>, u64) {
    run_with(file, pull, tracks, None)
}

fn run_with(file: &[u8], pull: bool, tracks: &[u64], slots: Option<u32>) -> (Vec<Frames>, u64) {
    let mut p = pf_reactor::pipeline();
    if let Some(n) = slots {
        p.set_pool(4096, n);
    }
    let served = Arc::new(AtomicU64::new(0));
    let src = p.add(MemSrc { data: file.to_vec(), pull, pos: 0, served: Arc::clone(&served) });
    let demux = p.add(MkvDemux::new(header(file)));
    p.link(src.src(SrcPad(0)), demux.sink(MkvDemux::IN)).expect("src ! demux");
    let pads = p.preroll().expect("preroll").srcs;
    let mut got = Vec::new();
    for (n, pad) in pads.iter().enumerate().filter(|(n, _)| tracks.contains(&(*n as u64 + 1))) {
        let g: Got = Arc::default();
        let rec = p.add(Rec(Arc::clone(&g)));
        p.link(pad.element.src(pad.pad), rec.sink(SinkPad(0)))
            .unwrap_or_else(|e| panic!("pad {n}: {e}"));
        got.push(g);
    }
    p.run().expect("run");
    let got = got.iter().map(|g| g.lock().unwrap().clone()).collect();
    (got, served.load(Ordering::Relaxed))
}

#[test]
fn the_writer_cues_every_subtitle_block_with_its_place() {
    let file = film();
    let info = parse_seek_head(&header(&file)).expect("a SeekHead");
    let at = (info.segment_data_start + info.cues_pos.expect("Cues")) as usize;
    let cues = parse_cue_tracks(&file[at..], info.segment_data_start, info.timestamp_scale);
    let text: Vec<_> = cues.iter().filter(|c| c.track == 2).collect();
    assert_eq!(text.len(), 9, "a caption every 1.2 s over 10 s");
    assert!(text.iter().all(|c| c.relative.is_some()));
    assert!(
        cues.iter().filter(|c| c.track == 1).all(|c| c.relative.is_none()),
        "keyframes cue their Cluster"
    );
}

#[test]
fn subtitles_alone_are_read_at_their_cues_and_the_same_as_walked() {
    let file = film();
    let (pushed, _) = run(&file, false, &[2]);
    let (cued, served) = run(&file, true, &[2]);
    let (walked, walked_served) = run(&file, true, &[1, 2]);
    let captions: Vec<(u64, String)> =
        pushed[0].iter().map(|(t, b)| (t / MS, String::from_utf8_lossy(b).into_owned())).collect();
    assert_eq!(captions.len(), 9, "{captions:?}");
    assert_eq!(captions[1], (1_200, "caption 1".to_string()));
    assert_eq!(cued[0], pushed[0], "read at the cues as walked pushed");
    assert_eq!(walked[1], pushed[0], "walked pulled with the picture");
    let len = file.len() as u64;
    assert!(served < len / 100, "{served} of {len} read");
    assert!(walked_served > len / 2, "a walk reads the file, {walked_served} of {len}");
}

fn off_by_one(file: &[u8]) -> Vec<u8> {
    let info = parse_seek_head(&header(file)).expect("a SeekHead");
    let at = (info.segment_data_start + info.cues_pos.expect("Cues")) as usize;
    let mut out = file.to_vec();
    let mut i = at;
    while i + 2 < out.len() {
        if out[i] == id::CUE_RELATIVE_POSITION[0] && out[i + 1] == 0x81 && out[i + 2] < 255 {
            out[i + 2] += 1;
            i += 3;
        } else if out[i] == id::CUE_RELATIVE_POSITION[0] && out[i + 1] == 0x82 {
            let v = u16::from_be_bytes([out[i + 2], out[i + 3]]).wrapping_add(1);
            out[i + 2..i + 4].copy_from_slice(&v.to_be_bytes());
            i += 4;
        } else {
            i += 1;
        }
    }
    out
}

#[test]
fn positions_that_are_off_have_the_file_walked() {
    let file = film();
    let bad = off_by_one(&file);
    let (pushed, _) = run(&file, false, &[2]);
    let (got, served) = run(&bad, true, &[2]);
    assert_eq!(got[0], pushed[0], "every caption, once");
    assert!(served > bad.len() as u64 / 2, "walked, {served} read");
}

#[test]
fn more_captions_than_buffers_all_come_out() {
    let file = film_of(80, 80, 600);
    let (pushed, _) = run_with(&file, false, &[2], Some(16));
    let (cued, _) = run_with(&file, true, &[2], Some(16));
    assert_eq!(pushed[0].len(), 125);
    assert_eq!(cued[0], pushed[0]);
}
