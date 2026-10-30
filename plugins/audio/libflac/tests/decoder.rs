// Test code, fixtures on the heap.
#![allow(clippy::disallowed_methods)]

mod support;

use pf_libflac::{DecodeError, FlacDecoder, FlacEncoder, SampleFormat, StreamDecoder};

const BLOCK: usize = 512;

fn stream(frames: usize) -> Vec<u8> {
    let (mut enc, header) =
        FlacEncoder::new_streaming(44_100, 2, SampleFormat::S16, BLOCK as u32).unwrap();
    let mut pcm = Vec::with_capacity(frames * BLOCK * 4);
    for n in 0..frames * BLOCK {
        let n = n as f64;
        pcm.extend_from_slice(&((8000.0 * (n * 0.03).sin()) as i16).to_le_bytes());
        pcm.extend_from_slice(&((6000.0 * (n * 0.047).cos()) as i16).to_le_bytes());
    }
    let mut out = header;
    enc.encode_interleaved(&pcm, &mut out).unwrap();
    enc.finish(&mut out);
    out
}

type Placed = (Vec<i32>, Option<(u64, u64)>);

fn frames(bytes: &[u8], piece: usize) -> Vec<Placed> {
    let mut d = StreamDecoder::new();
    let mut out = Vec::new();
    let mut got = Vec::new();
    for chunk in bytes.chunks(piece) {
        d.push(chunk);
        while d.pull_into(&mut out).expect("decodes").is_some() {
            got.push((out.clone(), d.last_frame()));
        }
    }
    got
}

#[test]
fn resync_after_seek_decodes_the_tail() {
    let bytes = stream(10);
    let baseline = FlacDecoder::decode(&bytes).expect("baseline decode").samples;
    assert_eq!(baseline.len(), 10 * BLOCK * 2);

    let mut dec = StreamDecoder::new();
    dec.push(&bytes);
    dec.pull_into(&mut Vec::new()).expect("first frame decodes").expect("a frame is present");
    assert!(dec.info().is_some(), "header parsed");
    dec.seek_reset();
    assert_eq!(dec.last_frame(), None);
    let off = bytes.len() / 2;
    dec.push(&bytes[off..]);
    dec.restart_at(off as u64);

    let (_, cuts) = support::split_frames(&bytes);
    let mut got = Vec::new();
    let mut out = Vec::new();
    while dec.pull_into(&mut out).expect("resync decode never errors").is_some() {
        let (first, byte) = dec.last_frame().expect("placed behind restart_at");
        let k = cuts.iter().position(|r| r.start as u64 == byte).expect("a frame start");
        assert_eq!(first, (k * BLOCK) as u64);
        got.extend(out.iter().map(|&s| i64::from(s)));
    }
    assert!(!got.is_empty(), "resynced and decoded at least one frame past the seek");
    assert_eq!(got.len() % (BLOCK * 2), 0, "resync landed on a frame boundary");
    assert_eq!(got, baseline[baseline.len() - got.len()..], "the tail is bit exact");
}

#[test]
fn frames_are_placed_at_their_bytes_whatever_the_pieces() {
    let bytes = stream(6);
    let (head, cuts) = support::split_frames(&bytes);
    for piece in [1, 7, 100, 4096, bytes.len()] {
        let got = frames(&bytes, piece);
        assert_eq!(got.len(), cuts.len(), "pieces of {piece}");
        for (k, ((s, at), r)) in got.iter().zip(&cuts).enumerate() {
            assert_eq!(
                *at,
                Some(((k * BLOCK) as u64, r.start as u64)),
                "frame {k}, pieces of {piece}"
            );
            assert_eq!(s.len(), BLOCK * 2);
        }
    }
    let mut d = StreamDecoder::new();
    d.push(&bytes);
    let _ = d.pull_into(&mut Vec::new());
    assert_eq!(d.audio_start(), Some(head.end as u64));
}

#[test]
fn a_corrupt_frame_is_dropped_and_the_next_decodes() {
    let bytes = stream(8);
    let (_, cuts) = support::split_frames(&bytes);
    let clean = frames(&bytes, bytes.len());
    let mut bad = bytes.clone();
    let hit = (cuts[3].start + cuts[3].end) / 2;
    bad[hit] ^= 0x5A;
    for piece in [3, 333, bad.len()] {
        let got = frames(&bad, piece);
        let want: Vec<_> =
            clean.iter().enumerate().filter(|&(k, _)| k != 3).map(|(_, f)| f.clone()).collect();
        assert!(got == want, "pieces of {piece}: {} frames", got.len());
    }
    let mut junk = bytes[..cuts[5].start].to_vec();
    junk.extend_from_slice(&[0xFF, 0xF8, 0x00, 0x13, 0x37, 0xFF, 0xFF, 0x00]);
    junk.extend_from_slice(&bytes[cuts[5].start..]);
    let got = frames(&junk, 64);
    assert_eq!(got.len(), clean.len());
    assert_eq!(got[5].1, Some(((5 * BLOCK) as u64, cuts[5].start as u64 + 8)));
    assert!(got.iter().zip(&clean).all(|(a, b)| a.0 == b.0));
}

#[test]
fn a_truncated_tail_is_waited_for_not_an_error() {
    let bytes = stream(4);
    let (_, cuts) = support::split_frames(&bytes);
    let cut = cuts[3].start + 10;
    let mut d = StreamDecoder::new();
    d.push(&bytes[..cut]);
    let mut out = Vec::new();
    let mut n = 0;
    while d.pull_into(&mut out).expect("no error").is_some() {
        n += 1;
    }
    assert_eq!(n, 3);
    assert_eq!(d.pull_into(&mut out), Ok(None), "nothing new, nothing done");
    assert_eq!(d.buffered().len(), 10, "the partial frame is kept");
    d.push(&bytes[cut..]);
    assert_eq!(d.pull_into(&mut out), Ok(Some(())));
    assert_eq!(FlacDecoder::decode(&bytes[..cut]).err(), Some(DecodeError::UnexpectedEof));
}

#[test]
fn stream_decoder_captures_vorbis_comments() {
    let mut bytes = stream(2);
    let mut header = bytes[..42].to_vec();
    header[4] &= 0x7f;
    let mut body = Vec::new();
    body.extend_from_slice(&3u32.to_le_bytes());
    body.extend_from_slice(b"ref");
    let comments = ["TITLE=Set Theory", "ARTIST=Carbon Based Lifeforms"];
    body.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        body.extend_from_slice(&(c.len() as u32).to_le_bytes());
        body.extend_from_slice(c.as_bytes());
    }
    header.push(0x80 | 4);
    header.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    header.extend_from_slice(&body);
    header.extend_from_slice(&bytes[42..]);
    bytes = header;

    let mut dec = StreamDecoder::new();
    dec.push(&bytes);
    dec.pull_into(&mut Vec::new()).expect("decodes").expect("a frame is present");
    assert!(dec.info().is_some(), "header parsed");
    assert_eq!(
        dec.tags().comments,
        vec![
            ("TITLE".to_string(), "Set Theory".to_string()),
            ("ARTIST".to_string(), "Carbon Based Lifeforms".to_string()),
        ]
    );
}

#[test]
fn stream_info_agrees_with_the_decoder() {
    const SAMPLES: u32 = 4_000;
    let (mut enc, mut flac) = FlacEncoder::new(44_100, 2, SampleFormat::S16).unwrap();
    let pcm: Vec<u8> = (0..SAMPLES)
        .flat_map(|i| {
            let s = (i as i16).wrapping_mul(97);
            [s.to_le_bytes(), s.wrapping_neg().to_le_bytes()]
        })
        .flatten()
        .collect();
    let mut frames = Vec::new();
    enc.encode_interleaved(&pcm, &mut frames).unwrap();
    let body = enc.finish(&mut frames);
    let at = pf_libflac::streaminfo_offset();
    flac[at..at + body.len()].copy_from_slice(&body);
    flac.extend_from_slice(&frames);

    let info = pf_tags::flacmeta::stream_info(&flac).expect("STREAMINFO");
    let decoded = FlacDecoder::decode(&flac).expect("decodes");
    assert_eq!(info.sample_rate, decoded.info.sample_rate);
    assert_eq!(u32::from(info.channels), decoded.info.channels);
    assert_eq!(u32::from(info.bits_per_sample), decoded.info.bits_per_sample);
    assert_eq!(info.total_samples, decoded.info.total_samples);
    assert_eq!(
        (info.sample_rate, info.channels, info.bits_per_sample, info.total_samples),
        (44_100, 2, 16, u64::from(SAMPLES))
    );
}

#[test]
fn fixed_block_frames_are_numbered_by_frame_when_the_block_sizes_differ() {
    let mut bytes = stream(5);
    // min block 100, max 512.
    bytes[8..10].copy_from_slice(&100u16.to_be_bytes());
    let got = frames(&bytes, 999);
    assert_eq!(got.len(), 5);
    for (k, (_, at)) in got.iter().enumerate() {
        assert_eq!(at.map(|a| a.0), Some((k * BLOCK) as u64), "frame {k}");
    }
}

fn run(bytes: &[u8], cuts: &[usize]) -> (Vec<Placed>, Result<(), DecodeError>, u64) {
    let mut d = StreamDecoder::new();
    let mut out = Vec::new();
    let mut got = Vec::new();
    let mut rest = bytes;
    for &n in cuts.iter().cycle() {
        if rest.is_empty() {
            break;
        }
        let (chunk, tail) = rest.split_at(n.min(rest.len()));
        rest = tail;
        d.push(chunk);
        loop {
            match d.pull_into(&mut out) {
                Ok(Some(())) => got.push((out.clone(), d.last_frame())),
                Ok(None) => break,
                Err(e) => return (got, Err(e), d.errors()),
            }
        }
    }
    (got, Ok(()), d.errors())
}

#[test]
fn a_damaged_stream_decodes_the_same_however_it_is_cut() {
    let bytes = stream(24);
    let (_, cuts) = support::split_frames(&bytes);
    let mut bad = bytes[..cuts[2].start].to_vec();
    // A flipped bit, a frame cut in half, junk, a bare valid header, a repeated frame, a lost byte.
    let mut f2 = bytes[cuts[2].clone()].to_vec();
    let hit = f2.len() / 3;
    f2[hit] ^= 0x10;
    bad.extend_from_slice(&f2);
    bad.extend_from_slice(&bytes[cuts[3].start..(cuts[3].start + cuts[3].end) / 2]);
    bad.extend_from_slice(&[0x00, 0xFF, 0xFF, 0xF8, 0x12, 0x00, 0xFF]);
    bad.extend_from_slice(&bytes[cuts[6].start..cuts[6].start + 6]);
    bad.extend_from_slice(&[0x5A; 40]);
    bad.extend_from_slice(&bytes[cuts[4].clone()]);
    bad.extend_from_slice(&bytes[cuts[4].clone()]);
    bad.extend_from_slice(&bytes[cuts[5].start + 1..cuts[5].end]);
    let from = bad.len();
    bad.extend_from_slice(&bytes[cuts[6].start..]);
    for k in (from + 17..bad.len()).step_by(997) {
        bad[k] ^= 1 << (k % 8);
    }
    let whole = run(&bad, &[bad.len()]);
    assert!(whole.2 > 0 && whole.0.len() > 10, "{} frames, {} errors", whole.0.len(), whole.2);
    assert_eq!(whole.1, Ok(()));
    for cuts in [&[1][..], &[2], &[3], &[7], &[64], &[333], &[4096], &[1, 1000, 13, 2], &[5000, 1]]
    {
        assert!(run(&bad, cuts) == whole, "cut by {cuts:?}");
    }
    let dec = FlacDecoder::decode(&bad).expect("decodes what it can");
    let samples: Vec<i64> =
        whole.0.iter().flat_map(|(s, _)| s.iter().map(|&v| i64::from(v))).collect();
    assert_eq!((dec.samples, dec.errors), (samples, whole.2));
}

#[test]
fn an_id3v2_tag_ahead_of_the_marker_moves_the_frames_by_its_length() {
    let bytes = stream(6);
    let bare = run(&bytes, &[bytes.len()]);
    for footer in [false, true] {
        let tag = support::id3_tag(70_000, footer);
        let tagged = [&tag[..], &bytes[..]].concat();
        let moved: Vec<Placed> = bare
            .0
            .iter()
            .map(|(s, at)| (s.clone(), at.map(|(f, b)| (f, b + tag.len() as u64))))
            .collect();
        for cuts in [&[tagged.len()][..], &[1], &[5, 6], &[9, 1, 70_000], &[4096]] {
            let got = run(&tagged, cuts);
            assert!(got.0 == moved, "cut by {cuts:?}");
            assert_eq!((got.1, got.2), (Ok(()), 0), "cut by {cuts:?}");
        }
        let mut d = StreamDecoder::new();
        d.push(&tagged);
        let _ = d.pull_into(&mut Vec::new());
        let start = pf_tags::flacmeta::audio_start(&bytes).expect("a header") + tag.len();
        assert_eq!(d.audio_start(), Some(start as u64));
        assert_eq!(pf_tags::flacmeta::audio_start(&tagged), Some(start));
        assert_eq!(
            FlacDecoder::decode(&tagged).expect("decodes").samples,
            FlacDecoder::decode(&bytes).unwrap().samples
        );
    }
    let tag = support::id3_tag(100, false);
    let tagged = [&tag[..], &bytes[..]].concat();
    for n in [0, 1, 3, 9, 10, 50, tag.len(), tag.len() + 3] {
        assert_eq!(
            FlacDecoder::decode(&tagged[..n]).err(),
            Some(DecodeError::UnexpectedEof),
            "cut at {n}"
        );
    }
}
