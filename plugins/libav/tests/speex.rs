//! `libavspeexdec` against the system ffmpeg's decode of an Ogg Speex stream made with libspeex.
//! The fixture is made at test time, skipped where ffmpeg or libspeex is missing:
//!
//! ```text
//! ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=16000:duration=2" -c:a libspeex -f ogg tone.spx
//! ffmpeg -i tone.spx -fps_mode passthrough -f s16le -c:a pcm_s16le tone.spx.s16
//! ```

// Test code.
#![allow(clippy::disallowed_methods)]

mod support;

use pf_harness::Harness;
use pf_libav::LibavAudioDec;
use profluens_core::bus::BusMessage;
use profluens_core::format::Value;
use profluens_core::time::Timestamp;

const SLOT: usize = 64 * 1024;
const RATE: i64 = 16_000;

fn fixture() -> Option<(Vec<u8>, Vec<u8>)> {
    let args = [
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=16000:duration=2",
        "-c:a",
        "libspeex",
        "-f",
        "ogg",
    ];
    let ogg = support::make("tone.spx", &args)?;
    let pcm = support::oracle("tone.spx", "tone.spx.s16", &["-f", "s16le", "-c:a", "pcm_s16le"])?;
    Some((ogg, pcm))
}

fn packets(ogg: &[u8]) -> Vec<Vec<u8>> {
    pf_ogg::demux_all(ogg).into_iter().map(|p| p.data).collect()
}

fn announced(h: &Harness, field: &str) -> Option<i64> {
    match h.announced()?.get(h.vocabulary().field_id(field)?)? {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

fn warnings(h: &mut Harness) -> usize {
    h.bus_messages().into_iter().filter(|m| matches!(m, BusMessage::Warning { .. })).count()
}

/// The PCM of `input`, the first packet stamped `first`.
fn decode_from(h: &mut Harness, input: &[&[u8]], first: Timestamp) -> (Vec<u8>, Vec<Timestamp>) {
    let (mut pcm, mut stamps) = (Vec::new(), Vec::new());
    let mut take = |out: Vec<profluens_core::buffer::Buffer>| {
        for b in out {
            stamps.push(b.pts);
            pcm.extend_from_slice(b.memory.data());
        }
    };
    for (i, data) in input.iter().enumerate() {
        let mut buf = h.alloc(data);
        buf.pts = if i == 0 { first } else { Timestamp::NONE };
        h.push(buf).expect("push");
        take(h.drain_outputs());
    }
    take(h.eos().expect("eos"));
    (pcm, stamps)
}

fn decode(h: &mut Harness, input: &[&[u8]]) -> (Vec<u8>, Vec<Timestamp>) {
    decode_from(h, input, Timestamp::ZERO)
}

fn samples(b: &[u8]) -> Vec<i16> {
    b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect()
}

/// ffmpeg drops the samples ahead of the first granule, the element keeps every decoded one, so
/// ffmpeg's decode lines up with the element's tail.
fn matches_ffmpeg(name: &str, got: &[u8], want: &[u8]) {
    let (g, w) = (samples(got), samples(want));
    eprintln!("{name}: {} samples, ffmpeg {}", g.len(), w.len());
    assert!(
        g.len() >= w.len() && g.len() - w.len() < 640,
        "{name}: {} against {}",
        g.len(),
        w.len()
    );
    let lead = g.len() - w.len();
    let worst = g[lead..].iter().zip(&w).map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs()).max();
    assert!(worst.unwrap_or(0) <= 1, "{name}: a sample {worst:?} steps off ffmpeg");
}

#[test]
fn in_band_headers_decode_as_ffmpeg_does() {
    let Some((ogg, want)) = fixture() else { return };
    let all = packets(&ogg);
    assert!(all[0].starts_with(b"Speex   ") && all[0].len() >= 80);
    let input: Vec<&[u8]> = all.iter().map(Vec::as_slice).collect();
    let mut h = Harness::with_slot_size(LibavAudioDec::speex(), SLOT);
    let (got, stamps) = decode(&mut h, &input);
    matches_ffmpeg("in-band", &got, &want);
    assert_eq!(announced(&h, "rate"), Some(RATE));
    assert_eq!(announced(&h, "channels"), Some(1));
    assert_eq!(stamps.first(), Some(&Timestamp::ZERO), "from the stream's first sample");
    assert_eq!(warnings(&mut h), 0);
}

/// A seek lands on audio, the demuxer may hand the headers over again ahead of it.
#[test]
fn the_headers_outlast_a_flush_and_again_they_are_dropped() {
    let Some((ogg, _)) = fixture() else { return };
    let all = packets(&ogg);
    let input: Vec<&[u8]> = all.iter().map(Vec::as_slice).collect();
    let mut whole = Harness::with_slot_size(LibavAudioDec::speex(), SLOT);
    let (unbroken, _) = decode(&mut whole, &input);

    let from = all.len() / 2;
    // One 20 ms frame a packet, the headers ahead of the first.
    let at = Timestamp::from_nanos((from as u64 - 2) * 20_000_000);
    for again in [false, true] {
        let mut h = Harness::with_slot_size(LibavAudioDec::speex(), SLOT);
        let _ = decode(&mut h, &input[..from]);
        h.seek(0, at).expect("seek");
        let _ = h.drain_outputs();
        let mut tail: Vec<&[u8]> = if again { input[..2].to_vec() } else { Vec::new() };
        tail.extend_from_slice(&input[from..]);
        let (got, stamps) = decode_from(&mut h, &tail, at);
        assert_eq!(stamps.first(), Some(&at), "again {again}");
        assert!(got.len() > 16_000, "again {again}: {} bytes behind the seek", got.len());
        // Speex predicts from the frames before, a few after a jump differ, not the tail.
        let n = got.len() / 2;
        assert!(got[n..] == unbroken[unbroken.len() - (got.len() - n)..], "again {again}");
        assert_eq!(warnings(&mut h), 0, "again {again}");
    }
}

#[test]
fn packets_ahead_of_the_identification_header_are_dropped() {
    let Some((ogg, _)) = fixture() else { return };
    let all = packets(&ogg);
    let mut h = Harness::with_slot_size(LibavAudioDec::speex(), SLOT);
    let input: Vec<&[u8]> = all[2..12].iter().map(Vec::as_slice).collect();
    let (got, _) = decode(&mut h, &input);
    assert!(got.is_empty(), "no header, no sound");
    assert!(warnings(&mut h) == 0, "a drop is logged, never posted");
}

#[test]
fn hostile_headers_never_panic() {
    let Some((ogg, _)) = fixture() else { return };
    let all = packets(&ogg);
    let audio: Vec<&[u8]> = all[2..20].iter().map(Vec::as_slice).collect();
    let ident = &all[0];
    let mut cases: Vec<Vec<u8>> = Vec::new();
    for n in [8, 40, 79] {
        cases.push(ident[..n].to_vec());
    }
    for at in [36, 40, 44, 48, 56, 64, 68] {
        for v in [0i32, -1, 3, i32::MAX] {
            let mut bad = ident.clone();
            bad[at..at + 4].copy_from_slice(&v.to_le_bytes());
            cases.push(bad);
        }
    }
    for head in &cases {
        let mut h = Harness::with_slot_size(LibavAudioDec::speex(), SLOT);
        let mut input: Vec<&[u8]> = vec![head, &all[1]];
        input.extend_from_slice(&audio);
        let _ = decode(&mut h, &input);
        let _ = warnings(&mut h);
    }
}
