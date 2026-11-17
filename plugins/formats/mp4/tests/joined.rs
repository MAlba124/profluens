//! A pulled `mp4demux` asks a source that reads in order for few long ranges and one that reads
//! at once for a range a reframed sample, and puts out what a pushed one does either way.

// Tests own their fixtures and their recordings.
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pf_mp4::{Mp4Demux, Mp4Reader};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::pull::Ask;

/// 30 s of H.264 beside AAC as ffmpeg writes it with the `moov` in front, None without ffmpeg.
fn movie() -> Option<Arc<Vec<u8>>> {
    let path = std::env::temp_dir().join(format!("pf_mp4_joined_{}.mp4", std::process::id()));
    let ok = std::process::Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=d=30:s=320x240:r=24"])
        .args(["-f", "lavfi", "-i", "sine=d=30:r=48000", "-map", "0:v", "-map", "1:a"])
        .args(["-pix_fmt", "yuv420p", "-c:v", "libx264", "-c:a", "aac"])
        .args(["-movflags", "+faststart"])
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    let file = ok.then(|| std::fs::read(&path).ok()).flatten();
    let _ = std::fs::remove_file(&path);
    if file.is_none() {
        eprintln!("ffmpeg could not make the movie, skipped");
    }
    file.map(Arc::new)
}

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static SRC: ElementDesc = desc("src", &[], &SRC_PADS);
static REC: ElementDesc = desc("rec", &SINK_PADS, &[]);

/// Pushes the file, or serves the demuxer's asks and counts them, saying it reads them in order
/// or not.
struct Src {
    data: Arc<Vec<u8>>,
    at: usize,
    asks: Option<(Arc<AtomicU32>, bool)>,
}

impl Element for Src {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some((_, in_order)) = &self.asks {
            ctx.offer_ranges(SrcPad(0));
            ctx.serve_in_order(SrcPad(0), *in_order);
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let Some((asks, _)) = &self.asks else {
            if self.at >= self.data.len() {
                return Ok(Flow::Eos);
            }
            let Some(mut buf) = ctx.try_alloc() else {
                return Ok(Flow::Ok);
            };
            let n = buf.memory.capacity().min(self.data.len() - self.at).min(1000);
            buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            buf.memory.set_len(n);
            ctx.out(SrcPad(0)).push(buf);
            self.at += n;
            return Ok(Flow::Ok);
        };
        while let Some(Ask { offset, len, tag, mut buf }) = ctx.next_ask(SrcPad(0)) {
            asks.fetch_add(1, Ordering::Relaxed);
            let from = (offset as usize).min(self.data.len());
            let to = (from + len as usize).min(self.data.len());
            buf.memory.as_mut_full()[..to - from].copy_from_slice(&self.data[from..to]);
            buf.memory.set_len(to - from);
            ctx.serve(SrcPad(0), tag, buf);
        }
        Ok(if ctx.asks_over(SrcPad(0)) { Flow::Eos } else { Flow::Ok })
    }
}

type Out = (Option<u64>, u32, Vec<u8>);

struct Rec(Arc<Mutex<Vec<Out>>>);

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &REC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut out = self.0.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            out.push((buf.pts.nanos(), buf.flags.0, buf.memory.data().to_vec()));
        }
        Ok(Flow::Ok)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Read {
    Pushed,
    AtOnce,
    InOrder,
}

/// What each pad puts out and the ranges asked, none for a pushed file.
fn pads(file: &Arc<Vec<u8>>, read: Read, remux: bool) -> (Vec<Vec<Out>>, u32) {
    let asks = Arc::new(AtomicU32::new(0));
    let mut p = Pipeline::new();
    let pulled = (read != Read::Pushed).then(|| (Arc::clone(&asks), read == Read::InOrder));
    let src = p.add(Src { data: Arc::clone(file), at: 0, asks: pulled });
    let demux = match remux {
        true => p.add(Mp4Demux::passthrough(file.to_vec())),
        false => p.add(Mp4Demux::new(file.to_vec())),
    };
    p.link(src.src(SrcPad(0)), demux.sink(Mp4Demux::IN)).expect("link");
    let added = p.preroll().expect("preroll").srcs;
    let recs: Vec<Arc<Mutex<Vec<Out>>>> = added.iter().map(|_| Arc::default()).collect();
    for (pad, rec) in added.iter().zip(&recs) {
        let sink = p.add(Rec(Arc::clone(rec)));
        p.link(pad.element.src(pad.pad), sink.sink(SinkPad(0))).expect("link");
    }
    p.run().expect("run");
    let out = recs.iter().map(|rec| std::mem::take(&mut *rec.lock().unwrap())).collect();
    (out, asks.load(Ordering::Relaxed))
}

fn assert_same(pushed: &[Vec<Out>], pulled: &[Vec<Out>], what: &str) {
    let counts: Vec<usize> = pushed.iter().map(Vec::len).collect();
    assert!(counts.len() == 2 && counts.iter().all(|n| *n > 700), "{what} {counts:?}");
    for (n, (a, b)) in pushed.iter().zip(pulled).enumerate() {
        assert_eq!(a.len(), b.len(), "{what} pad {n}");
        for (k, (a, b)) in a.iter().zip(b).enumerate() {
            assert_eq!(a, b, "{what} pad {n} buffer {k}");
        }
    }
}

/// The video is reframed to Annex B. Read in order its samples share ranges with the audio's
/// and each is a copy of its bytes there, the ranges growing with what was read. Read at once
/// each has the range it is rewritten in, as many ranges as pictures at least.
#[test]
fn samples_read_in_shared_ranges_or_alone_come_out_as_pushed() {
    let Some(file) = movie() else { return };
    let (pushed, _) = pads(&file, Read::Pushed, false);
    let samples = Mp4Reader::new(&file).expect("resolves").tables().sample_count() as u64;
    let pictures = pushed.iter().map(Vec::len).min().unwrap_or(0) as u64 - 1;

    let (pulled, asks) = pads(&file, Read::InOrder, false);
    assert_same(&pushed, &pulled, "in order");
    assert!(u64::from(asks) * 3 < samples, "{asks} ranges in order, {samples} samples");

    let (pulled, asks) = pads(&file, Read::AtOnce, false);
    assert_same(&pushed, &pulled, "at once");
    assert!(u64::from(asks) >= pictures, "{asks} ranges at once, {pictures} pictures");
}

/// A remux puts its samples out as views of their ranges, long from the first whatever the
/// source, so few buffers hold many samples.
#[test]
fn a_remux_reads_long_ranges_from_the_first() {
    let Some(file) = movie() else { return };
    let (pushed, _) = pads(&file, Read::Pushed, true);
    for read in [Read::InOrder, Read::AtOnce] {
        let (pulled, asks) = pads(&file, read, true);
        assert_same(&pushed, &pulled, "remux");
        assert!(asks < 40, "{asks} ranges for {} bytes", file.len());
    }
}
