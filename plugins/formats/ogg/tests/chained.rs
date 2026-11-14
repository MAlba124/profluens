//! A chained Ogg file (RFC 3533 section 4) read by `oggdemux` from its head, the links going on
//! in the tracks of the first.

// A test's own files and clock, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pf_ogg::{OggDemux, OggWriter};
use pf_sim::driver::reactor::{Faults, World};
use pf_sim::time::MS;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::command::Control;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::{Index, Pipeline};
use profluens_core::sched::{Outcome, Stepped};
use profluens_core::time::Timestamp;
use profluens_elements::io::FileSrc;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static REC: ElementDesc = ElementDesc {
    name: "rec",
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ev {
    Buf { pts: Option<u64>, head: bool, bytes: Vec<u8> },
    Flush,
}

struct Rec {
    evs: Arc<Mutex<Vec<Ev>>>,
    seen: Arc<AtomicUsize>,
}

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &REC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut evs = self.evs.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            evs.push(Ev::Buf {
                pts: buf.pts.nanos(),
                head: buf.flags.contains(BufferFlags::HEADER),
                bytes: buf.memory.data().to_vec(),
            });
            self.seen.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.evs.lock().unwrap().push(Ev::Flush);
        }
        Ok(())
    }
}

const PRE_SKIP: u64 = 312;
/// 20 ms of 48 kHz samples.
const SAMPLES: u64 = 960;

fn opus_head(channels: u8) -> Vec<u8> {
    let mut h = b"OpusHead\x01".to_vec();
    h.push(channels);
    h.extend_from_slice(&(PRE_SKIP as u16).to_le_bytes());
    h.extend_from_slice(&[0x80, 0xBB, 0, 0, 0, 0, 0]);
    h
}

/// A media packet of a link, a 20 ms CELT TOC, the link and the packet's number after it.
fn packet(link: usize, i: u64, size: usize) -> Vec<u8> {
    let mut p = vec![link as u8; size];
    p[0] = 31 << 3;
    p[1..9].copy_from_slice(&i.to_le_bytes());
    p
}

/// One Opus link of `packets` packets, ten a page.
fn link(out: &mut Vec<u8>, n: usize, serial: u32, channels: u8, packets: u64) {
    let mut w = OggWriter::new(serial);
    w.write_packet(out, &opus_head(channels), 0).unwrap();
    w.flush(out);
    w.write_packet(out, b"OpusTags\0\0\0\0\0\0\0\0", 0).unwrap();
    w.flush(out);
    for i in 0..packets {
        w.write_packet(out, &packet(n, i, 300), (i + 1) * SAMPLES).unwrap();
        if i % 10 == 9 {
            w.flush(out);
        }
    }
    w.finish(out);
}

/// Links of (channels, packets), each its own serial.
fn chain(links: &[(u8, u64)]) -> Vec<u8> {
    let mut file = Vec::new();
    for (n, (channels, packets)) in links.iter().enumerate() {
        link(&mut file, n, 0x100 + n as u32, *channels, *packets);
    }
    file
}

fn link_ns(packets: u64) -> u64 {
    (packets * SAMPLES - PRE_SKIP) * 1_000_000_000 / 48_000
}

fn sample_ns(samples: u64) -> u64 {
    samples * 1_000_000_000 / 48_000
}

struct Out {
    evs: Vec<Ev>,
    index: Option<Arc<Index>>,
}

type Seek = Option<(usize, Timestamp, u64)>;

/// Runs to the end, seeking once `seek.0` packets are out and the index is published.
fn step(
    mut run: Stepped,
    clock: &MockClock,
    mut seek: Seek,
    ctl: Arc<Control>,
    ready: impl Fn() -> bool,
) {
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..4_000_000u32 {
        if seek.is_some_and(|(after, ..)| ready() && after == 0) {
            let (_, at, byte) = seek.take().unwrap();
            let _ = ctl.seek_bytes(at, &[(profluens_core::command::ALL, byte, at)]);
        }
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        if ready.is_empty() {
            match run.next_deadline() {
                Some(at) if at > clock.now() => clock.advance(Timestamp(at.0 - clock.now().0)),
                Some(_) => {}
                None => break,
            }
            continue;
        }
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let g = ready[(rng % ready.len() as u64) as usize];
        let _ = run.pass(g).expect("pass");
    }
    let stalled = (0..run.groups()).any(|g| !matches!(run.outcome(g), Some(Outcome::Exit)));
    assert!(!stalled, "the run stalled");
    run.finish().expect("finish");
}

/// The file pulled from its head, `seek` = (after so many packets, to, byte guess).
fn demux(data: &[u8], seek: Seek) -> Out {
    demux_in(data, seek, 4 << 20)
}

/// [`demux`] with a pool of `bytes` for the demuxer.
fn demux_in(data: &[u8], seek: Seek, bytes: u64) -> Out {
    let name = "/chained.opus";
    let clock = MockClock::new();
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
    let mut world = World::new(Faults { latency: (0, 3 * MS), split: true, ..Faults::NONE });
    world.file(name, data.to_vec());
    p.set_reactor_factory(World::factory(Arc::new(Mutex::new(world)), Arc::clone(&shared), 1));
    let src = p.add(FileSrc::new(name));
    let len = data.len() as u64;
    let head = data[..data.len().min(64 << 10)].to_vec();
    let demux = p.add(OggDemux::with_len(Some(len)).with_head(head));
    p.set_element_pool(src, 64 << 10, 8);
    p.set_element_pool_bytes(demux, 16 << 10, 16, bytes);
    p.link(src.src(FileSrc::OUT), demux.sink(OggDemux::IN)).expect("filesrc ! oggdemux");
    let pads = p.preroll().expect("preroll").srcs;
    assert_eq!(pads.len(), 1, "the first link's one stream");
    let evs = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(AtomicUsize::new(0));
    let rec = p.add(Rec { evs: Arc::clone(&evs), seen: Arc::clone(&seen) });
    p.link(pads[0].element.src(pads[0].pad), rec.sink(SinkPad(0))).expect("oggdemux ! rec");
    let taps = p.tap_handle();
    let ctl = p.control();
    let run = p.step(Arc::clone(&shared)).expect("step");
    let after = seek.map_or(0, |s| s.0);
    let ready = || seen.load(Ordering::Relaxed) >= after && taps.index(demux).is_some();
    step(run, &clock, seek.map(|(_, at, byte)| (0, at, byte)), ctl, ready);
    let evs = evs.lock().unwrap().clone();
    Out { evs, index: taps.index(demux) }
}

/// The link and packet number of a media packet.
fn which(bytes: &[u8]) -> (usize, u64) {
    (bytes[9] as usize, u64::from_le_bytes(bytes[1..9].try_into().unwrap()))
}

fn media(evs: &[Ev]) -> Vec<(usize, u64, Option<u64>)> {
    evs.iter()
        .filter_map(|e| match e {
            // the nil packet of an eos page
            Ev::Buf { head: false, bytes, .. } if bytes.is_empty() => None,
            Ev::Buf { pts, head: false, bytes } => {
                let (link, i) = which(bytes);
                Some((link, i, *pts))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn every_link_plays_on_in_the_track_stamped_on_from_the_last() {
    let links = [(2u8, 400u64), (2, 30), (2, 250)];
    let data = chain(&links);
    let out = demux(&data, None);
    let heads = out.evs.iter().filter(|e| matches!(e, Ev::Buf { head: true, .. })).count();
    assert_eq!(heads, 2 * links.len(), "each link's two headers");
    let got = media(&out.evs);
    let mut want = Vec::new();
    let mut start = 0;
    for (n, (_, packets)) in links.iter().enumerate() {
        for i in 0..*packets {
            want.push((n, i, Some(start + sample_ns(i * SAMPLES))));
        }
        start += link_ns(*packets);
    }
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(&want) {
        assert_eq!(g, w);
    }
    let index = out.index.expect("published");
    assert_eq!(index.duration, Timestamp(start), "the duration of every link");
}

#[test]
fn a_link_of_other_channels_is_dropped_and_a_later_one_like_the_first_goes_on() {
    let links = [(2u8, 100u64), (1, 50), (2, 80)];
    let data = chain(&links);
    let out = demux(&data, None);
    let got = media(&out.evs);
    assert!(got.iter().all(|(link, ..)| *link != 1), "the mono link is not put out");
    assert_eq!(got.len(), 180);
    // the third goes on where the scan puts it, the second's time skipped
    let third = got.iter().find(|(link, ..)| *link == 2).unwrap();
    assert_eq!(third.2, Some(link_ns(100) + link_ns(50)));
    let index = out.index.expect("published");
    assert_eq!(index.duration, Timestamp(link_ns(100) + link_ns(50) + link_ns(80)));
}

/// The seeks wait on the index, the scan done, the reader by then well into the long second link.
#[test]
fn a_seek_into_another_link_reads_it_from_its_headers_and_one_in_its_own_bisects() {
    let links = [(2u8, 1000u64), (2, 10_000), (2, 300)];
    let data = chain(&links);
    let third = link_ns(1000) + link_ns(10_000);
    let out = demux(&data, Some((0, Timestamp(third + 2_000_000_000), data.len() as u64 / 2)));
    let from = out.evs.iter().rposition(|e| *e == Ev::Flush).expect("sought") + 1;
    let before = media(&out.evs[..from]);
    assert_eq!(before.last().map(|b| b.0), Some(1), "sought in the second link");
    let behind = &out.evs[from..];
    let heads = behind.iter().take_while(|e| matches!(e, Ev::Buf { head: true, .. })).count();
    assert_eq!(heads, 2, "the third link's headers first");
    let got = media(behind);
    assert_eq!(got[0], (2, 0, Some(third)), "the third link from its start");
    assert_eq!(got.len(), 300);
    // within the link it is in, a bisection lands by its page
    let back = link_ns(1000) + 150_000_000_000;
    let mid = demux(&data, Some((0, Timestamp(back), data.len() as u64 / 2)));
    let from = mid.evs.iter().rposition(|e| *e == Ev::Flush).expect("sought") + 1;
    let before = media(&mid.evs[..from]);
    assert_eq!(before.last().map(|b| b.0), Some(1), "sought in the second link");
    let behind = &mid.evs[from..];
    assert!(matches!(behind.first(), Some(Ev::Buf { head: false, .. })), "no headers first");
    let got = media(behind);
    let (link, i, pts) = got[0];
    assert_eq!(link, 1);
    assert_eq!(pts, Some(link_ns(1000) + sample_ns(i * SAMPLES)));
    let landed = sample_ns(i * SAMPLES);
    assert!(landed <= 150_000_000_000 && landed > 149_000_000_000, "landed at packet {i}");
}

/// A pool too small for the scan leaves the duration unknown, a seek once a later link plays
/// reads the file again from its first.
#[test]
fn links_not_known_play_on_and_a_seek_reads_from_the_first() {
    let data = chain(&[(2, 100), (2, 5000)]);
    let out =
        demux_in(&data, Some((300, Timestamp(50_000_000_000), data.len() as u64 / 2)), 96 << 10);
    assert_eq!(out.index.expect("published").duration, Timestamp::NONE);
    let from = out.evs.iter().rposition(|e| *e == Ev::Flush).expect("sought") + 1;
    assert_eq!(media(&out.evs[..from]).last().map(|b| b.0), Some(1), "sought in the second");
    let behind = &out.evs[from..];
    let heads = behind.iter().take_while(|e| matches!(e, Ev::Buf { head: true, .. })).count();
    assert_eq!(heads, 2);
    let got = media(behind);
    assert_eq!(got[0], (0, 0, Some(0)));
    assert_eq!(got.len(), 5100);
    assert_eq!(got[100], (1, 0, Some(link_ns(100))));
}
