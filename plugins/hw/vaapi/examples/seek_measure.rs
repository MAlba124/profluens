//! What a seek to a time costs a video decoder. Not a test, prints one row a command.
//! Usage: seek_measure h264|h265 sw|va STREAM PTS [--skips]

// Measurement code.
#![allow(clippy::disallowed_methods)]

use std::time::Instant;

use pf_harness::Harness;
use pf_libav::LibavH264Dec as H264Dec;
use pf_libav::LibavH265Dec as H265Dec;
use pf_vaapi::{VaapiH264Dec, VaapiH265Dec};
use profluens_core::time::Timestamp;

const SLOT: usize = 4 * 1024 * 1024;
const PLAY: Timestamp = Timestamp::from_secs(4);
const TARGET: Timestamp = Timestamp::from_secs(9);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
    H264,
    H265,
}

fn opens_picture(codec: Codec, data: &[u8], at: usize) -> bool {
    match codec {
        Codec::H264 => matches!(data[at] & 0x1F, 1 | 5) && data[at + 1] & 0x80 != 0,
        Codec::H265 => (data[at] >> 1) & 0x3F < 32 && data[at + 2] & 0x80 != 0,
    }
}

fn units(codec: Codec, data: &[u8]) -> Vec<&[u8]> {
    let (mut cuts, mut open, mut head) = (vec![0], false, 0);
    for i in 0..data.len().saturating_sub(6) {
        if data[i..i + 3] != [0, 0, 1] {
            continue;
        }
        if opens_picture(codec, data, i + 3) {
            if open {
                cuts.push(head);
            }
            (open, head) = (true, usize::MAX);
        } else if head == usize::MAX {
            head = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
        }
    }
    let ends = cuts.iter().skip(1).copied().chain([data.len()]);
    cuts.iter().zip(ends).map(|(&a, b)| &data[a..b]).collect()
}

fn non_reference(codec: Codec, unit: &[u8]) -> bool {
    let mut slices = 0;
    for i in 0..unit.len().saturating_sub(4) {
        if unit[i..i + 3] != [0, 0, 1] {
            continue;
        }
        let header = unit[i + 3];
        let leaf = match codec {
            Codec::H264 => match header & 0x1F {
                1 => Some(header >> 5 & 3 == 0),
                6 | 9 | 12 => None,
                _ => Some(false),
            },
            Codec::H265 => match header >> 1 & 0x3F {
                kind @ 0..=15 => Some(kind % 2 == 0 && unit[i + 4] & 7 == 1),
                35 | 38 | 39 | 40 => None,
                _ => Some(false),
            },
        };
        match leaf {
            Some(true) => slices += 1,
            Some(false) => return false,
            None => {}
        }
    }
    slices > 0
}

struct Clip<'a> {
    codec: Codec,
    units: Vec<&'a [u8]>,
    pts: Vec<Timestamp>,
    skips: bool,
}

#[derive(Default)]
struct Cost {
    fed: usize,
    skipped: usize,
    early: usize,
    shown_early: usize,
    shown: usize,
    millis: f64,
}

impl Clip<'_> {
    fn run(&self, h: &mut Harness, from: usize, until: Timestamp, cost: &mut Cost) -> usize {
        for i in from..self.units.len() {
            let mut buf = h.alloc(self.units[i]);
            buf.pts = self.pts[i];
            h.push(buf).expect("push");
            cost.fed += 1;
            if self.pts[i] < until {
                cost.early += 1;
                cost.skipped += usize::from(self.skips && non_reference(self.codec, self.units[i]));
            }
            let out = h.drain_outputs();
            cost.shown += out.len();
            cost.shown_early += out.iter().filter(|b| b.pts < until).count();
            if out.iter().any(|b| b.pts >= until) {
                return i + 1;
            }
        }
        panic!("the stream ended before {until:?}");
    }

    fn measure(
        &self,
        mut h: Harness,
        name: &str,
        point: Timestamp,
        command: fn(&mut Harness) -> bool,
    ) {
        let next = self.run(&mut h, 0, PLAY, &mut Cost::default());
        let mut cost = Cost::default();
        let started = Instant::now();
        let from = if command(&mut h) { 0 } else { next };
        self.run(&mut h, from, point, &mut cost);
        cost.millis = started.elapsed().as_secs_f64() * 1e3;
        let decoded = cost.fed - cost.skipped;
        let decode_only = (cost.early - cost.skipped).saturating_sub(cost.shown_early);
        println!(
            "{name:<14} fed {:>4}  decoded {:>4}  skipped {:>4}  shown {:>4}  shown early {:>4}  \
             decode-only {:>4}  {:>9.1} ms",
            cost.fed, decoded, cost.skipped, cost.shown, cost.shown_early, decode_only, cost.millis
        );
    }
}

fn harness(codec: Codec, hardware: bool) -> Harness {
    match (codec, hardware) {
        (Codec::H264, false) => Harness::with_pool(H264Dec::new(), SLOT, 8),
        (Codec::H265, false) => Harness::with_pool(H265Dec::new(), SLOT, 8),
        (Codec::H264, true) => Harness::with_pool(VaapiH264Dec::new(), SLOT, 8),
        (Codec::H265, true) => Harness::with_pool(VaapiH265Dec::new(), SLOT, 8),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, codec, decoder, stream, pts, rest @ ..] = args.as_slice() else {
        eprintln!("usage: seek_measure h264|h265 sw|va STREAM PTS [--skips]");
        std::process::exit(2);
    };
    let codec = if codec == "h265" { Codec::H265 } else { Codec::H264 };
    let hardware = decoder == "va";
    let data = std::fs::read(stream).expect("read the stream");
    let seconds = std::fs::read_to_string(pts).expect("read the pts list");
    let pts: Vec<Timestamp> = seconds
        .lines()
        .map(|line| {
            Timestamp::from_nanos((line.trim().parse::<f64>().expect("a pts") * 1e9).round() as u64)
        })
        .collect();
    let units = units(codec, &data);
    assert_eq!(units.len(), pts.len(), "one pts a unit");
    let clip = Clip { codec, units, pts, skips: rest.iter().any(|a| a == "--skips") };

    let leaves = clip.units.iter().filter(|u| non_reference(codec, u)).count();
    println!(
        "{} units, {leaves} non-reference, point {TARGET:?}, played to {PLAY:?}",
        clip.units.len()
    );
    let new = || harness(codec, hardware);
    clip.measure(new(), "keyframe seek", Timestamp::ZERO, |h| h.seek(0, Timestamp::ZERO).is_ok());
    clip.measure(new(), "accurate seek", TARGET, |h| h.seek(0, TARGET).is_ok());
    clip.measure(new(), "skip", TARGET, |h| h.skip_to(TARGET).is_err());
}
