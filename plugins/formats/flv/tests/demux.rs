//! `flvdemux` on the flv fixtures against `ffprobe`, the codecs it refuses and decoding.

#![allow(clippy::disallowed_methods)] // test fixtures

mod common;

use std::sync::{Arc, Mutex};

use common::{Format, MS, Packet, Recorded, fixture, of, packets, run};
use pf_flv::FlvDemux;
use pf_libav::{
    LibavAudioDec, LibavFlv1Dec, LibavH264Dec, LibavH265Dec, LibavVp6aDec, LibavVp6fDec,
    LibavVp9Dec,
};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::{Ctx, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

fn whole(name: &str, chunk: usize) -> common::Out {
    let data = fixture(name);
    run(&data, &data, chunk)
}

/// Every frame as ffprobe lists it, stamped at its presentation time and as long.
fn matches(name: &str, rec: &Recorded, rows: &[Packet]) {
    let frames = rec.frames();
    assert_eq!(frames.len(), rows.len(), "{name}");
    for (ours, row) in frames.iter().zip(rows) {
        assert_eq!((ours.0, ours.1), (Some(row.pts as u64 * MS), row.size), "{name} {row:?}");
        let key = ours.2 & BufferFlags::KEYFRAME.0 != 0;
        assert_eq!(key, row.key, "{name} {row:?}");
    }
}

fn format(family: &str, rate: i64, channels: i64) -> Format {
    Format {
        family: family.into(),
        rate: Some(rate),
        channels: Some(channels),
        ..Format::default()
    }
}

fn picture(family: &str) -> Format {
    Format { family: family.into(), width: Some(128), height: Some(96), ..Format::default() }
}

#[test]
fn h264_and_aac_leave_as_ffprobe_lists_them() {
    let rows = packets("h264_aac");
    let out = whole("h264_aac", 4096);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let [(vk, vc, video), (ak, ac, audio)] = &out.streams[..] else { panic!("two streams") };
    assert_eq!((*vk, *vc, *ak, *ac), (StreamKind::Video, "h264/annexb", StreamKind::Audio, "aac"));
    matches("h264_aac video", video, &of(&rows, 0));
    assert!(of(&rows, 0).iter().any(|p| p.pts != p.dts), "the fixture reorders");
    assert_eq!(video.formats, [picture("h264/annexb")]);
    assert!(video.units[0].2 & BufferFlags::HEADER.0 != 0, "the parameter sets lead");
    assert_eq!(&video.bytes[..5], [0, 0, 0, 1, 0x67], "an Annex B SPS");
    let first = video.units[0].1;
    assert_eq!(&video.bytes[first..first + 4], [0, 0, 0, 1], "frames in Annex B");

    matches("h264_aac audio", audio, &of(&rows, 1));
    assert_eq!(audio.formats, [format("aac", 44_100, 2)]);
    let config = (Some(of(&rows, 1)[0].pts as u64 * MS), 5, BufferFlags::HEADER.0);
    assert_eq!(audio.units[0], config, "the config leads, with its SBR sync extension");
    assert_eq!(&audio.bytes[..2], [0x12, 0x10], "AAC-LC, 44.1 kHz, stereo");
}

#[test]
fn hevc_in_enhanced_flv_keeps_its_composition_times() {
    let rows = packets("hevc_aac");
    let out = whole("hevc_aac", 1000);
    let [(_, vc, video), (_, ac, audio)] = &out.streams[..] else { panic!("two streams") };
    assert_eq!((*vc, *ac), ("h265/annexb", "aac"));
    assert!(of(&rows, 0).iter().any(|p| p.pts != p.dts), "the fixture reorders");
    matches("hevc video", video, &of(&rows, 0));
    matches("hevc audio", audio, &of(&rows, 1));
    assert_eq!(&video.bytes[..5], [0, 0, 0, 1, 0x40], "an Annex B VPS");
}

#[test]
fn av1_vp9_mp3_and_pcm_leave_as_ffprobe_lists_them() {
    let out = whole("av1", 4096);
    let [(_, codec, video)] = &out.streams[..] else { panic!("one stream") };
    assert_eq!(*codec, "av1");
    matches("av1", video, &packets("av1"));
    assert_eq!(video.formats, [picture("av1")]);

    let rows = packets("vp9_pcm");
    let out = whole("vp9_pcm", 4096);
    let [(_, vc, video), (_, ac, audio)] = &out.streams[..] else { panic!("two streams") };
    assert_eq!((*vc, *ac), ("vp9", "audio/raw"));
    matches("vp9", video, &of(&rows, 0));
    matches("pcm", audio, &of(&rows, 1));
    assert_eq!(audio.formats, [format("audio/raw", 11_025, 1)]);

    let rows = packets("h264_mp3");
    let out = whole("h264_mp3", 4096);
    let [(_, _, video), (_, ac, audio)] = &out.streams[..] else { panic!("two streams") };
    assert_eq!(*ac, "mp3");
    matches("h264_mp3 video", video, &of(&rows, 0));
    matches("mp3", audio, &of(&rows, 1));
    assert_eq!(audio.formats, [format("mp3", 44_100, 2)]);
    assert_eq!(&audio.bytes[..2], [0xFF, 0xFB], "an MPEG-1 layer III frame");
}

#[test]
fn sound_alone_is_one_stream() {
    let out = whole("aac", 777);
    let [(kind, codec, audio)] = &out.streams[..] else { panic!("one stream") };
    assert_eq!((*kind, *codec), (StreamKind::Audio, "aac"));
    matches("aac", audio, &packets("aac"));
}

#[test]
fn how_the_input_is_cut_changes_nothing() {
    for name in ["h264_aac", "hevc_aac", "vp9_pcm"] {
        let reference = whole(name, 1 << 20).streams;
        for chunk in [1usize, 7, 188, 4096] {
            assert_eq!(whole(name, chunk).streams, reference, "{name} in pieces of {chunk}");
        }
    }
}

#[test]
fn a_head_of_the_header_and_metadata_alone_finds_the_streams() {
    let data = fixture("h264_aac");
    let head = &data[..common::tags(&data)[1].0];
    let reference = run(&data, &data, 4096).streams;
    let out = run(&data, head, 4096).streams;
    assert_eq!(out, reference, "the configs are read as the stream goes");
}

#[test]
fn legacy_video_plays_and_the_rest_is_named_and_refused() {
    let data = fixture("flv1_mp3");
    let (streams, warnings) = common::preroll(&data).expect("both play");
    assert_eq!(streams, [(StreamKind::Video, "flv1"), (StreamKind::Audio, "mp3")]);
    assert!(warnings.is_empty(), "{warnings:?}");
    let out = run(&data, &data, 4096);
    matches("flv1_mp3 picture", &out.streams[0].2, &of(&packets("flv1_mp3"), 1));
    matches("flv1_mp3 sound", &out.streams[1].2, &of(&packets("flv1_mp3"), 0));
    assert_eq!(decode("flv1_mp3", [Some(Box::new(LibavFlv1Dec::new())), None]), [15]);

    let err = common::preroll(&fixture("nellymoser")).expect_err("nothing plays");
    assert!(err.contains("Nellymoser"), "{err}");

    // The first video tag rewritten to name another legacy codec.
    let at = common::tags(&data).into_iter().find(|t| t.1 == 9).expect("a video tag").0;
    for (id, name) in [(3, "Screen video"), (6, "Screen video v2")] {
        let mut other = data.clone();
        other[at + 11] = 0x10 | id;
        let (_, warnings) = common::preroll(&other).expect("the sound plays");
        assert!(warnings.iter().any(|w| w.contains(name)), "{name} {warnings:?}");
    }
    for (id, family) in [(4, "vp6f"), (5, "vp6a")] {
        let mut other = data.clone();
        other[at + 11] = 0x10 | id;
        let (streams, _) = common::preroll(&other).expect("both play");
        assert_eq!(streams[0], (StreamKind::Video, family));
    }
    assert!(common::preroll(b"FLX\x01\x05\x00\x00\x00\x09").unwrap_err().contains("FLV header"));
}

static RAW_OFFERS: [OfferDesc; 2] = [OfferDesc::any("video/raw"), OfferDesc::any("audio/raw")];
static RAW_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_OFFERS }];
static RAW_DESC: ElementDesc = ElementDesc {
    name: "rawsink",
    sinks: &RAW_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct RawSink {
    count: Arc<Mutex<(usize, usize)>>,
}

impl Element for RawSink {
    fn desc(&self) -> &'static ElementDesc {
        &RAW_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut count = self.count.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            (count.0, count.1) = (count.0 + 1, count.1 + buf.memory.len());
        }
        Ok(Flow::Ok)
    }
}

/// Buffers decoded of each stream given a decoder, the others left unselected.
fn decode(name: &str, decoders: [Option<Box<dyn Element>>; 2]) -> Vec<usize> {
    decode_bytes(fixture(name), decoders).into_iter().map(|(n, _)| n).collect()
}

/// Buffers and bytes decoded of each stream of `data`.
fn decode_bytes(data: Vec<u8>, decoders: [Option<Box<dyn Element>>; 2]) -> Vec<(usize, usize)> {
    let mut p = Pipeline::new();
    let src = p.add(common::PieceSrc { data: data.clone(), chunk: 3000, at: 0 });
    let demux = p.add(FlvDemux::new(data.clone()));
    p.link(src.src(common::PieceSrc::OUT), demux.sink(FlvDemux::IN)).expect("src to demux");
    p.set_select_all(false);
    p.preroll().expect("preroll");
    let mut counts = Vec::new();
    for (n, dec) in decoders.into_iter().enumerate() {
        let Some(dec) = dec else { continue };
        let pad = p.select(demux, n as u32).expect("select");
        let count = Arc::new(Mutex::new((0, 0)));
        let dec = p.add_boxed(dec);
        let sink = p.add(RawSink { count: Arc::clone(&count) });
        p.link(demux.src(pad), dec.sink(SinkPad(0))).expect("demux to decoder");
        p.link(dec.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("decoder to sink");
        counts.push(count);
    }
    p.run().expect("the run ends");
    counts.iter().map(|c| *c.lock().unwrap()).collect()
}

/// A local file named by `PF_FLV_FILE`, VP6 or Sorenson video and MP3, decodes whole.
#[test]
fn a_local_vp6_or_sorenson_file_decodes() {
    let Some(path) = std::env::var_os("PF_FLV_FILE") else { return };
    let data = std::fs::read(&path).expect("read PF_FLV_FILE");
    let (streams, warnings) = common::preroll(&data).expect("preroll");
    eprintln!("{streams:?} {warnings:?}");
    let video: Box<dyn Element> = match streams.iter().find(|s| s.0 == StreamKind::Video) {
        Some((_, "flv1")) => Box::new(LibavFlv1Dec::new()),
        Some((_, "vp6a")) => Box::new(LibavVp6aDec::new()),
        _ => Box::new(LibavVp6fDec::new()),
    };
    let (pictures, bytes) = decode_bytes(data.clone(), [Some(video), None])[0];
    // The MP3 decoder is passive, so it runs alone.
    let (sound, _) = decode_bytes(data, [None, Some(Box::new(LibavAudioDec::mp3()))])[0];
    eprintln!("{pictures} pictures of {} bytes, {sound} buffers of sound", bytes / pictures.max(1));
    assert!(pictures > 0 && sound > 0);
}

#[test]
fn the_streams_decode() {
    let aac = || Some(Box::new(LibavAudioDec::aac()) as Box<dyn Element>);
    let got = decode("h264_aac", [Some(Box::new(LibavH264Dec::new())), aac()]);
    assert_eq!(got[0], 60, "four seconds at 15 pictures a second");
    assert!(got[1] > 100, "{} buffers of sound", got[1]);
    let got = decode("hevc_aac", [Some(Box::new(LibavH265Dec::new())), aac()]);
    assert_eq!(got[0], 30);
    // The MP3 decoder is passive, so a branch beside it is not.
    let got = decode("h264_mp3", [None, Some(Box::new(LibavAudioDec::mp3()))]);
    assert!(got[0] > 50, "{} buffers of sound", got[0]);
    assert_eq!(decode("vp9_pcm", [Some(Box::new(LibavVp9Dec::new())), None]), [15]);
}
