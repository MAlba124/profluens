//! pf-libflac on untrusted bytes, and the encoder against the decoder.
//! Knob byte bit 0 clear decodes, bits 1-3 piece size, 4-7 resync frame. Bit 0 set encodes,
//! bits 1-2 format, 3-5 channels, 6 streaming, 7 smooth, then rate, block, step bytes, then PCM.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_libflac::index::{GROWN_SPACING, Grown, MAX_GROWN, samples_ns, table_entries};
use pf_libflac::{
    DecodeError, EncodeError, FlacDecoder, FlacEncoder, FlacTags, SampleFormat, StreamDecoder,
    streaminfo_offset,
};
use pf_tags::flacmeta::{self as tags, SeekPoint};
use profluens_core::event::TagSink;

const PIECES: [usize; 8] = [1, 7, 64, 333, 4096, 1 << 16, 1 << 20, usize::MAX];
const CAP: usize = 1 << 22;
const FORMATS: [SampleFormat; 4] =
    [SampleFormat::S8, SampleFormat::S16, SampleFormat::S24, SampleFormat::S32];
const RATES: [u32; 8] = [44_100, 48_000, 8_000, 96_000, 1, 22_050, 192_000, (1 << 20) - 1];
const BLOCKS: [usize; 8] = [1, 15, 16, 192, 1152, 4096, 4608, 9000];
const STREAMINFO_END: usize = 42;

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    metadata(rest);
    grown(rest);
    if knob & 1 == 0 {
        decode(rest, knob);
    } else if let Some((&shape, pcm)) = rest.split_first() {
        round_trip(knob, shape, pcm);
    }
});

#[derive(Default)]
struct Got {
    text: Vec<(String, String)>,
    pics: Vec<(String, Vec<u8>)>,
}

impl TagSink for Got {
    fn text(&mut self, key: &str, value: &str) {
        self.text.push((key.to_string(), value.to_string()));
    }
    fn picture(&mut self, mime: &str, data: &[u8]) {
        self.pics.push((mime.to_string(), data.to_vec()));
    }
}

fn metadata(b: &[u8]) {
    let owned = tags::parse(b);
    let mut got = Got::default();
    tags::parse_into(b, &mut got);
    assert!(
        owned.comments.len() + owned.pictures.len() <= b.len() / 4,
        "more tags than the bytes hold"
    );
    assert!(owned.pictures.iter().map(|(_, d)| d.len()).sum::<usize>() <= b.len());
    for (k, _) in &got.text {
        assert!(
            k.len() > 256 || !k.bytes().any(|c| c.is_ascii_lowercase()),
            "a lowercase key {k:?}"
        );
    }
    let upper: Vec<(String, String)> =
        got.text.into_iter().map(|(k, v)| (k.to_ascii_uppercase(), v)).collect();
    assert_eq!(
        owned,
        FlacTags { comments: upper, pictures: got.pics },
        "parse and parse_into disagree"
    );
    if let Some(si) = tags::stream_info(b) {
        assert!((1..=8).contains(&si.channels) && (1..=32).contains(&si.bits_per_sample));
        assert!(si.sample_rate < 1 << 20 && si.total_samples < 1 << 36);
    }
    let start = tags::audio_start(b);
    assert!(start.is_none_or(|n| (8..=b.len()).contains(&n)), "the audio starts at {start:?}");
    let points: Vec<SeekPoint> = tags::seek_table(b).collect();
    assert!(points.len() <= b.len() / 18 && points.iter().all(|p| p.sample != u64::MAX));
    let rate = tags::stream_info(b).map_or(0, |s| s.sample_rate);
    let len = Some(b.len() as u64);
    let at = start.unwrap_or(0) as u64;
    let entries = table_entries(&points, at, rate, len);
    assert!(entries.len() <= points.len() && (rate > 0 || entries.is_empty()));
    for w in entries.windows(2) {
        assert!(
            w[0].time_ns < w[1].time_ns && w[0].byte <= w[1].byte,
            "seek table entries out of order"
        );
    }
    assert!(entries.iter().all(|e| e.byte < b.len() as u64 && e.byte >= at));
    for p in &points {
        let ns = samples_ns(p.sample, rate);
        assert_eq!(ns.is_some(), rate > 0);
        let exact = u128::from(p.sample) * 1_000_000_000 / u128::from(rate.max(1));
        assert!(ns.is_none_or(|n| u128::from(n) == exact.min(u128::from(u64::MAX))));
    }
}

fn grown(b: &[u8]) {
    let mut g = Grown::default();
    for &[t0, t1, t2, t3, b0, b1] in b.as_chunks::<6>().0.iter().take(512) {
        let time = u64::from(u32::from_le_bytes([t0, t1, t2, t3])) * 10_000_000;
        let byte = u64::from(u16::from_le_bytes([b0, b1])) * 64;
        let before = g.entries.len();
        let full = g.frame(time, byte);
        assert!(g.entries.len() - before <= 1);
        assert_eq!(
            full,
            g.entries.len() >= g.published + pf_libflac::index::GROWN_EVERY
                && g.entries.len() > before
        );
        assert_eq!(g.unpublished(), g.entries.len() != g.published);
        if full {
            g.published = g.entries.len();
        }
    }
    assert!(g.entries.len() <= MAX_GROWN);
    for w in g.entries.windows(2) {
        assert!(
            w[1].time_ns - w[0].time_ns >= GROWN_SPACING && w[0].byte < w[1].byte,
            "grown entries too close"
        );
    }
}

fn frame_head(b: &[u8], max_block: u32) -> (usize, usize, u64) {
    let (strategy, bs, ch) = (b[1] & 1, b[2] >> 4, b[3] >> 4);
    let lead = b[4];
    let n = (lead.leading_ones() as usize).max(1);
    let mut coded = u64::from(lead) & if n == 1 { 0x7F } else { 0x7F >> n };
    for &c in &b[5..4 + n] {
        coded = (coded << 6) | u64::from(c & 0x3F);
    }
    let at = 4 + n;
    let block = match bs {
        1 => 192,
        2..=5 => 576 << (bs - 2),
        6 => usize::from(b[at]) + 1,
        7 => usize::from(u16::from_be_bytes([b[at], b[at + 1]])) + 1,
        _ => 256 << (bs - 8),
    };
    let channels = if ch < 8 { usize::from(ch) + 1 } else { 2 };
    let step = if max_block > 0 { u64::from(max_block) } else { block as u64 };
    let first = if strategy == 0 { coded.saturating_mul(step) } else { coded };
    (block, channels, first)
}

type Frame = (Vec<i32>, Option<(u64, u64)>);

#[derive(Debug, PartialEq)]
struct Run {
    frames: Vec<Frame>,
    total: usize,
    end: Result<(), DecodeError>,
    capped: bool,
    errors: u64,
}

fn stream(d: &mut StreamDecoder, b: &[u8], piece: usize) -> Run {
    stream_cut(d, b, 0, piece)
}

fn stream_cut(d: &mut StreamDecoder, b: &[u8], first: usize, piece: usize) -> Run {
    let mut run = Run { frames: Vec::new(), total: 0, end: Ok(()), capped: false, errors: 0 };
    let mut out = Vec::new();
    let (head, rest) = b.split_at(first.min(b.len()));
    let pieces = (!head.is_empty())
        .then_some(head)
        .into_iter()
        .chain(rest.chunks(piece.min(rest.len()).max(1)));
    for chunk in pieces {
        d.push(chunk);
        loop {
            match d.pull_into(&mut out) {
                Ok(Some(())) => {
                    run.total += out.len();
                    run.frames.push((std::mem::take(&mut out), d.last_frame()));
                    if run.total > CAP {
                        run.capped = true;
                        return run;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    run.end = Err(e);
                    run.errors = d.errors();
                    return run;
                }
            }
        }
    }
    run.errors = d.errors();
    run
}

fn decode(b: &[u8], knob: u8) {
    // A partial frame is decoded again each push, so a piece is at least a 64th of the input.
    let piece = PIECES[usize::from(knob >> 1 & 7)].max(b.len() / 64);
    let mut d = StreamDecoder::new();
    let run = stream(&mut d, b, piece);
    if let Some(info) = d.info() {
        let si = tags::stream_info(b).expect("a header the decoder took");
        assert_eq!(
            (
                si.sample_rate,
                u32::from(si.channels),
                u32::from(si.bits_per_sample),
                si.total_samples
            ),
            (info.sample_rate, info.channels, info.bits_per_sample, info.total_samples)
        );
        assert_eq!(
            d.audio_start(),
            tags::audio_start(b).map(|n| n as u64),
            "the decoder and the walk end the header apart"
        );
        assert_eq!(d.seek_table(), tags::seek_table(b).collect::<Vec<_>>().as_slice());
        assert_eq!(d.tags(), &tags::parse(b));
        for (samples, at) in &run.frames {
            let (first, byte) = at.expect("a frame decoded from byte 0 is placed");
            let (block, channels, want) = frame_head(&b[byte as usize..], info.max_block_size);
            assert_eq!(
                samples.len(),
                block * channels,
                "a frame of {block} x {channels} gave {}",
                samples.len()
            );
            assert_eq!(first, want, "the frame at {byte} starts elsewhere");
        }
        for w in run.frames.windows(2) {
            assert!(w[0].1.map(|a| a.1) < w[1].1.map(|a| a.1), "frames out of byte order");
        }
    }
    if !run.capped {
        whole_agrees(b, &d, &run);
        let other = PIECES[(usize::from(knob >> 1 & 7) + 3) % 8].max(b.len() / 64);
        let again = stream(&mut StreamDecoder::new(), b, other);
        assert_eq!(
            again.frames.len(),
            run.frames.len(),
            "pieces of {other} and {piece} decode apart"
        );
        assert_eq!(
            (&again.end, again.errors),
            (&run.end, run.errors),
            "pieces of {other} and {piece} end apart"
        );
        assert!(again == run, "pieces of {other} and {piece} decode apart");
        behind_id3(b, knob, &run, d.audio_start());
    }
    resync(b, &run, usize::from(knob >> 4));
}

fn behind_id3(b: &[u8], knob: u8, run: &Run, start: Option<u64>) {
    if tags::id3v2_len(b).is_some() {
        return;
    }
    let body = [0usize, 1, 200, 5000][usize::from(knob >> 4 & 3)];
    let mut tagged = b"ID3\x04\x00".to_vec();
    tagged.push(if knob & 0x40 != 0 { 0x10 } else { 0 });
    tagged.extend((0..4).rev().map(|i| ((body >> (7 * i)) & 0x7F) as u8));
    tagged.resize(10 + body, 0xFF);
    if knob & 0x40 != 0 {
        tagged.extend_from_slice(b"3DI\x04\x00\x10\x00\x00\x00\x00");
    }
    let tag = tagged.len();
    assert_eq!(tags::id3v2_len(&tagged), Some(tag));
    tagged.extend_from_slice(b);
    assert_eq!(tags::audio_start(&tagged), tags::audio_start(b).map(|n| n + tag));
    assert_eq!(tags::stream_info(&tagged), tags::stream_info(b));
    let piece = PIECES[usize::from(knob >> 1 & 7)].max(tagged.len() / 64);
    let mut d = StreamDecoder::new();
    let got = stream_cut(&mut d, &tagged, 1 + body / 2, piece);
    if b.starts_with(b"fLaC") {
        let moved: Vec<Frame> = run
            .frames
            .iter()
            .map(|(s, at)| (s.clone(), at.map(|(f, byte)| (f, byte + tag as u64))))
            .collect();
        assert!(got.frames == moved, "a tag of {tag} changed the frames");
        assert_eq!(
            (&got.end, got.errors, got.capped),
            (&run.end, run.errors, run.capped),
            "a tag of {tag}"
        );
        assert_eq!(d.audio_start(), start.map(|n| n + tag as u64));
    } else {
        assert!(got.frames.is_empty() && d.info().is_none(), "frames behind a tag and no marker");
    }
}

fn whole_agrees(b: &[u8], d: &StreamDecoder, run: &Run) {
    let samples: Vec<i64> =
        run.frames.iter().flat_map(|(s, _)| s.iter().map(|&v| i64::from(v))).collect();
    match FlacDecoder::decode(b) {
        Ok(w) => {
            assert_eq!(run.end, Ok(()), "pieces failed where the whole stream decodes");
            assert!(
                d.buffered().len() < 2,
                "{} bytes left past the last frame",
                d.buffered().len()
            );
            assert_eq!(Some(&w.info), d.info());
            assert_eq!(&w.tags, d.tags());
            assert_eq!(w.errors, run.errors, "errors whole and in pieces");
            assert!(
                w.samples == samples,
                "{} samples whole, {} in pieces",
                w.samples.len(),
                samples.len()
            );
        }
        Err(DecodeError::UnexpectedEof) => {
            assert_eq!(run.end, Ok(()), "pieces failed on a short stream")
        }
        Err(e) => assert_eq!(run.end, Err(e), "pieces fail otherwise"),
    }
}

fn resync(b: &[u8], run: &Run, j: usize) {
    let Some((_, Some((_, at)))) = run.frames.get(j) else {
        return;
    };
    let at = *at as usize;
    let mut d = StreamDecoder::new();
    d.push(&b[..at]);
    while let Ok(Some(())) = d.pull_into(&mut Vec::new()) {}
    if d.info().is_none() {
        return;
    }
    d.seek_reset();
    assert_eq!(d.last_frame(), None);
    let mut out = Vec::new();
    d.push(&b[at..]);
    d.restart_at(at as u64);
    for (k, (want, place)) in run.frames[j..].iter().enumerate() {
        match d.pull_into(&mut out) {
            Ok(Some(())) => {
                assert!(&out == want, "frame {} after a resync on {j}", j + k);
                assert_eq!(
                    d.last_frame(),
                    *place,
                    "frame {} placed elsewhere after a resync",
                    j + k
                );
            }
            other => panic!("frame {} after a resync on {j} gave {other:?}", j + k),
        }
    }
    if !run.capped {
        let end = d.pull_into(&mut out).map(|_| ());
        assert_eq!(end, run.end, "a resync ends otherwise");
    }
}

fn wrap(v: i64, bits: u32) -> i64 {
    let s = 64 - bits;
    (v << s) >> s
}

fn round_trip(knob: u8, shape: u8, raw: &[u8]) {
    let format = FORMATS[usize::from(knob >> 1 & 3)];
    let channels = u32::from(knob >> 3 & 7) + 1;
    let streaming = knob & 0x40 != 0;
    let rate = RATES[usize::from(shape & 7)];
    let block = BLOCKS[usize::from(shape >> 3 & 7)];
    let bits = format.bits_per_sample();
    let width = format.bytes_per_sample();
    let stride = width * channels as usize;

    let step = [0u32, 4, 8, bits.saturating_sub(8)][usize::from(shape >> 6)];
    let want: Vec<i64> = if knob & 0x80 == 0 {
        raw.chunks_exact(width)
            .take(raw.len() / stride * channels as usize)
            .map(|s| {
                let mut v = [0u8; 8];
                v[..width].copy_from_slice(s);
                wrap(i64::from_le_bytes(v), bits)
            })
            .collect()
    } else {
        let mut acc = vec![0i64; channels as usize];
        raw.iter()
            .take(raw.len() - raw.len() % channels as usize)
            .enumerate()
            .map(|(i, &d)| {
                let c = &mut acc[i % channels as usize];
                *c = wrap(*c + (i64::from(d as i8) << step), bits);
                *c
            })
            .collect()
    };
    let mut want = want;
    let most = (block.min(4608) * 256).min(16_384 / channels as usize);
    want.truncate(most * channels as usize);
    let mut pcm = Vec::with_capacity(want.len() * width);
    for v in &want {
        pcm.extend_from_slice(&v.to_le_bytes()[..width]);
    }
    let frames = want.len() / channels as usize;

    assert!(
        FlacEncoder::new(rate, 9, format).is_err()
            && FlacEncoder::new(0, channels, format).is_err()
    );
    let declared = (block as u32).clamp(16, 4608);
    let made = if streaming {
        FlacEncoder::new_streaming(rate, channels, format, declared)
    } else {
        FlacEncoder::new(rate, channels, format)
    };
    let (mut enc, mut out) = made.expect("parameters in range");
    let block = if streaming { block.min(declared as usize) } else { block };
    for chunk in pcm.chunks(block * stride) {
        enc.encode_interleaved(chunk, &mut out).expect("whole interchannel samples");
    }
    if stride > 1 {
        let before = out.len();
        assert_eq!(enc.encode_interleaved(&[0], &mut out), Err(EncodeError::RaggedInput));
        assert_eq!(out.len(), before, "a ragged block wrote");
    }
    let body = enc.finish(&mut out);
    if !streaming {
        out[streaminfo_offset()..STREAMINFO_END].copy_from_slice(&body);
    }

    let dec = FlacDecoder::decode(&out).expect("the encoder's stream decodes");
    assert!(dec.samples == want, "{} samples back of {}", dec.samples.len(), want.len());
    let i = dec.info;
    assert_eq!((i.sample_rate, i.channels, i.bits_per_sample), (rate, channels, bits));
    if streaming {
        assert_eq!((i.total_samples, i.max_block_size, i.min_frame_size), (0, declared, 0));
    } else {
        assert_eq!(i.total_samples, frames as u64);
        // libFLAC writes its block size, 4608, whatever the chunks.
        assert!(frames == 0 || i.max_block_size == 4608);
    }
    let si = tags::stream_info(&out).expect("STREAMINFO");
    assert_eq!(
        (si.sample_rate, u32::from(si.channels), u32::from(si.bits_per_sample)),
        (rate, channels, bits)
    );
    assert_eq!(tags::audio_start(&out), Some(STREAMINFO_END));

    let mut d = StreamDecoder::new();
    let run = stream(&mut d, &out, (1 + usize::from(shape) * 37).max(out.len() / 64));
    assert_eq!(run.end, Ok(()));
    let mut at = 0u64;
    for (k, (s, place)) in run.frames.iter().enumerate() {
        let n = s.len() / channels as usize;
        assert_eq!(s.len() % channels as usize, 0);
        assert!(n >= 1 && n as u32 <= i.max_block_size.max(1), "frame {k} of {n} samples");
        let (first, _) = place.expect("placed");
        if block <= 4608 && (block as u32 == i.max_block_size || run.frames.len() == 1) {
            assert_eq!(first, at, "frame {k} numbered {first}, starts at {at}");
        }
        at += n as u64;
    }
    assert_eq!(at, frames as u64);

    let a = String::from_utf8_lossy(&raw[..raw.len().min(40)]).into_owned();
    let lines = [format!("Title={a}"), "ARTIST=x".to_string(), "x=y=z".to_string()];
    let art = &raw[..raw.len().min(300)];
    let mut tagged = out[..STREAMINFO_END].to_vec();
    tagged[4] &= 0x7F;
    tagged.extend_from_slice(&pf_tags::fixture::flac_block(
        false,
        4,
        &pf_tags::fixture::vorbis_comment(&lines),
    ));
    tagged.extend_from_slice(&pf_tags::fixture::flac_block(
        true,
        6,
        &pf_tags::fixture::picture_block("image/png", art),
    ));
    let head = tagged.len();
    tagged.extend_from_slice(&out[STREAMINFO_END..]);
    let dec = FlacDecoder::decode(&tagged).expect("a tagged stream decodes");
    assert!(dec.samples == want, "the tags changed the samples");
    let comments = vec![
        ("TITLE".to_string(), a),
        ("ARTIST".to_string(), "x".to_string()),
        ("X".to_string(), "y=z".to_string()),
    ];
    assert_eq!(
        dec.tags,
        FlacTags { comments, pictures: vec![("image/png".to_string(), art.to_vec())] }
    );
    assert_eq!(tags::audio_start(&tagged), Some(head));
}
