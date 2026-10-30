//! `oggmuxn` in a real pipeline.

// A test's own fixtures and staging, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pf_ogg::{
    Mapping, OggMuxN, PageHeader, demux_all, flac_frame_samples, header_flags, opus_packet_samples,
};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];

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

static SRC: ElementDesc = desc("packetsrc", &[], &SRC_PADS);
static COLLECT: ElementDesc = desc("collect", &SINK_PADS, &[]);

struct PacketSrc {
    packets: Vec<Vec<u8>>,
    at: usize,
    stall: Option<(usize, Arc<AtomicBool>)>,
    done: Option<Arc<AtomicBool>>,
    watch: Option<(usize, Arc<AtomicUsize>, Arc<AtomicUsize>, usize)>,
}

impl PacketSrc {
    fn new(packets: Vec<Vec<u8>>) -> Self {
        Self { packets, at: 0, stall: None, done: None, watch: None }
    }
}

impl Element for PacketSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.at >= self.packets.len() {
            if let Some(d) = &self.done {
                d.store(true, Ordering::Release);
            }
            return Ok(Flow::Eos);
        }
        if let Some((i, flag)) = &self.stall
            && self.at == *i
            && !flag.load(Ordering::Acquire)
        {
            ctx.wake_within(Duration::from_millis(1));
            return Ok(Flow::Ok);
        }
        if let Some((i, sink, seen, more_than)) = &self.watch
            && self.at == *i
        {
            let held = sink.load(Ordering::Acquire);
            if held <= *more_than {
                ctx.wake_within(Duration::from_millis(1));
                return Ok(Flow::Ok);
            }
            seen.store(held, Ordering::Release);
        }
        let p = &self.packets[self.at];
        let Some(mut buf) = ctx.try_alloc() else {
            ctx.wake_within(Duration::from_millis(1));
            return Ok(Flow::Ok);
        };
        buf.memory.as_mut_full()[..p.len()].copy_from_slice(p);
        buf.memory.set_len(p.len());
        ctx.out(SrcPad(0)).push(buf);
        self.at += 1;
        Ok(Flow::Ok)
    }
}

struct Collect {
    bytes: Arc<Mutex<Vec<u8>>>,
    count: Arc<AtomicUsize>,
}

impl Element for Collect {
    fn desc(&self) -> &'static ElementDesc {
        &COLLECT
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut b = self.bytes.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            b.extend_from_slice(buf.memory.data());
        }
        self.count.store(b.len(), Ordering::Release);
        Ok(Flow::Ok)
    }
}

fn ffmpeg(name: &str, args: &[&str]) -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!("pf_ogg_muxn_{}_{name}", std::process::id()));
    let ok = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y"])
        .args(args)
        .args(["-f", "ogg"])
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("muxn: ffmpeg could not make {name}, skipped");
    }
    ok.then_some(path)
}

fn fixtures(secs: u32) -> Vec<PathBuf> {
    let a = format!("anoisesrc=d={secs}:a=0.3:r=48000:seed=5");
    let b = format!("sine=f=500:d={secs}:r=44100");
    let mut all = Vec::new();
    let two = [
        "-f", "lavfi", "-i", &a, "-f", "lavfi", "-i", &b, "-map", "0:a", "-map", "1:a", "-c:a",
        "libopus",
    ];
    all.extend(ffmpeg(&format!("two_opus_{secs}.ogg"), &two));
    let mixed = [
        "-f", "lavfi", "-i", &a, "-f", "lavfi", "-i", &b, "-map", "0:a", "-map", "1:a", "-c:a:0",
        "libopus", "-c:a:1", "flac",
    ];
    all.extend(ffmpeg(&format!("opus_flac_{secs}.ogg"), &mixed));
    all
}

struct Logical {
    serial: u32,
    packets: Vec<(Vec<u8>, u64)>,
}

fn logical(data: &[u8]) -> Vec<Logical> {
    let mut out: Vec<Logical> = Vec::new();
    for p in demux_all(data) {
        match out.iter_mut().find(|l| l.serial == p.serial) {
            Some(l) => l.packets.push((p.data, p.granule)),
            None => out.push(Logical { serial: p.serial, packets: vec![(p.data, p.granule)] }),
        }
    }
    out
}

fn mapped_granules(packets: &[(Vec<u8>, u64)]) -> Vec<u64> {
    let mapping = Mapping::identify(&packets[0].0);
    let heads = mapping.header_packets(&packets[0].0) as usize;
    let mut g = 0u64;
    packets
        .iter()
        .enumerate()
        .map(|(i, (p, _))| {
            if i >= heads {
                g += u64::from(match mapping {
                    Mapping::Opus => opus_packet_samples(p).unwrap(),
                    _ => flac_frame_samples(p).unwrap(),
                });
            }
            g
        })
        .collect()
}

fn pages(data: &[u8]) -> Vec<(u32, u8, u64)> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < data.len() {
        let h = PageHeader::parse(&data[at..]).expect("a page");
        out.push((h.serial(), h.header_type(), h.granule_position()));
        at += h.len();
    }
    out
}

fn mux(srcs: Vec<PacketSrc>, pool: Option<u32>) -> (Vec<u8>, Arc<AtomicUsize>) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    mux_into(srcs, pool, Arc::clone(&bytes), Arc::clone(&count));
    let out = bytes.lock().unwrap().clone();
    (out, count)
}

fn mux_into(
    srcs: Vec<PacketSrc>,
    pool: Option<u32>,
    bytes: Arc<Mutex<Vec<u8>>>,
    count: Arc<AtomicUsize>,
) {
    let mut p = Pipeline::new();
    let m = p.add(OggMuxN::new());
    let sink = p.add(Collect { bytes, count });
    for (i, s) in srcs.into_iter().enumerate() {
        let e = p.add(s);
        if let Some(slots) = pool.filter(|_| i == 0) {
            p.set_element_pool(e, 4096, slots);
        }
        p.link(e.src(SrcPad(0)), m.sink(OggMuxN::INS[i])).expect("src ! oggmuxn");
    }
    p.link(m.src(OggMuxN::OUT), sink.sink(SinkPad(0))).expect("oggmuxn ! collect");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(p.run().map(|_| ()).map_err(|e| format!("{e:?}")));
    });
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(r) => r.expect("the mux runs"),
        Err(_) => panic!("oggmuxn did not finish in a minute"),
    }
}

fn ffprobe_ok(path: &Path) -> Option<Vec<String>> {
    let out = Command::new("ffprobe")
        .env_remove("LD_LIBRARY_PATH")
        .args([
            "-v",
            "error",
            "-count_packets",
            "-show_entries",
            "stream=codec_name,nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && err.trim().is_empty(), "ffprobe: {err}");
    Some(String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
}

#[test]
fn two_streams_round_trip_grouped_with_their_granules() {
    for path in fixtures(3) {
        let what = path.display().to_string();
        let data = std::fs::read(&path).unwrap();
        let want = logical(&data);
        assert_eq!(want.len(), 2, "{what}");
        let srcs = want
            .iter()
            .map(|l| PacketSrc::new(l.packets.iter().map(|p| p.0.clone()).collect()))
            .collect();
        let (out, _) = mux(srcs, None);
        let got = logical(&out);
        assert_eq!(got.len(), 2, "{what}: two streams out");
        for (g, w) in got.iter().zip(&want) {
            let (gp, wp): (Vec<&[u8]>, Vec<&[u8]>) = (
                g.packets.iter().map(|p| &p.0[..]).collect(),
                w.packets.iter().map(|p| &p.0[..]).collect(),
            );
            assert_eq!(gp, wp, "{what}: the packets of serial {:08x}", w.serial);
            let mapped = mapped_granules(&w.packets);
            let last = w.packets.len() - 1;
            for (i, ((_, gran), m)) in w.packets.iter().zip(&mapped).enumerate() {
                let ends_page = i < last && w.packets[i + 1].1 != *gran;
                if ends_page && *gran != u64::MAX {
                    assert_eq!(*gran, *m, "{what}: ffmpeg's granule at packet {i}");
                }
            }
            for (i, (_, gran)) in g.packets.iter().enumerate() {
                let end =
                    (i..g.packets.len()).take_while(|j| g.packets[*j].1 == *gran).last().unwrap();
                assert_eq!(*gran, mapped[end], "{what}: our granule at packet {i}");
            }
        }
        let pg = pages(&out);
        let bos: Vec<usize> = (0..pg.len()).filter(|i| pg[*i].1 & header_flags::BOS != 0).collect();
        assert_eq!(bos, vec![0, 1], "{what}: both bos pages first");
        let serials = [pg[0].0, pg[1].0];
        assert_ne!(serials[0], serials[1]);
        let eos = pg.iter().filter(|p| p.1 & header_flags::EOS != 0).count();
        assert_eq!(eos, 2, "{what}: an eos page per stream");
        for s in serials {
            assert_eq!(
                pg.iter().rev().find(|p| p.0 == s).unwrap().1 & header_flags::EOS,
                header_flags::EOS,
                "{what}"
            );
        }
        let first_data = pg.iter().position(|p| p.2 != 0).unwrap();
        let heads = want
            .iter()
            .map(|w| Mapping::identify(&w.packets[0].0).header_packets(&w.packets[0].0) as usize);
        let head_pages: usize = pg[..first_data].len();
        assert!(head_pages >= heads.count() + 2, "{what}: header pages ahead of data");
        let out_path =
            std::env::temp_dir().join(format!("pf_ogg_muxn_out_{}.ogg", std::process::id()));
        std::fs::write(&out_path, &out).unwrap();
        if let Some(lines) = ffprobe_ok(&out_path) {
            assert_eq!(lines.len(), 2, "{what}: ffprobe sees both streams, {lines:?}");
            for (line, w) in lines.iter().zip(&want) {
                let n = w.packets.len()
                    - Mapping::identify(&w.packets[0].0).header_packets(&w.packets[0].0) as usize;
                assert!(line.ends_with(&format!(",{n}")), "{what}: {line} for {n} packets");
            }
        }
        let _ = std::fs::remove_file(&out_path);
    }
}

#[test]
fn a_stalled_stream_holds_the_other_back_by_a_bound() {
    for path in fixtures(12) {
        let what = path.display().to_string();
        let data = std::fs::read(&path).unwrap();
        let want = logical(&data);
        let packets =
            |i: usize| -> Vec<Vec<u8>> { want[i].packets.iter().map(|p| p.0.clone()).collect() };
        let done = Arc::new(AtomicBool::new(false));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(AtomicUsize::new(0));
        let third = want.iter().flat_map(|l| &l.packets).map(|p| p.0.len()).sum::<usize>() / 3;
        let mut a = PacketSrc::new(packets(0));
        a.done = Some(Arc::clone(&done));
        a.watch = Some((want[0].packets.len() - 10, Arc::clone(&count), Arc::clone(&seen), third));
        let mut b = PacketSrc::new(packets(1));
        let heads =
            Mapping::identify(&want[1].packets[0].0).header_packets(&want[1].packets[0].0) as usize;
        b.stall = Some((heads + 3, Arc::clone(&done)));
        mux_into(vec![a, b], Some(56), Arc::clone(&bytes), Arc::clone(&count));
        let out = bytes.lock().unwrap().clone();
        let early = seen.load(Ordering::Acquire);
        assert!(
            early > third,
            "{what}: {early} of {} bytes out before the first stream ended",
            out.len()
        );
        let got = logical(&out);
        assert_eq!(got.len(), 2, "{what}");
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(
                g.packets.len(),
                w.packets.len(),
                "{what}: every packet of {:08x}",
                w.serial
            );
        }
    }
}

#[test]
fn oggmuxn_is_built_by_name() {
    let mut r = pf_registry::Registry::new();
    pf_ogg::register(&mut r);
    let desc = r.get("oggmuxn").expect("registered");
    assert_eq!(desc.sinks.iter().map(|p| p.name).collect::<Vec<_>>()[..2], ["sink_0", "sink_1"]);
    assert_eq!(desc.sinks.len(), pf_ogg::mux_multi::MAX_STREAMS);
    let made = (desc.make_default.expect("name-constructible"))();
    assert_eq!(made.desc().name, "oggmuxn");
}
