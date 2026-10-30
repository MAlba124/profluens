// Test module. Fixtures, recordings and assertions allocate freely.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use profluens_audio::{
    AudioFormat, SampleFormat, WavParse, position_at, time_at, trim_pcm, write_pcm_wav,
};
use profluens_core::batch::Inputs;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_elements::io::FileSrc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Shown { pts: Timestamp, sample: i64, now: Timestamp },
    Before,
    Flush,
}

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static DESC: ElementDesc = ElementDesc {
    name: "indexsink",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct IndexSink {
    format: AudioFormat,
    rows: Arc<Mutex<Vec<Row>>>,
}

impl Element for IndexSink {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        for _ in 0..inputs.len() {
            if ctx.command_pending() {
                break;
            }
            let Some(front) = inputs.front() else { break };
            let cut =
                trim_pcm(front.memory.data(), self.format, front.pts, ctx.presentation_point());
            if cut.pcm.is_empty() && cut.skipped > 0 {
                self.rows.lock().unwrap().push(Row::Before);
            } else if !ctx.due(cut.pts) {
                break;
            } else {
                let s = |at: usize| i64::from(u16::from_le_bytes([cut.pcm[at], cut.pcm[at + 1]]));
                let sample = if self.format.channels == 2 { s(0) | s(2) << 16 } else { s(0) };
                self.rows.lock().unwrap().push(Row::Shown { pts: cut.pts, sample, now: ctx.now() });
            }
            inputs.pop();
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.rows.lock().unwrap().push(Row::Flush);
        }
        Ok(())
    }
}

const SECONDS: u64 = 3;
const WIDE: AudioFormat = AudioFormat::new(48_000, 2, SampleFormat::S16);
const NARROW: AudioFormat = AudioFormat::new(8_000, 1, SampleFormat::S16);
const HEADER: u64 = 44;

fn indexed(name: &str, format: AudioFormat) -> PathBuf {
    let frames = SECONDS * u64::from(format.sample_rate);
    let mut pcm = Vec::new();
    for n in 0..frames {
        pcm.extend_from_slice(&(n as u16).to_le_bytes());
        if format.channels == 2 {
            pcm.extend_from_slice(&((n >> 16) as u16).to_le_bytes());
        }
    }
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("two_sources");
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let path = dir.join(name);
    std::fs::write(&path, write_pcm_wav(&format, &pcm)).expect("write the fixture");
    path
}

struct Branch {
    src: ElementId,
    parse: ElementId,
    format: AudioFormat,
    rows: Arc<Mutex<Vec<Row>>>,
}

impl Branch {
    fn add(p: &mut Pipeline, name: &str, format: AudioFormat) -> Branch {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let src = p.add(FileSrc::new(indexed(name, format)).push_only());
        let parse = p.add(WavParse::new());
        let sink = p.add(IndexSink { format, rows: Arc::clone(&rows) });
        p.set_element_pool(src, format.frame_stride() * format.sample_rate as usize / 100, 16);
        p.link(src.src(FileSrc::OUT), parse.sink(WavParse::IN)).expect("link the parser");
        p.link(parse.src(WavParse::OUT), sink.sink(SinkPad(0))).expect("link the sink");
        Branch { src, parse, format, rows }
    }

    fn targets(&self, at: Timestamp) -> [(ElementId, u64, Timestamp); 2] {
        let frame = position_at(at, self.format.sample_rate);
        let byte = HEADER + frame * self.format.frame_stride() as u64;
        [(self.src, byte, at), (self.parse, byte, at)]
    }

    fn rows(&self) -> Vec<Row> {
        self.rows.lock().unwrap().clone()
    }

    fn first_behind_the_flush(&self) -> Option<(Timestamp, i64, Timestamp, usize)> {
        let rows = self.rows();
        let flush = rows.iter().rposition(|r| *r == Row::Flush)?;
        let before =
            rows[flush..].iter().take_while(|r| !matches!(r, Row::Shown { .. })).count() - 1;
        rows[flush..].iter().find_map(|r| match *r {
            Row::Shown { pts, sample, now } => Some((pts, sample, now, before)),
            _ => None,
        })
    }
}

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn two_sources_seek_to_their_own_bytes_and_present_the_same_stream_time_together() {
    let clock = MockClock::new();
    let mut p = pf_reactor::pipeline();
    p.set_clock(Arc::new(clock.clone()));
    let wide = Branch::add(&mut p, "wide.wav", WIDE);
    let narrow = Branch::add(&mut p, "narrow.wav", NARROW);
    let (seek, stop) = (p.control(), p.control());
    let run = std::thread::spawn(move || p.run());
    let shown = |b: &Branch| b.rows().iter().filter(|r| matches!(r, Row::Shown { .. })).count();
    wait_for("both play", || shown(&wide) >= 1 && shown(&narrow) >= 1);
    clock.advance(Timestamp::from_millis(200));
    wait_for("both play on", || shown(&wide) >= 15 && shown(&narrow) >= 15);

    let target = Timestamp::from_micros(1_234_500);
    let (near, far) = (Timestamp::from_millis(1_200), Timestamp::from_secs(1));
    let targets = [wide.targets(near), narrow.targets(far)].concat();
    assert_ne!(targets[0].1, targets[2].1, "one byte could not serve both");
    seek.seek_bytes(target, &targets).expect("seek");
    wait_for("both present behind the flush", || {
        wide.first_behind_the_flush().is_some() && narrow.first_behind_the_flush().is_some()
    });
    let running = clock.now();
    for (branch, from) in [(&wide, near), (&narrow, far)] {
        let rate = branch.format.sample_rate;
        let (pts, sample, now, before) = branch.first_behind_the_flush().expect("waited for");
        let first = position_at(target, rate);
        assert_eq!(pts, time_at(first, rate), "{rate} Hz: the first frame at or after the target");
        assert_eq!(sample, first as i64, "{rate} Hz: and it is the frame the file holds there");
        let ahead = (target.0 - from.0) / 10_000_000;
        assert!(
            (before as u64).abs_diff(ahead) <= 1,
            "{rate} Hz: {before} buffers dropped of {ahead}"
        );
        assert_eq!(now, target, "{rate} Hz: presented at {now:?}");
    }
    assert!(running >= Timestamp::from_millis(200));

    let (a, b) = (shown(&wide), shown(&narrow));
    clock.advance(Timestamp::from_millis(10));
    wait_for("both present the next buffer", || shown(&wide) > a && shown(&narrow) > b);

    let back = Timestamp::from_millis(500);
    seek.seek_bytes(back, &wide.targets(back)).expect("seek");
    let flushed = |b: &Branch| b.rows().iter().filter(|r| **r == Row::Flush).count() == 2;
    wait_for("the narrow file is read from its start", || {
        [&wide, &narrow].iter().all(|b| flushed(b) && b.first_behind_the_flush().is_some())
    });
    let (pts, sample, _, before) = narrow.first_behind_the_flush().expect("waited for");
    assert_eq!((pts, sample), (back, 4_000), "the narrow file's frame at half a second");
    assert!((49..=51).contains(&before), "it read half a second to get there, {before} buffers");
    let (pts, sample, _, before) = wide.first_behind_the_flush().expect("waited for");
    assert_eq!((pts, sample, before), (back, 24_000, 0), "the wide file went straight to its byte");

    stop.stop();
    run.join().expect("the run thread").expect("the run ends cleanly");
}
