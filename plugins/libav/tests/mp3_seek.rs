//! Seeks into a bare MP3 read from byte 0, against ffmpeg's decode of the whole file.

// Test code.
#![allow(clippy::disallowed_methods)]

mod support;

use pf_harness::Harness;
use pf_libav::LibavAudioDec;
use pf_mp3::index::{Clock, HeadScan, Resync};
use profluens_core::pipeline::{Index, IndexEntry};
use profluens_core::time::Timestamp;

const SLOT: usize = 64 * 1024;
const RATE: u64 = 44_100;
const STRIDE: usize = 4;

fn stream(name: &str, args: &[&str]) -> Option<Vec<u8>> {
    let mut all = vec![
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=44100:duration=8",
        "-f",
        "lavfi",
        "-i",
        "anoisesrc=d=8:a=0.05:r=44100",
        "-filter_complex",
        "[0][1]amerge=inputs=2",
        "-ac",
        "2",
        "-metadata",
        "title=a tag ahead of the frames",
    ];
    all.extend_from_slice(args);
    all.extend_from_slice(&["-f", "mp3"]);
    support::make(name, &all)
}

fn oracle(name: &str) -> Option<Vec<u8>> {
    support::oracle(name, &format!("{name}.s16"), &["-f", "s16le", "-c:a", "pcm_s16le"])
}

fn head(file: &[u8]) -> (Index, Clock) {
    let mut scan = HeadScan::new(Some(file.len() as u64));
    let index = scan.feed(file).or_else(|| scan.end()).expect("an index");
    let clock = Clock::new(scan.layout().expect("a layout"), &index);
    (index, clock)
}

fn feed(h: &mut Harness, bytes: &[u8], pts: Timestamp, eos: bool) -> (Vec<u8>, Option<Timestamp>) {
    let (mut pcm, mut first) = (Vec::new(), None);
    let mut take = |out: Vec<profluens_core::buffer::Buffer>| {
        for b in out {
            first.get_or_insert(b.pts);
            pcm.extend_from_slice(b.memory.data());
        }
    };
    for (i, c) in bytes.chunks(4096).enumerate() {
        let mut buf = h.alloc(c);
        buf.pts = if i == 0 { pts } else { Timestamp::NONE };
        h.push(buf).expect("push");
        loop {
            let out = h.drain_outputs();
            if out.is_empty() {
                break;
            }
            take(out);
            h.crank().expect("crank");
        }
    }
    if eos {
        take(h.eos().expect("eos"));
    }
    (pcm, first)
}

fn worst(got: &[u8], want: &[u8]) -> i32 {
    let s = |b: &[u8]| {
        b.as_chunks::<2>().0.iter().map(|c| i32::from(i16::from_le_bytes(*c))).collect::<Vec<_>>()
    };
    s(got).iter().zip(s(want)).map(|(a, b)| (a - b).abs()).max().unwrap_or(0)
}

fn sample_at(ns: u64) -> usize {
    (u128::from(ns) * u128::from(RATE)).div_ceil(1_000_000_000) as usize
}

fn seeks_land_on_the_sample(
    name: &str,
    file: &[u8],
    index: &Index,
    want: &[u8],
    targets: &[f64],
    h: &mut Harness,
) {
    for &t in targets {
        let ns = (t * 1e9) as u64;
        let e: &IndexEntry =
            index.entries.iter().rev().find(|e| e.time_ns <= ns).expect("an entry");
        h.seek(e.restart, Timestamp(ns)).expect("seek");
        let _ = h.drain_outputs();
        let (got, first) = feed(
            h,
            &file[e.restart as usize..(e.restart as usize + 200_000).min(file.len())],
            Timestamp(e.restart_ns),
            false,
        );
        let at = sample_at(ns);
        assert_eq!(
            first,
            Some(profluens_audio::time_at(at as u64, RATE as u32)),
            "{name}: a seek to {t} s"
        );
        let n = (RATE as usize / 2 * STRIDE).min(got.len()).min(want.len() - at * STRIDE);
        assert!(n > 10_000, "{name}: {n} bytes behind {t} s");
        let off = worst(&got[..n], &want[at * STRIDE..at * STRIDE + n]);
        assert!(off <= 1, "{name}: behind {t} s a sample {off} steps off the oracle");
    }
}

#[test]
fn a_constant_bitrate_mp3_seeks_to_the_sample() {
    for (name, args) in [
        ("cbr_info.mp3", &["-c:a", "libmp3lame", "-b:a", "128k"][..]),
        ("cbr_bare.mp3", &["-c:a", "libmp3lame", "-b:a", "320k", "-write_xing", "0"][..]),
    ] {
        let Some(file) = stream(name, args) else {
            return;
        };
        let Some(want) = oracle(name) else { return };
        let (index, clock) = head(&file);
        assert!(clock.layout().cbr.is_some(), "{name}: one bitrate");
        assert!(index.entries.len() > 6, "{name}: an entry a second");
        let mut h =
            Harness::with_slot_size(LibavAudioDec::mp3().indexing(Some(file.len() as u64)), SLOT);
        let (all, first) = feed(&mut h, &file, Timestamp::ZERO, true);
        assert_eq!(first, Some(Timestamp::ZERO), "{name}");
        let n = all.len().min(want.len());
        assert!(
            want.len().abs_diff(all.len()) <= 1152 * STRIDE,
            "{name}: {} and {} bytes",
            all.len(),
            want.len()
        );
        assert!(worst(&all[..n], &want[..n]) <= 1, "{name}: from the start");
        seeks_land_on_the_sample(name, &file, &index, &want, &[1.0, 6.5, 2.345, 0.2, 7.0], &mut h);
    }
}

#[test]
fn a_vbr_mp3_seeks_back_into_what_was_read_to_the_sample() {
    let name = "vbr.mp3";
    let Some(file) = stream(name, &["-c:a", "libmp3lame", "-q:a", "2"]) else {
        return;
    };
    let Some(want) = oracle(name) else { return };
    let (index, mut clock) = head(&file);
    assert!(clock.layout().cbr.is_none(), "many bitrates");
    assert!(index.complete && index.entries.len() == 100, "the table");
    let mut base = 0u64;
    loop {
        match clock.resync(&file[base as usize..], base) {
            Resync::Drop(n) if n > 0 => base += n as u64,
            Resync::Frame { skip, .. } => {
                base += skip as u64;
                break;
            }
            other => panic!("no first frame, {other:?}"),
        }
    }
    let _ = clock.walk(&file[base as usize..], base);
    let grown = clock.index().expect("restarts grown");
    let mut h =
        Harness::with_slot_size(LibavAudioDec::mp3().indexing(Some(file.len() as u64)), SLOT);
    let (all, _) = feed(&mut h, &file, Timestamp::ZERO, false);
    let n = all.len().min(want.len());
    assert!(worst(&all[..n], &want[..n]) <= 1, "from the start");
    assert!(grown.entries.len() > 6, "an entry a second over 8 s");
    seeks_land_on_the_sample(name, &file, &grown, &want, &[5.5, 1.25, 3.0], &mut h);

    let mut h =
        Harness::with_slot_size(LibavAudioDec::mp3().indexing(Some(file.len() as u64)), SLOT);
    let _ = feed(&mut h, &file[..20_000], Timestamp::ZERO, false);
    let span = (u128::from(index.duration.0) * u128::from(RATE) / 1_000_000_000) as usize;
    for e in [&index.entries[30], &index.entries[70]] {
        h.seek(e.restart, Timestamp(e.time_ns)).expect("seek");
        let _ = h.drain_outputs();
        let end = (e.restart as usize + 100_000).min(file.len());
        let (got, first) =
            feed(&mut h, &file[e.restart as usize..end], Timestamp(e.restart_ns), false);
        let first = sample_at(first.expect("audio").0);
        // A fifth of a second in, past the frames the reservoir left short.
        let probe = &got[RATE as usize / 5 * STRIDE..][..256 * STRIDE];
        let found = (0..want.len() / STRIDE - 256)
            .find(|&i| worst(probe, &want[i * STRIDE..(i + 256) * STRIDE]) <= 1)
            .expect("the audio is the oracle's")
            - RATE as usize / 5;
        let off = found as i64 - first as i64;
        eprintln!("vbr: a seek to the table's {} ms is {off} samples off", e.time_ns / 1_000_000);
        assert!(off.unsigned_abs() as usize <= span / 256 + 2 * 1152, "{off} samples off");
    }
}
