//! `libavvorbisdec` against ffmpeg's decode of an Ogg Vorbis stream made with libvorbis.

// Test code.
#![allow(clippy::disallowed_methods)]

mod support;

use pf_harness::Harness;
use pf_libav::LibavAudioDec;
use profluens_audio::{position_at, time_at};
use profluens_core::bus::BusMessage;
use profluens_core::time::Timestamp;

const SLOT: usize = 64 * 1024;
const RATE: u32 = 44_100;
const STRIDE: usize = 4;

fn fixture() -> Option<(Vec<u8>, Vec<u8>)> {
    let args = [
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=44100:duration=3",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=997:sample_rate=44100:duration=3",
        "-filter_complex",
        "[0][1]amix=inputs=2,volume=4",
        "-ac",
        "2",
        "-c:a",
        "libvorbis",
        "-b:a",
        "128k",
        "-f",
        "ogg",
    ];
    let ogg = support::make("tone.ogg", &args)?;
    let pcm = support::oracle("tone.ogg", "tone.ogg.s16", &["-f", "s16le", "-c:a", "pcm_s16le"])?;
    Some((ogg, pcm))
}

fn packets(ogg: &[u8]) -> Vec<(Vec<u8>, u64)> {
    pf_ogg::demux_all(ogg).into_iter().map(|p| (p.data, p.granule)).collect()
}

fn laced(heads: &[(Vec<u8>, u64)]) -> Vec<u8> {
    let mut out = vec![2u8];
    for (h, _) in &heads[..2] {
        out.extend(std::iter::repeat_n(0xFF, h.len() / 255));
        out.push((h.len() % 255) as u8);
    }
    for (h, _) in &heads[..3] {
        out.extend_from_slice(h);
    }
    out
}

fn warnings(h: &mut Harness) -> usize {
    h.bus_messages().into_iter().filter(|m| matches!(m, BusMessage::Warning { .. })).count()
}

fn feed(h: &mut Harness, input: &[(&[u8], Timestamp)]) -> (Vec<u8>, Vec<Timestamp>) {
    let (mut pcm, mut stamps) = (Vec::new(), Vec::new());
    for (data, pts) in input {
        let mut buf = h.alloc(data);
        buf.pts = *pts;
        h.push(buf).expect("push");
        for b in h.drain_outputs() {
            stamps.push(b.pts);
            pcm.extend_from_slice(b.memory.data());
        }
    }
    (pcm, stamps)
}

fn end(h: &mut Harness, pcm: &mut Vec<u8>) {
    for b in h.eos().expect("eos") {
        pcm.extend_from_slice(b.memory.data());
    }
}

fn stream(all: &[(Vec<u8>, u64)]) -> Vec<(&[u8], Timestamp)> {
    all.iter()
        .enumerate()
        .map(|(i, (d, _))| (d.as_slice(), if i == 0 { Timestamp::ZERO } else { Timestamp::NONE }))
        .collect()
}

fn worst(got: &[u8], want: &[u8]) -> i32 {
    let s =
        |b: &[u8]| b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect::<Vec<_>>();
    s(got)
        .iter()
        .zip(s(want).iter())
        .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
        .max()
        .unwrap_or(0)
}

/// ffmpeg cuts the end to the last granule, the element may run a block longer.
fn matches_ffmpeg(name: &str, got: &[u8], want: &[u8]) {
    let (g, w) = (got.len() / STRIDE, want.len() / STRIDE);
    eprintln!("{name}: {g} frames, ffmpeg {w}");
    assert!(g >= w && g - w <= 2048, "{name}: {g} frames against ffmpeg's {w}");
    assert!(worst(got, want) <= 1, "{name}: a sample {} steps off ffmpeg", worst(got, want));
}

#[test]
fn in_band_headers_decode_as_ffmpeg_does() {
    let Some((ogg, want)) = fixture() else { return };
    let all = packets(&ogg);
    assert!(all[0].0.starts_with(b"\x01vorbis") && all[2].0.starts_with(b"\x05vorbis"));
    let mut h = Harness::with_slot_size(LibavAudioDec::vorbis(), SLOT);
    let (mut got, stamps) = feed(&mut h, &stream(&all));
    end(&mut h, &mut got);
    matches_ffmpeg("in-band", &got, &want);
    assert_eq!(stamps.first(), Some(&Timestamp::ZERO), "from the stream's first sample");
    assert_eq!(warnings(&mut h), 0);
}

#[test]
fn a_laced_head_decodes_as_the_in_band_headers_do() {
    let Some((ogg, want)) = fixture() else { return };
    let all = packets(&ogg);
    let head = laced(&all);
    let mut input: Vec<(&[u8], Timestamp)> = vec![(&head, Timestamp::NONE)];
    input.extend(all[3..].iter().map(|(d, _)| (d.as_slice(), Timestamp::NONE)));
    input[1].1 = Timestamp::ZERO;
    let mut h = Harness::with_slot_size(LibavAudioDec::vorbis(), SLOT);
    let (mut got, _) = feed(&mut h, &input);
    end(&mut h, &mut got);
    matches_ffmpeg("laced", &got, &want);
    assert_eq!(warnings(&mut h), 0);

    let mut h = Harness::with_slot_size(LibavAudioDec::vorbis(), SLOT);
    let (mut in_band, _) = feed(&mut h, &stream(&all));
    end(&mut h, &mut in_band);
    assert!(got == in_band, "the same samples either way");
}

fn landing(all: &[(Vec<u8>, u64)], page: usize) -> (usize, u64) {
    let starts: Vec<usize> =
        (4..all.len()).filter(|&i| all[i].1 != all[i - 1].1 && all[i - 1].1 > 0).collect();
    let i = starts[page.min(starts.len() - 1)];
    (i, all[i - 1].1)
}

#[derive(Clone, Copy)]
struct Landing {
    again: bool,
    lead: bool,
    past: u64,
}

fn restarts_in_place(ogg: &[u8], want: &[u8], page: usize, how: Landing) {
    let all = packets(ogg);
    let mut h = Harness::with_slot_size(LibavAudioDec::vorbis(), SLOT);
    let _ = feed(&mut h, &stream(&all[..all.len() / 2]));
    let (from, granule) = landing(&all, page);
    let stamp = Timestamp(time_at(granule, RATE).0);
    let at = time_at(granule + how.past, RATE);
    h.seek(0, at).expect("seek");
    let _ = h.drain_outputs();
    let mut input: Vec<(&[u8], Timestamp)> = Vec::new();
    if how.again {
        input.extend(all[..3].iter().map(|(d, _)| (d.as_slice(), Timestamp::NONE)));
    }
    if how.lead {
        input.push((&all[from - 1].0, Timestamp::NONE));
    }
    input.push((&all[from].0, stamp));
    input.extend(all[from + 1..].iter().map(|(d, _)| (d.as_slice(), Timestamp::NONE)));
    let (mut got, stamps) = feed(&mut h, &input);
    end(&mut h, &mut got);
    let (first, point) = (position_at(stamps[0], RATE), position_at(at, RATE));
    match (how.lead, point > granule + 4096) {
        (true, _) | (false, true) => {
            assert_eq!(first, point, "page {page}: from the point, the landing at {granule}")
        }
        // The first packet primes the overlap and puts nothing out.
        (false, false) => {
            assert!(
                first > granule && first <= granule + 4096,
                "out from {first}, the landing at {granule}"
            );
            assert!(first >= point, "nothing before the point");
        }
    }
    let from = first as usize * STRIDE;
    let n = got.len().min(want.len() - from);
    let left = (want.len() - from).min(RATE as usize * STRIDE / 4);
    assert!(n >= left, "a quarter second behind the seek or to the end, {n} bytes");
    let off = worst(&got[..n], &want[from..from + n]);
    assert!(
        off <= 1,
        "page {page}: {off} steps off ffmpeg's unbroken decode at the stamped sample"
    );
    assert_eq!(warnings(&mut h), 0, "headers known again are no packets");
}

const BARE: Landing = Landing { again: false, lead: false, past: 0 };

#[test]
fn the_headers_outlast_a_flush_and_the_restart_is_in_place() {
    let Some((ogg, want)) = fixture() else { return };
    restarts_in_place(&ogg, &want, 6, BARE);
}

#[test]
fn headers_handed_over_again_behind_a_flush_are_dropped() {
    let Some((ogg, want)) = fixture() else { return };
    restarts_in_place(&ogg, &want, 6, Landing { again: true, ..BARE });
    restarts_in_place(&ogg, &want, 6, Landing { again: true, lead: true, ..BARE });
}

#[test]
fn a_restart_ahead_of_the_point_is_cut_to_it() {
    let Some((ogg, want)) = fixture() else { return };
    restarts_in_place(&ogg, &want, 6, Landing { past: 6_000, ..BARE });
    restarts_in_place(&ogg, &want, 6, Landing { lead: true, past: 300, ..BARE });
}

#[test]
fn behind_the_landing_page_s_last_packet_every_page_lands_on_its_granule() {
    let wave = "0.3*sin(2*PI*440*t)+if(eq(mod(n,3001),0),0.9,0)";
    let args = [
        "-f",
        "lavfi",
        "-i",
        &format!("aevalsrc='{wave}|{wave}':s=44100:d=4"),
        "-c:a",
        "libvorbis",
        "-b:a",
        "128k",
        "-page_duration",
        "40000",
        "-f",
        "ogg",
    ];
    let Some(ogg) = support::make("clicks.ogg", &args) else {
        return;
    };
    let Some(want) =
        support::oracle("clicks.ogg", "clicks.ogg.s16", &["-f", "s16le", "-c:a", "pcm_s16le"])
    else {
        return;
    };
    let all = packets(&ogg);
    let pages = (4..all.len()).filter(|&i| all[i].1 != all[i - 1].1 && all[i - 1].1 > 0).count();
    assert!(pages > 8, "{pages} pages");
    for page in 0..pages - 2 {
        restarts_in_place(&ogg, &want, page, Landing { lead: true, ..BARE });
    }
}

#[test]
fn a_restart_stamped_zero_is_the_stream_start() {
    let Some((ogg, want)) = fixture() else { return };
    let all = packets(&ogg);
    let mut h = Harness::with_slot_size(LibavAudioDec::vorbis(), SLOT);
    let _ = feed(&mut h, &stream(&all[..all.len() / 2]));
    h.seek(0, Timestamp::ZERO).expect("seek");
    let _ = h.drain_outputs();
    let mut input: Vec<(&[u8], Timestamp)> =
        all[3..].iter().map(|(d, _)| (d.as_slice(), Timestamp::NONE)).collect();
    input[0].1 = Timestamp::ZERO;
    let (mut got, stamps) = feed(&mut h, &input);
    end(&mut h, &mut got);
    assert_eq!(stamps.first(), Some(&Timestamp::ZERO));
    matches_ffmpeg("from zero again", &got, &want);
}

fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn survives(name: &str, input: &[Vec<u8>], audio: &[(Vec<u8>, u64)]) -> usize {
    let mut h = Harness::with_slot_size(LibavAudioDec::vorbis(), SLOT);
    let mut all: Vec<(&[u8], Timestamp)> =
        input.iter().map(|d| (d.as_slice(), Timestamp::NONE)).collect();
    all.extend(audio.iter().map(|(d, _)| (d.as_slice(), Timestamp::NONE)));
    let (mut pcm, _) = feed(&mut h, &all);
    end(&mut h, &mut pcm);
    let _ = warnings(&mut h);
    let _ = name;
    pcm.len()
}

#[test]
fn hostile_and_truncated_headers_never_panic() {
    let Some((ogg, _)) = fixture() else { return };
    let all = packets(&ogg);
    let (ident, comment, setup) = (&all[0].0, &all[1].0, &all[2].0);
    let audio = &all[3..40];

    assert_eq!(survives("no headers", &[], audio), 0, "no headers, no sound");
    // Cut at the framing flag libavcodec reads past the end and may take it.
    for n in [1, 7, 8, 16, 28] {
        let out = survives("ident", &[ident[..n].to_vec(), comment.clone(), setup.clone()], audio);
        assert_eq!(out, 0, "an identification header of {n} bytes, {out} out");
    }
    survives("ident", &[ident[..29].to_vec(), comment.clone(), setup.clone()], audio);
    for n in [1, 7, 8, 9, 12, 40, setup.len() / 3, setup.len() / 2, setup.len() - 1] {
        survives("setup cut", &[ident.clone(), comment.clone(), setup[..n].to_vec()], audio);
    }
    for (i, at) in [8usize, 9, 30, setup.len() / 2, setup.len() - 2].into_iter().enumerate() {
        let mut bad = setup.clone();
        bad[at] ^= 0x5A ^ i as u8;
        survives("setup flipped", &[ident.clone(), comment.clone(), bad], audio);
    }
    survives("order", &[setup.clone(), comment.clone(), ident.clone()], audio);
    survives("twice", &[ident.clone(), ident.clone(), setup.clone(), setup.clone()], audio);
    let head = laced(&all);
    for n in (1..head.len()).step_by(97).chain([2, 3, 4, head.len() - 1]) {
        survives("laced cut", &[head[..n].to_vec()], audio);
    }
    let mut long = vec![2u8];
    long.extend(std::iter::repeat_n(0xFF, 64));
    survives("laced lengths", &[long], audio);
    survives("laced zero", &[vec![2, 0, 0], vec![2, 0, 0, 1, 5]], audio);
    for seed in 0..8u64 {
        let mut n = noise(seed, 64 + seed as usize * 37);
        n[0] = [1u8, 3, 5, 2][seed as usize % 4];
        if seed % 2 == 0 {
            n[1..7].copy_from_slice(b"vorbis");
        }
        survives("noise", &[n.clone(), ident.clone(), comment.clone(), setup.clone(), n], audio);
    }
    let mut noisy: Vec<(Vec<u8>, u64)> = audio.to_vec();
    for (i, p) in noisy.iter_mut().enumerate().skip(3).step_by(5) {
        p.0 = noise(i as u64, p.0.len());
        p.0[0] &= 0xFE;
    }
    let out = survives("noisy audio", &[ident.clone(), comment.clone(), setup.clone()], &noisy);
    assert!(out > 0, "the good packets still decode");
}
