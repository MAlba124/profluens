//! `flacdec` through the element [`Harness`], its format announcement, carry and EOS flush.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

mod support;

use pf_harness::Harness;
use pf_libflac::{FlacDecoder, FlacEncoder, SampleFormat};
use profluens_core::format::Value;

const RATE: u32 = 22_050;
const CHANNELS: u32 = 2;

fn gen_s16(frames: usize) -> (Vec<u8>, Vec<i64>) {
    let mut bytes = Vec::with_capacity(frames * CHANNELS as usize * 2);
    let mut expected = Vec::with_capacity(frames * CHANNELS as usize);
    for i in 0..frames {
        for c in 0..CHANNELS {
            let phase = 2.0 * std::f64::consts::PI * (3.0 + c as f64) * (i as f64) / 384.0;
            let s = (11_000.0 * phase.sin()).round() as i64;
            expected.push(s);
            bytes.extend_from_slice(&(s as i16).to_le_bytes());
        }
    }
    (bytes, expected)
}

fn encode_stream(pcm: &[u8]) -> Vec<u8> {
    let (mut enc, header) =
        FlacEncoder::new_streaming(RATE, CHANNELS, SampleFormat::S16, 4096).expect("enc");
    let mut out = header;
    enc.encode_interleaved(pcm, &mut out).expect("encode");
    enc.finish(&mut out);
    out
}

fn encode_stream_with_tags(
    pcm: &[u8],
    comments: &[&str],
    picture: Option<(&str, &[u8])>,
) -> Vec<u8> {
    let (mut enc, mut header) =
        FlacEncoder::new_streaming(RATE, CHANNELS, SampleFormat::S16, 4096).expect("enc");
    header[4] &= 0x7f; // clear STREAMINFO's last-metadata-block flag

    let mut vc = Vec::new();
    vc.extend_from_slice(&3u32.to_le_bytes());
    vc.extend_from_slice(b"ref"); // vendor string
    vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
        vc.extend_from_slice(c.as_bytes());
    }
    header.push(if picture.is_some() { 4 } else { 0x80 | 4 });
    header.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
    header.extend_from_slice(&vc);

    if let Some((mime, data)) = picture {
        let mut pic = Vec::new();
        pic.extend_from_slice(&3u32.to_be_bytes()); // picture type: front cover
        pic.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        pic.extend_from_slice(mime.as_bytes());
        pic.extend_from_slice(&0u32.to_be_bytes()); // description length
        pic.extend_from_slice(&[0u8; 16]); // width, height, colour depth, indexed colours
        pic.extend_from_slice(&(data.len() as u32).to_be_bytes());
        pic.extend_from_slice(data);
        header.push(0x80 | 6); // last-block | PICTURE
        header.extend_from_slice(&(pic.len() as u32).to_be_bytes()[1..]);
        header.extend_from_slice(&pic);
    }

    let mut out = header;
    enc.encode_interleaved(pcm, &mut out).expect("encode");
    enc.finish(&mut out);
    out
}

fn samples_from_bytes(bytes: &[u8]) -> Vec<i64> {
    bytes.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes([c[0], c[1]]) as i64).collect()
}

#[test]
fn flacdec_announces_format_and_decodes_pcm() {
    let frames = 4_000; // several FLAC frames (block size 4096, a couple of frames)
    let (pcm, expected) = gen_s16(frames);
    let flac = encode_stream(&pcm);

    let mut h = Harness::new(pf_libflac::FlacDec::new());

    let mut decoded_bytes: Vec<u8> = Vec::new();
    for chunk in flac.chunks(700) {
        let buf = h.alloc(chunk);
        h.push(buf).expect("flacdec process");
        while let Some(out) = h.pull(pf_libflac::FlacDec::OUT) {
            decoded_bytes.extend_from_slice(out.memory.data());
        }
    }

    let ann = h.announced().expect("flacdec announced its audio/raw format");
    let rate_id = h.vocabulary().field_id("rate").expect("rate interned from offers");
    let channels_id = h.vocabulary().field_id("channels").expect("channels interned");
    let sample_id = h.vocabulary().field_id("sample").expect("sample interned");
    assert_eq!(h.vocabulary().family_name(ann.family), Some("audio/raw"));
    assert_eq!(ann.get(rate_id), Some(Value::Int(RATE as i64)), "announced rate");
    assert_eq!(ann.get(channels_id), Some(Value::Int(CHANNELS as i64)), "announced channels");
    match ann.get(sample_id) {
        Some(Value::Id(v)) => assert_eq!(h.vocabulary().value_name(v), Some("s16")),
        other => panic!("expected a categorical sample format, got {other:?}"),
    }

    for buf in h.eos().expect("eos flush") {
        decoded_bytes.extend_from_slice(buf.memory.data());
    }

    let got = samples_from_bytes(&decoded_bytes);
    assert_eq!(got.len(), expected.len(), "decoded sample count matches the input");
    assert_eq!(got, expected, "decoded PCM is lossless and correctly ordered");

    let oracle = FlacDecoder::decode(&flac).expect("standalone decode");
    assert_eq!(oracle.info.sample_rate, RATE);
    assert_eq!(oracle.info.channels, CHANNELS);
    assert_eq!(got, oracle.samples, "harness path matches the standalone decoder");
}

#[test]
fn flacdec_posts_tags_to_the_bus() {
    use profluens_core::bus::BusMessage;

    let (pcm, _) = gen_s16(4_000);
    let art = b"\xFF\xD8\xFF\xE0jpeg-ish cover-art bytes";
    let flac = encode_stream_with_tags(
        &pcm,
        &["TITLE=Set Theory", "ARTIST=Carbon Based Lifeforms"],
        Some(("image/jpeg", art)),
    );

    let mut h = Harness::new(pf_libflac::FlacDec::new());
    for chunk in flac.chunks(700) {
        let buf = h.alloc(chunk);
        h.push(buf).expect("flacdec process");
        while h.pull(pf_libflac::FlacDec::OUT).is_some() {} // drain decoded PCM
    }
    for _ in h.eos().expect("eos flush") {}

    let tag_lists: Vec<_> = h
        .bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            BusMessage::Tags { tags, .. } => Some(tags),
            _ => None,
        })
        .collect();
    assert_eq!(tag_lists.len(), 1, "tags posted exactly once");
    let tags = &tag_lists[0];
    assert_eq!(tags.get("TITLE"), Some("Set Theory"));
    assert_eq!(tags.get("ARTIST"), Some("Carbon Based Lifeforms"));
    assert_eq!(tags.get("artist"), Some("Carbon Based Lifeforms"), "lookup is case-insensitive");

    assert_eq!(tags.pictures().len(), 1, "one attached picture");
    assert_eq!(&*tags.pictures()[0].mime, "image/jpeg");
    assert_eq!(tags.pictures()[0].data.data(), art, "image bytes forwarded intact");
}

// Test code, the output on the heap.
#[allow(clippy::disallowed_methods)]
fn run_flacdec(bytes: &[u8], chunks: &[usize]) -> (Vec<u8>, Option<String>) {
    let mut h = Harness::new(pf_libflac::FlacDec::new());
    let mut out = Vec::new();
    let mut rest = bytes;
    for &n in chunks.iter().cycle() {
        if rest.is_empty() {
            break;
        }
        let (chunk, tail) = rest.split_at(n.min(rest.len()));
        rest = tail;
        let buf = h.alloc(chunk);
        h.push(buf).expect("flacdec process");
        while let Some(b) = h.pull(pf_libflac::FlacDec::OUT) {
            out.extend_from_slice(b.memory.data());
        }
    }
    for b in h.eos().expect("eos flush") {
        out.extend_from_slice(b.memory.data());
    }
    let sample = h.vocabulary().field_id("sample");
    let name = match (h.announced(), sample) {
        (Some(ann), Some(id)) => match ann.get(id) {
            Some(Value::Id(v)) => h.vocabulary().value_name(v).map(str::to_string),
            _ => None,
        },
        _ => None,
    };
    (out, name)
}

#[test]
fn flacdec_skips_an_id3v2_tag_split_across_pushes() {
    let (pcm, expected) = gen_s16(4_000);
    let flac = encode_stream(&pcm);
    for footer in [false, true] {
        let tag = support::id3_tag(3_000, footer);
        let tagged = [&tag[..], &flac[..]].concat();
        for chunks in [&[7][..], &[1, 9, 2, 3_001], &[700], &[tagged.len()]] {
            let (out, name) = run_flacdec(&tagged, chunks);
            assert_eq!(name.as_deref(), Some("s16"), "chunks {chunks:?}");
            assert_eq!(samples_from_bytes(&out), expected, "chunks {chunks:?}, footer {footer}");
        }
    }
}

#[test]
fn odd_depths_go_out_in_the_next_container_at_full_scale() {
    for bits in [4u32, 7, 8, 12, 15, 17, 20, 23, 24, 28, 31, 32] {
        let (lo, hi) = (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
        let samples: Vec<i64> =
            [lo, hi, 0, 1, -1, lo + 1, hi - 1].iter().copied().cycle().take(32).collect();
        let stream = support::verbatim_stream(bits, &samples);
        let dec = FlacDecoder::decode(&stream).expect("a valid stream");
        assert_eq!(
            (dec.info.bits_per_sample, &dec.samples),
            (bits, &samples),
            "the decoder keeps the depth"
        );
        let (out, name) = run_flacdec(&stream, &[5]);
        let bytes = (bits as usize).div_ceil(8);
        let want = ["s8", "s16", "s24", "s32"][bytes - 1];
        assert_eq!(name.as_deref(), Some(want), "{bits} bits");
        assert_eq!(out.len(), samples.len() * bytes, "{bits} bits as {want}");
        let shift = bytes as u32 * 8 - bits;
        for (k, (got, &s)) in out.chunks_exact(bytes).zip(&samples).enumerate() {
            let mut v = [0u8; 8];
            v[..bytes].copy_from_slice(got);
            let v = i64::from_le_bytes(v) << (64 - bytes * 8) >> (64 - bytes * 8);
            assert_eq!(v, s << shift, "{bits} bits, sample {k}");
        }
    }
}
