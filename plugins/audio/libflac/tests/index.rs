//! The seek index `flacdec` publishes. Frames are cut at their CRCs, apart from the decoder.

// A test's own fixtures and threads, not an element.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod support;

use pf_libflac::{FlacDec, FlacEncoder, SampleFormat};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;
use profluens_core::pipeline::Index;
use profluens_core::time::Timestamp;
use profluens_elements::io::FileSrc;

const RATE: u32 = 44_100;
const BLOCK: usize = 4_608;

static BYTES: [OfferDesc; 2] = [OfferDesc::any("audio/raw"), OfferDesc::any("bytes")];
static IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static SINK: ElementDesc = ElementDesc {
    name: "countsink",
    sinks: &IN,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct Count(Arc<AtomicUsize>, Duration);

impl Element for Count {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {
            self.0.fetch_add(1, Ordering::Release);
            std::thread::sleep(self.1);
        }
        Ok(Flow::Ok)
    }
}

fn stream(frames: usize, every: usize) -> (Vec<u8>, Vec<(u64, u64)>) {
    let (mut enc, mut head) = FlacEncoder::new(RATE, 2, SampleFormat::S16).unwrap();
    let mut body = Vec::new();
    for f in 0..frames {
        let pcm: Vec<u8> = (0..BLOCK)
            .flat_map(|i| {
                let t = (f * BLOCK + i) as f64 / f64::from(RATE);
                let v = ((t * 440.0 * std::f64::consts::TAU).sin() * 12_000.0) as i16;
                [v.to_le_bytes(), (v / 3).to_le_bytes()]
            })
            .flatten()
            .collect();
        enc.encode_interleaved(&pcm, &mut body).unwrap();
    }
    let info = enc.finish(&mut body);
    let o = pf_libflac::streaminfo_offset();
    head[o..o + info.len()].copy_from_slice(&info);
    let (_, cuts) = support::split_frames(&[head.as_slice(), body.as_slice()].concat());
    assert_eq!(cuts.len(), frames);
    let at: Vec<(u64, u64)> = cuts
        .iter()
        .enumerate()
        .map(|(f, r)| ((f * BLOCK) as u64, (r.start - head.len()) as u64))
        .collect();
    if every > 0 {
        head[4] &= 0x7F;
        let mut table = Vec::new();
        for &(sample, byte) in at.iter().step_by(every) {
            table.extend_from_slice(&sample.to_be_bytes());
            table.extend_from_slice(&byte.to_be_bytes());
            table.extend_from_slice(&(BLOCK as u16).to_be_bytes());
        }
        // A placeholder point, which is no entry (RFC 9639 section 8.5).
        table.extend_from_slice(&u64::MAX.to_be_bytes());
        table.extend_from_slice(&[0; 10]);
        head.push(0x80 | 3);
        head.extend_from_slice(&(table.len() as u32).to_be_bytes()[1..]);
        head.extend_from_slice(&table);
    }
    let start = head.len() as u64;
    let mut file = head;
    file.extend_from_slice(&body);
    (file, at.into_iter().map(|(s, b)| (s, start + b)).collect())
}

fn ns(sample: u64) -> u64 {
    sample * 1_000_000_000 / u64::from(RATE)
}

fn write(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pf-libflac-index-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn published(
    name: &str,
    bytes: &[u8],
    dec: FlacDec,
    pace: Duration,
    seeks: &[(u64, Timestamp)],
) -> Option<Arc<Index>> {
    let path = write(name, bytes);
    let count = Arc::new(AtomicUsize::new(0));
    let mut p = pf_reactor::pipeline();
    p.set_pool(16 * 1024, 16);
    let src = p.add(FileSrc::new(path.to_str().unwrap()));
    let dec = p.add(dec);
    let sink = p.add(Count(Arc::clone(&count), pace));
    p.link(src.src(FileSrc::OUT), dec.sink(FlacDec::IN)).unwrap();
    p.link(dec.src(FlacDec::OUT), sink.sink(SinkPad(0))).unwrap();
    p.preroll().unwrap();
    let (control, tap) = (p.control(), p.tap_handle());
    let run = std::thread::spawn(move || p.run());
    for &(byte, at) in seeks {
        let before = count.load(Ordering::Acquire);
        let mut waited = 0;
        while count.load(Ordering::Acquire) < before + 8 && waited < 4_000 {
            std::thread::sleep(Duration::from_millis(1));
            waited += 1;
        }
        let _ = control.seek_bytes(at, &[(profluens_core::command::ALL, byte, at)]);
    }
    run.join().unwrap().unwrap();
    tap.index(dec)
}

#[test]
fn the_seektable_is_the_index_file_absolute_and_complete() {
    let (file, frames) = stream(60, 10);
    let len = file.len() as u64;
    let index =
        published("table.flac", &file, FlacDec::new().indexing(Some(len)), Duration::ZERO, &[])
            .expect("published");
    assert!(index.complete);
    assert_eq!(index.duration, Timestamp(ns(60 * BLOCK as u64)));
    assert_eq!(index.file_len, Some(len));
    let want: Vec<(u64, u64)> = frames.iter().step_by(10).map(|&(s, b)| (ns(s), b)).collect();
    let got: Vec<(u64, u64)> = index.entries.iter().map(|e| (e.time_ns, e.byte)).collect();
    assert_eq!(got, want);
}

#[test]
fn without_a_table_the_index_grows_from_the_frames() {
    let (file, frames) = stream(120, 0);
    let len = file.len() as u64;
    let index =
        published("grown.flac", &file, FlacDec::new().indexing(Some(len)), Duration::ZERO, &[])
            .expect("published");
    assert!(!index.complete, "what was read");
    assert_eq!(index.duration, Timestamp(ns(120 * BLOCK as u64)));
    let want: Vec<(u64, u64)> = frames.iter().step_by(10).map(|&(s, b)| (ns(s), b)).collect();
    let got: Vec<(u64, u64)> = index.entries.iter().map(|e| (e.time_ns, e.byte)).collect();
    assert_eq!(got, want);
}

#[test]
fn behind_a_seek_the_frames_are_placed_from_its_byte() {
    let (file, frames) = stream(200, 0);
    let len = file.len() as u64;
    let seeks = [
        (frames[150].1 + 37, Timestamp(ns(frames[150].0))),
        (frames[20].1, Timestamp(ns(frames[20].0))),
    ];
    let dec = FlacDec::new().indexing(Some(len));
    let index =
        published("sought.flac", &file, dec, Duration::from_millis(1), &seeks).expect("published");
    let starts: std::collections::HashMap<u64, u64> = frames.iter().map(|&(s, b)| (b, s)).collect();
    assert!(index.entries.len() > 5, "{index:?}");
    for e in &index.entries {
        let sample = starts.get(&e.byte).unwrap_or_else(|| panic!("{e:?} is no frame start"));
        assert_eq!(e.time_ns, ns(*sample), "{e:?}");
    }
    assert!(index.entries.windows(2).all(|w| w[1].time_ns - w[0].time_ns >= 1_000_000_000));
}

#[test]
fn a_decoder_not_told_it_reads_a_file_publishes_nothing() {
    let (file, _) = stream(10, 5);
    assert_eq!(published("none.flac", &file, FlacDec::new(), Duration::ZERO, &[]), None);
}

#[test]
fn an_id3v2_tag_ahead_of_the_marker_moves_the_index_by_its_length() {
    for (every, name) in [(10, "id3-table.flac"), (0, "id3-grown.flac")] {
        let (bare, frames) = stream(40, every);
        let tag = support::id3_tag(40_000, true);
        let file = [&tag[..], &bare[..]].concat();
        let len = file.len() as u64;
        let index = published(name, &file, FlacDec::new().indexing(Some(len)), Duration::ZERO, &[])
            .expect("published");
        assert_eq!(index.duration, Timestamp(ns(40 * BLOCK as u64)), "{name}");
        let want: Vec<(u64, u64)> =
            frames.iter().step_by(10).map(|&(s, b)| (ns(s), b + tag.len() as u64)).collect();
        let got: Vec<(u64, u64)> = index.entries.iter().map(|e| (e.time_ns, e.byte)).collect();
        assert_eq!(got, want, "{name}");
    }
}
