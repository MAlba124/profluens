//! The audio elements against the system ffmpeg's decode. Oracle and element FFmpeg
//! versions differ, so a float may round one step apart.

// Test code.
#![allow(clippy::disallowed_methods)]

mod support;

use pf_harness::Harness;
use pf_libav::LibavAudioDec;
use profluens_core::bus::BusMessage;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::time::Timestamp;

const SLOT: usize = 64 * 1024;

fn announced(h: &Harness, field: &str) -> Option<i64> {
    let f = h.announced()?;
    match f.get(h.vocabulary().field_id(field)?)? {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

fn announced_sample(h: &Harness) -> String {
    let f = h.announced().expect("announced");
    let v = h.vocabulary();
    let Some(Value::Id(id)) = f.get(v.field_id("sample").expect("sample")) else {
        panic!("sample")
    };
    v.value_name(id).expect("named").to_string()
}

fn taking(element: LibavAudioDec, names: &[&'static str]) -> Harness {
    let mut h = Harness::with_slot_size(element, SLOT);
    let values: Vec<ValueDesc> = names.iter().map(|n| ValueDesc::Id(n)).collect();
    h.fix_src_takes(LibavAudioDec::OUT, "sample", &values);
    h
}

fn warnings(h: &mut Harness) -> usize {
    h.bus_messages().into_iter().filter(|m| matches!(m, BusMessage::Warning { .. })).count()
}

fn decode(h: &mut Harness, chunks: &[Vec<u8>]) -> (Vec<u8>, Vec<Timestamp>) {
    let (mut pcm, mut stamps) = (Vec::new(), Vec::new());
    let mut take = |out: Vec<profluens_core::buffer::Buffer>| {
        for b in out {
            stamps.push(b.pts);
            pcm.extend_from_slice(b.memory.data());
        }
    };
    for (i, c) in chunks.iter().enumerate() {
        let mut buf = h.alloc(c);
        buf.pts = if i == 0 { Timestamp::ZERO } else { Timestamp::NONE };
        h.push(buf).expect("push");
        take(h.drain_outputs());
    }
    take(h.eos().expect("eos"));
    (pcm, stamps)
}

fn close(name: &str, got: &[u8], want: &[u8]) {
    assert_eq!(got.len(), want.len(), "{name}: length");
    fn s(b: &[u8]) -> impl Iterator<Item = i16> + '_ {
        b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c))
    }
    let (mut exact, mut worst) = (0usize, 0i32);
    for (a, b) in s(got).zip(s(want)) {
        let d = (i32::from(a) - i32::from(b)).abs();
        exact += usize::from(d == 0);
        worst = worst.max(d);
    }
    eprintln!("{name}: {exact} of {} samples exact, worst {worst}", got.len() / 2);
    assert!(worst <= 1, "{name}: a sample {worst} steps off ffmpeg");
}

fn chunks(bytes: &[u8], n: usize) -> Vec<Vec<u8>> {
    bytes.chunks(n).map(<[u8]>::to_vec).collect()
}

fn stream(name: &str, channels: u32, args: &[&str]) -> Option<Vec<u8>> {
    let ch = channels.to_string();
    let mut all = vec![
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=2",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=997:sample_rate=48000:duration=2",
        "-filter_complex",
        "[0][1]amix=inputs=2,volume=4",
        "-ac",
        &ch,
    ];
    all.extend_from_slice(args);
    support::make(name, &all)
}

fn oracle(name: &str) -> Option<Vec<u8>> {
    support::oracle(name, &format!("{name}.s16"), &["-f", "s16le", "-c:a", "pcm_s16le"])
}

#[test]
fn aac_adts_bytes_match_ffmpeg() {
    let Some(bytes) = stream("lc.aac", 2, &["-c:a", "aac", "-b:a", "128k", "-f", "adts"]) else {
        return;
    };
    let Some(want) = oracle("lc.aac") else { return };
    let mut h = Harness::with_slot_size(LibavAudioDec::aac(), SLOT);
    let (got, _) = decode(&mut h, &chunks(&bytes, 777));
    close("aac adts", &got, &want);
    assert_eq!((announced(&h, "rate"), announced(&h, "channels")), (Some(48_000), Some(2)));
    assert_eq!(warnings(&mut h), 0);
}

#[test]
fn an_id3_tag_ahead_of_adts_is_stepped_over() {
    let Some(bytes) = stream("lc_tag.aac", 2, &["-c:a", "aac", "-b:a", "64k", "-f", "adts"]) else {
        return;
    };
    let Some(want) = oracle("lc_tag.aac") else {
        return;
    };
    let mut tagged = b"ID3\x03\x00\x00\x00\x00\x01\x00".to_vec();
    tagged.extend(std::iter::repeat_n(0xFFu8, 128));
    tagged.extend_from_slice(&bytes);
    let mut h = Harness::with_slot_size(LibavAudioDec::aac(), SLOT);
    let (got, _) = decode(&mut h, &chunks(&tagged, 5));
    close("aac behind id3", &got, &want);
}

#[test]
fn raw_aac_units_behind_their_config_decode_as_the_adts_stream() {
    let Some(bytes) = stream("lc_raw.aac", 2, &["-c:a", "aac", "-b:a", "96k", "-f", "adts"]) else {
        return;
    };
    let Some(want) = oracle("lc_raw.aac") else {
        return;
    };
    let (asc, units) = support::adts_to_raw(&bytes);
    let mut h = Harness::with_slot_size(LibavAudioDec::aac(), SLOT);
    let mut input = vec![asc.clone()];
    input.extend(units[..10].iter().cloned());
    input.push(asc);
    input.extend(units[10..].iter().cloned());
    let (got, stamps) = decode(&mut h, &input);
    close("aac raw", &got, &want);
    assert_eq!(stamps.first(), Some(&Timestamp::ZERO));
    assert_eq!(warnings(&mut h), 0, "the repeated config is no unit");
}

/// The cookie then the samples of an MP4's first track.
fn mp4_units(file: &[u8]) -> Vec<Vec<u8>> {
    let r = pf_mp4::reader::Mp4Reader::new(file).expect("mp4");
    let t = r.tables();
    let mut out = vec![t.tracks()[0].codec_head().to_vec()];
    out.extend(t.samples().iter().map(|s| file[s.offset as usize..][..s.size as usize].to_vec()));
    out
}

#[test]
fn alac_16_bit_stereo_is_bit_exact() {
    let args = &["-c:a", "alac", "-sample_fmt", "s16p", "-movflags", "+faststart", "-f", "mp4"];
    let Some(bytes) = stream("alac16.m4a", 2, args) else {
        return;
    };
    let Some(want) = oracle("alac16.m4a") else {
        return;
    };
    let units = mp4_units(&bytes);
    assert_eq!(units[0].len(), 24, "ALACSpecificConfig");
    let mut h = Harness::with_slot_size(LibavAudioDec::alac(), SLOT);
    let (got, stamps) = decode(&mut h, &units);
    assert!(got == want, "alac16 differs from ffmpeg, {} and {} bytes", got.len(), want.len());
    assert_eq!(stamps.first(), Some(&Timestamp::ZERO));
    assert_eq!(warnings(&mut h), 0);
}

#[test]
fn alac_24_bit_5_1_goes_out_as_s32_bit_exact() {
    let args = &["-c:a", "alac", "-sample_fmt", "s32p", "-movflags", "+faststart", "-f", "mp4"];
    let Some(bytes) = stream("alac24.m4a", 6, args) else {
        return;
    };
    let Some(want) =
        support::oracle("alac24.m4a", "alac24.m4a.s32", &["-f", "s32le", "-c:a", "pcm_s32le"])
    else {
        return;
    };
    let mut h = taking(LibavAudioDec::alac(), &["s32"]);
    let (got, _) = decode(&mut h, &mp4_units(&bytes));
    assert_eq!(announced(&h, "channels"), Some(6));
    assert_eq!(announced_sample(&h), "s32");
    assert!(got == want, "alac24 differs from ffmpeg, {} and {} bytes", got.len(), want.len());
}

#[test]
fn ac3_stereo_and_5_1_match_ffmpeg() {
    for (name, ch) in [("st.ac3", 2u32), ("51.ac3", 6)] {
        let Some(bytes) = stream(name, ch, &["-c:a", "ac3", "-b:a", "384k", "-f", "ac3"]) else {
            return;
        };
        let Some(want) = oracle(name) else { return };
        let mut h = Harness::with_slot_size(LibavAudioDec::ac3(), SLOT);
        let (got, _) = decode(&mut h, &chunks(&bytes, 1500));
        close(name, &got, &want);
        assert_eq!(announced(&h, "channels"), Some(i64::from(ch)));
        let layout = announced(&h, "layout").expect("layout") as u64;
        assert_eq!(layout.count_ones(), ch, "{name}: {layout:#x}");
        let fronts = if ch == 2 { 0x3 } else { 0xF };
        assert_eq!(layout & fronts, fronts, "{name}: {layout:#x}");
    }
}

/// 20 stereo 96 kbit/s syncframes as an AVI carries them, one whole frame a unit, from
/// fixtures avi_mpeg4_ac3.avi. Each frame is 1536 samples, decoded without ffmpeg around.
#[test]
fn whole_ac3_frames_a_unit_decode() {
    let bytes = include_bytes!("fixtures/st96k_20.ac3");
    let mut h = Harness::with_slot_size(LibavAudioDec::ac3(), SLOT);
    let (got, _) = decode(&mut h, &chunks(bytes, 384));
    assert_eq!(announced(&h, "channels"), Some(2));
    assert_eq!(announced(&h, "rate"), Some(48_000));
    let sample = if announced_sample(&h) == "f32" { 4 } else { 2 };
    let frames = got.len() / (2 * sample);
    assert!(frames >= 19 * 1536, "{frames} sample frames of 20 syncframes");
}

#[test]
fn eac3_5_1_decodes_in_the_ac3_element() {
    let Some(bytes) = stream("51.eac3", 6, &["-c:a", "eac3", "-b:a", "256k", "-f", "eac3"]) else {
        return;
    };
    let Some(want) = oracle("51.eac3") else {
        return;
    };
    let mut h = Harness::with_slot_size(LibavAudioDec::ac3(), SLOT);
    let (got, _) = decode(&mut h, &chunks(&bytes, 4096));
    close("eac3 5.1", &got, &want);
    assert_eq!(announced(&h, "channels"), Some(6));
}

/// Layer II goes through the mp3 element, FFmpeg's mp3float decodes every layer
/// (mpegaudiodec_template.c mp_decode_frame), against ffmpeg's own mp2float.
#[test]
fn mp2_decodes_in_the_mp3_element_as_ffmpeg_mp2float() {
    let args = ["-c:a", "mp2", "-b:a", "192k", "-f", "mp2"];
    let Some(bytes) = stream("tone.mp2", 2, &args) else {
        return;
    };
    let (src, out) = (support::tmp("tone.mp2"), support::tmp("tone.mp2.s16"));
    let (src, out) = (src.to_str().expect("utf-8"), out.to_str().expect("utf-8"));
    if !support::ffmpeg(&["-c:a", "mp2float", "-i", src, "-f", "s16le", "-c:a", "pcm_s16le", out]) {
        return support::skip("ffmpeg could not decode tone.mp2");
    }
    let want = std::fs::read(out).expect("the oracle");
    let mut h = Harness::with_slot_size(LibavAudioDec::mp3(), SLOT);
    let (got, _) = decode(&mut h, &chunks(&bytes, 1000));
    assert!(got == want, "mp2: differs from ffmpeg's mp2float");
    assert_eq!((announced(&h, "rate"), announced(&h, "channels")), (Some(48_000), Some(2)));
    assert_eq!(warnings(&mut h), 0);
}

/// MPEG-1 Layer I, stereo, 48 kHz, 384 kbit/s, 384 byte frames. ffmpeg has no Layer I
/// encoder, so subbands 0 to 4 carry 8 bit samples of a slow sine in each channel.
fn layer1(frames: usize) -> Vec<u8> {
    const LEN: usize = 384;
    const BANDS: usize = 5;
    let mut out = vec![0u8; frames * LEN];
    for (f, frame) in out.as_chunks_mut::<LEN>().0.iter_mut().enumerate() {
        frame[..4].copy_from_slice(&[0xFF, 0xFF, 0xC4, 0x00]);
        let mut at = 32usize;
        let mut put = |v: u32, n: usize| {
            for b in (0..n).rev() {
                frame[at / 8] |= (((v >> b) & 1) as u8) << (7 - at % 8);
                at += 1;
            }
        };
        for sb in 0..32 {
            for _ch in 0..2 {
                put(if sb < BANDS { 7 } else { 0 }, 4);
            }
        }
        for _ in 0..BANDS * 2 {
            put(8, 6);
        }
        for j in 0..12 {
            for sb in 0..BANDS {
                for ch in 0..2 {
                    let t = (f * 12 + j) as f64 * 0.05 * (sb + ch + 1) as f64;
                    put((128.0 + 90.0 * t.sin()) as u32, 8);
                }
            }
        }
    }
    out
}

/// Layer I goes through the mp3 element as well, against ffmpeg's own mp1float.
#[test]
fn mp1_decodes_in_the_mp3_element_as_ffmpeg_mp1float() {
    let bytes = layer1(40);
    let (src, out) = (support::tmp("tone.mp1"), support::tmp("tone.mp1.s16"));
    std::fs::write(&src, &bytes).expect("write");
    let (src, out) = (src.to_str().expect("utf-8"), out.to_str().expect("utf-8"));
    let args = ["-f", "mp3", "-c:a", "mp1float", "-i", src, "-f", "s16le", "-c:a", "pcm_s16le"];
    if !support::ffmpeg(&[args.as_slice(), &[out]].concat()) {
        return support::skip("ffmpeg could not decode tone.mp1");
    }
    let want = std::fs::read(out).expect("the oracle");
    let mut h = Harness::with_slot_size(LibavAudioDec::mp3(), SLOT);
    let (got, _) = decode(&mut h, &chunks(&bytes, 500));
    assert_eq!(got.len(), 40 * 384 * 2 * 2, "384 stereo s16 samples a frame");
    close("mp1", &got, &want);
    assert!(got.iter().any(|&b| b != 0), "not silence");
    assert_eq!((announced(&h, "rate"), announced(&h, "channels")), (Some(48_000), Some(2)));
    assert_eq!(warnings(&mut h), 0);
}

/// A stream turning from Layer II to Layer I decodes on, every frame of both comes out.
#[test]
fn a_layer_change_midstream_decodes_on() {
    let args = ["-c:a", "mp2", "-b:a", "192k", "-f", "mp2"];
    let Some(mp2) = stream("tone.mp2", 2, &args) else {
        return;
    };
    let mut h = Harness::with_slot_size(LibavAudioDec::mp3(), SLOT);
    let (alone, _) = decode(&mut h, &chunks(&mp2, 1000));
    let mut h = Harness::with_slot_size(LibavAudioDec::mp3(), SLOT);
    let both = [mp2.as_slice(), &layer1(20), &mp2[..576 * 10]].concat();
    let (got, _) = decode(&mut h, &chunks(&both, 1000));
    assert!(got.starts_with(&alone), "the Layer II part as decoded alone");
    let tail = (20 * 384 + 10 * 1152) * 2 * 2;
    assert_eq!(got.len(), alone.len() + tail, "every Layer I and II frame behind the change");
    assert_eq!(announced(&h, "channels"), Some(2));
}

#[test]
fn mp3_behind_an_id3_tag_matches_ffmpeg() {
    let args = [
        "-c:a",
        "libmp3lame",
        "-b:a",
        "128k",
        "-write_xing",
        "0",
        "-id3v2_version",
        "3",
        "-metadata",
        "title=a title long enough to cross a chunk boundary of the test feed",
        "-f",
        "mp3",
    ];
    let Some(bytes) = stream("tag.mp3", 2, &args) else {
        return;
    };
    assert_eq!(&bytes[..3], b"ID3");
    let Some(want) = oracle("tag.mp3") else {
        return;
    };
    let mut h = Harness::with_slot_size(LibavAudioDec::mp3(), SLOT);
    let (got, _) = decode(&mut h, &chunks(&bytes, 37));
    close("mp3", &got, &want);
    assert_eq!(warnings(&mut h), 0);
}

#[test]
fn dts_5_1_matches_ffmpeg() {
    let args = ["-strict", "-2", "-c:a", "dca", "-b:a", "768k", "-f", "dts"];
    let Some(bytes) = stream("51.dts", 6, &args) else {
        return;
    };
    let Some(want) = oracle("51.dts") else { return };
    let mut h = Harness::with_slot_size(LibavAudioDec::dts(), SLOT);
    let (got, _) = decode(&mut h, &chunks(&bytes, 1000));
    close("dts 5.1", &got, &want);
    assert_eq!((announced(&h, "rate"), announced(&h, "channels")), (Some(48_000), Some(6)));
    assert_eq!(warnings(&mut h), 0);
}

#[test]
fn truehd_and_mlp_match_ffmpeg() {
    let cases = [
        ("51.thd", "truehd", 6u32, LibavAudioDec::truehd()),
        ("st.mlp", "mlp", 2, LibavAudioDec::mlp()),
    ];
    for (name, codec, ch, element) in cases {
        let args = ["-strict", "-2", "-c:a", codec, "-f", codec];
        let Some(bytes) = stream(name, ch, &args) else {
            return;
        };
        let Some(want) = oracle(name) else { return };
        let mut h = Harness::with_slot_size(element, SLOT);
        let (got, _) = decode(&mut h, &chunks(&bytes, 999));
        close(name, &got, &want);
        assert_eq!(announced(&h, "channels"), Some(i64::from(ch)), "{name}");
        assert_eq!(warnings(&mut h), 0, "{name}");
    }
}

#[test]
fn a_flush_restarts_the_count_from_the_next_stamp() {
    let Some(bytes) = stream("seek.ac3", 2, &["-c:a", "ac3", "-b:a", "192k", "-f", "ac3"]) else {
        return;
    };
    let mut h = Harness::with_slot_size(LibavAudioDec::ac3(), SLOT);
    let first = h.alloc(&bytes[..bytes.len() / 2]);
    h.push(first).expect("push");
    let _ = h.drain_outputs();
    h.seek(0, Timestamp::from_nanos(1_000_000_000)).expect("seek");
    let _ = h.drain_outputs();
    // AC-3 frames here are 768 bytes, 32 ms at 48 kHz. Frame 31 is at 992 ms.
    let mut buf = h.alloc(&bytes[31 * 768..]);
    buf.pts = Timestamp::from_nanos(992_000_000);
    h.push(buf).expect("push");
    let out = h.drain_outputs();
    assert_eq!(out.first().map(|b| b.pts), Some(Timestamp::from_nanos(1_000_000_000)));
}

static BYTES: [profluens_core::format::OfferDesc; 1] =
    [profluens_core::format::OfferDesc::any("bytes")];
static BYTE_PADS: [profluens_core::element::PadDesc; 1] =
    [profluens_core::element::PadDesc { name: "pad", offers: &BYTES }];
static CHUNKS: profluens_core::element::ElementDesc = profluens_core::element::ElementDesc {
    name: "chunksrc",
    sinks: &[],
    srcs: &BYTE_PADS,
    props: &[],
    sched: profluens_core::element::SchedHint::Active,
    inputs: profluens_core::element::InputPolicy::None,
    latency: profluens_core::element::LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
static DROPS: profluens_core::element::ElementDesc = profluens_core::element::ElementDesc {
    name: "dropsink",
    sinks: &BYTE_PADS,
    srcs: &[],
    props: &[],
    sched: profluens_core::element::SchedHint::Active,
    inputs: profluens_core::element::InputPolicy::Single,
    latency: profluens_core::element::LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct Chunks(Vec<Vec<u8>>, usize);
struct Drops;

impl profluens_core::element::Element for Chunks {
    fn desc(&self) -> &'static profluens_core::element::ElementDesc {
        &CHUNKS
    }
    fn process(
        &mut self,
        ctx: &mut profluens_core::ctx::Ctx,
        _inputs: profluens_core::batch::Inputs<'_>,
    ) -> Result<profluens_core::element::Flow, profluens_core::error::Error> {
        let Some(chunk) = self.0.get(self.1) else {
            return Ok(profluens_core::element::Flow::Eos);
        };
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(profluens_core::element::Flow::Ok);
        };
        buf.memory.as_mut_full()[..chunk.len()].copy_from_slice(chunk);
        buf.memory.set_len(chunk.len());
        ctx.out(profluens_core::id::SrcPad(0)).push(buf);
        self.1 += 1;
        Ok(profluens_core::element::Flow::Ok)
    }
}

impl profluens_core::element::Element for Drops {
    fn desc(&self) -> &'static profluens_core::element::ElementDesc {
        &DROPS
    }
    fn process(
        &mut self,
        _ctx: &mut profluens_core::ctx::Ctx,
        mut inputs: profluens_core::batch::Inputs<'_>,
    ) -> Result<profluens_core::element::Flow, profluens_core::error::Error> {
        while inputs.pop().is_some() {}
        Ok(profluens_core::element::Flow::Ok)
    }
}

#[test]
fn an_mp3_read_from_a_file_publishes_its_index() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../audio/mp3/tests/fixtures/mono_128_id3.mp3");
    let file = std::fs::read(path).expect("the fixture");
    let len = file.len() as u64;
    let mut p = profluens_core::pipeline::Pipeline::new();
    let src = p.add(Chunks(chunks(&file, 100), 0));
    let dec = p.add(LibavAudioDec::mp3().indexing(Some(len)));
    let sink = p.add(Drops);
    p.link(src.src(profluens_core::id::SrcPad(0)), dec.sink(LibavAudioDec::IN)).expect("src ! dec");
    p.link(dec.src(LibavAudioDec::OUT), sink.sink(profluens_core::id::SinkPad(0)))
        .expect("dec ! sink");
    p.run().expect("run");
    let index = p.index(dec).expect("published");
    assert_eq!(index.duration, Timestamp::from_nanos(350_000_000));
    assert!(index.complete && index.entries.len() == 1, "{index:?}");
    assert_eq!((index.entries[0].time_ns, index.entries[0].byte), (0, 111));
    let mp3dec =
        pf_mp3::index::index_of(&file[111..111 + 8192.min(file.len() - 111)], 111, Some(len));
    assert_eq!(*index, mp3dec, "what mp3dec publishes");
}

/// The first frames behind a seek reach into a bit reservoir the decoder was not given
/// (ISO/IEC 11172-3 2.4.2.7) and stand in as silence.
#[test]
fn an_mp3_decodes_on_behind_a_seek() {
    let args = ["-c:a", "libmp3lame", "-b:a", "128k", "-f", "mp3"];
    let Some(bytes) = stream("reservoir.mp3", 2, &args) else {
        return;
    };
    let mut h = Harness::with_slot_size(LibavAudioDec::mp3(), SLOT);
    let first = h.alloc(&bytes[..bytes.len() / 2]);
    h.push(first).expect("push");
    let _ = h.drain_outputs();
    let _ = h.bus_messages();
    h.seek(0, Timestamp::from_nanos(1_000_000_000)).expect("seek");
    let _ = h.drain_outputs();
    let from = bytes.len() / 2 + 101;
    let mut out = 0usize;
    for (i, c) in bytes[from..].chunks(4096).enumerate() {
        let mut buf = h.alloc(c);
        buf.pts = if i == 0 { Timestamp::from_nanos(1_000_000_000) } else { Timestamp::NONE };
        h.push(buf).expect("push");
        out += h.drain_outputs().iter().map(|b| b.memory.len()).sum::<usize>();
    }
    out += h.eos().expect("eos").iter().map(|b| b.memory.len()).sum::<usize>();
    assert!(out > 150_000, "{out} bytes behind the seek");
    assert!(warnings(&mut h) <= 3, "a few frames fail, not the stream");
}

#[test]
fn ac3_goes_out_in_float_where_the_link_takes_it() {
    let Some(bytes) = stream("51f.ac3", 6, &["-c:a", "ac3", "-b:a", "384k", "-f", "ac3"]) else {
        return;
    };
    let Some(want) =
        support::oracle("51f.ac3", "51f.ac3.f32", &["-f", "f32le", "-c:a", "pcm_f32le"])
    else {
        return;
    };
    let mut h = taking(LibavAudioDec::ac3(), &["s16", "f32"]);
    let (got, _) = decode(&mut h, &chunks(&bytes, 1500));
    assert_eq!(announced_sample(&h), "f32");
    assert_eq!(got.len(), want.len(), "length");
    let f = |b: &[u8]| -> Vec<f32> {
        b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
    };
    let worst = f(&got).iter().zip(f(&want)).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(worst < 1.0 / 32768.0, "a sample {worst} off ffmpeg");
    let quiet = f(&got).iter().filter(|x| **x != 0.0 && x.abs() < 1.0 / 32768.0).count();
    assert!(quiet > 0, "samples below a 16-bit step survive");
}

#[test]
fn truehd_of_24_bits_goes_out_in_the_taken_format_nearest_its_own() {
    let args = [
        "-f",
        "lavfi",
        "-i",
        "aevalsrc=0.3*sin(440*2*PI*t)|0.2*sin(997*2*PI*t):s=48000:d=2",
        "-ac",
        "6",
        "-sample_fmt",
        "s32p",
        "-strict",
        "-2",
        "-c:a",
        "truehd",
        "-f",
        "truehd",
    ];
    let Some(bytes) = support::make("51_24.thd", &args) else {
        return;
    };
    let Some(s32) =
        support::oracle("51_24.thd", "51_24.thd.s32", &["-f", "s32le", "-c:a", "pcm_s32le"])
    else {
        return;
    };
    let Some(s16) = oracle("51_24.thd") else {
        return;
    };
    assert!(
        s32.as_chunks::<4>().0.iter().any(|c| c[1] != 0),
        "the stream carries more than 16 bits"
    );

    let mut h = taking(LibavAudioDec::truehd(), &["s16", "s32", "f32"]);
    let (got, _) = decode(&mut h, &chunks(&bytes, 999));
    assert_eq!(announced_sample(&h), "s32");
    assert!(got == s32, "bit exact");

    let mut h = taking(LibavAudioDec::truehd(), &["s16", "f32"]);
    let (got, _) = decode(&mut h, &chunks(&bytes, 999));
    assert_eq!(announced_sample(&h), "f32");
    let want: Vec<f32> = s32
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| i32::from_le_bytes(*c) as f32 / 2_147_483_648.0)
        .collect();
    let got: Vec<f32> = got.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
    assert!(got == want, "24 bits held exactly");

    let mut h = taking(LibavAudioDec::truehd(), &["s16"]);
    let (got, _) = decode(&mut h, &chunks(&bytes, 999));
    assert_eq!(announced_sample(&h), "s16");
    close("truehd 24 bits as s16", &got, &s16);
    assert_eq!(warnings(&mut h), 0);
}

/// AC-3 at 32 kHz then at 48 kHz in one stream. Positions count samples, so the change of rate
/// rescales them and the stamps run on.
#[test]
fn a_rate_change_keeps_the_stamps_running() {
    let tone = |rate: &str| format!("sine=frequency=440:sample_rate={rate}:duration=1");
    let (low, high) = (tone("32000"), tone("48000"));
    let a = support::make("ac3_32k.ac3", &["-f", "lavfi", "-i", &low, "-c:a", "ac3", "-f", "ac3"]);
    let b = support::make("ac3_48k.ac3", &["-f", "lavfi", "-i", &high, "-c:a", "ac3", "-f", "ac3"]);
    let (Some(a), Some(b)) = (a, b) else {
        return;
    };
    let mut h = Harness::with_slot_size(LibavAudioDec::ac3(), SLOT);
    let (_, stamps) = decode(&mut h, &chunks(&[a, b].concat(), 4096));
    let stamps: Vec<Timestamp> = stamps.into_iter().filter(|t| *t != Timestamp::NONE).collect();
    assert!(stamps.windows(2).all(|w| w[0] <= w[1]), "a stamp stepped back: {stamps:?}");
    let last = stamps.last().expect("stamps").nanos().expect("a stamp");
    assert!(last > 1_800_000_000, "{last} ns, two seconds of sound end near 2 s");
}

/// One syncframe a stamped unit as a transport stream carries them, frame `lost` missing, the
/// PCM laid out by its stamps on one timeline.
fn ac3_by_stamps(lost: Option<usize>) -> (Vec<u8>, Timestamp) {
    let bytes = include_bytes!("fixtures/st96k_20.ac3");
    let mut h = Harness::with_slot_size(LibavAudioDec::ac3(), SLOT);
    let mut out = Vec::new();
    for k in (0..20).filter(|k| Some(*k) != lost) {
        let mut b = h.alloc(&bytes[k * 384..(k + 1) * 384]);
        b.pts = Timestamp::from_nanos(k as u64 * 32_000_000);
        h.push(b).expect("push");
        out.extend(h.drain_outputs());
    }
    out.extend(h.eos().expect("eos"));
    let stride = 2 * if announced_sample(&h) == "f32" { 4 } else { 2 };
    let mut line = vec![0u8; 20 * 1536 * stride];
    let mut end = Timestamp::ZERO;
    for b in &out {
        let at = (u128::from(b.pts.0) * 48_000).div_ceil(1_000_000_000) as usize * stride;
        line[at..at + b.memory.len()].copy_from_slice(b.memory.data());
        end = b.pts.saturating_add(b.duration);
    }
    (line, end)
}

#[test]
fn a_frame_lost_in_transport_leaves_the_audio_after_it_at_its_stamps() {
    let (whole, whole_end) = ac3_by_stamps(None);
    let (lossy, end) = ac3_by_stamps(Some(10));
    assert_eq!((whole_end, end), (Timestamp::from_millis(640), Timestamp::from_millis(640)));
    let stride = whole.len() / (20 * 1536);
    assert_eq!(stride, 4, "s16 stereo");
    // Frame 11 overlaps the lost frame's last block, from frame 12 on the audio is the same but
    // for the decoder's dither, a step at most. A frame early it differs by thousands.
    let from = 12 * 1536 * stride;
    let s16 =
        |b: &[u8]| b.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect::<Vec<_>>();
    let (got, want) = (s16(&lossy[from..]), s16(&whole[from..]));
    let worst = got.iter().zip(&want).map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs()).max();
    assert!(worst.is_some_and(|d| d <= 2), "the audio after the lost frame moved, {worst:?} apart");
    assert!(lossy[10 * 1536 * stride..11 * 1536 * stride].iter().all(|b| *b == 0), "silence");
}
