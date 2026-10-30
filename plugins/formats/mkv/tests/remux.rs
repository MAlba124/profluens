//! `mkvdemux ! mkvmux` remux in a real pipeline, the muxer configured by announcements alone.

// Tests own their fixtures and staging Vecs.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_libflac::{FlacEncoder, SampleFormat};
use pf_mkv::ebml::id;
use pf_mkv::{MatroskaReader, MatroskaWriter, MkvDemux, MkvError, MkvMux, MuxError, TrackConfig};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "bytesrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct ByteSrc {
    data: Vec<u8>,
    chunk: usize,
    pos: usize,
}

impl ByteSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for ByteSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.pos = 0;
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pos >= self.data.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = self.chunk.min(buf.memory.capacity()).min(self.data.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        ctx.out(Self::OUT).push(buf);
        self.pos += n;
        Ok(Flow::Ok)
    }
}

static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "bytecollect",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct ByteCollect {
    got: Arc<Mutex<Vec<u8>>>,
}

impl ByteCollect {
    const IN: SinkPad = SinkPad(0);
}

impl Element for ByteCollect {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut got = self.got.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            got.extend_from_slice(buf.memory.data());
        }
        Ok(Flow::Ok)
    }
}

fn make_flac(sample_rate: u32, channels: u32, n: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let (mut enc, mut header) =
        FlacEncoder::new_streaming(sample_rate, channels, SampleFormat::S16, 4096_u32).unwrap();
    let mut frames = Vec::new();
    let mut phase = 7i32;
    for _ in 0..n {
        let mut pcm = Vec::new();
        for _ in 0..4096 {
            for c in 0..channels {
                let v = ((phase.wrapping_mul(37) + c as i32 * 501) & 0x3FFF) as i16 - 0x2000;
                pcm.extend_from_slice(&v.to_le_bytes());
                phase = phase.wrapping_add(1);
            }
        }
        enc.encode_frames(&pcm, &mut frames).unwrap();
    }
    let body = enc.finish_frames(&mut frames);
    let off = pf_libflac::streaminfo_offset();
    header[off..off + body.len()].copy_from_slice(&body);
    (header, frames)
}

fn header_prefix(stream: &[u8]) -> Vec<u8> {
    let cluster =
        stream.windows(4).position(|w| w == id::CLUSTER).expect("stream has at least one Cluster");
    stream[..cluster].to_vec()
}

fn remux(stream: Vec<u8>, chunk: usize) -> Result<Vec<u8>, Error> {
    let header = header_prefix(&stream);
    let got = Arc::new(Mutex::new(Vec::new()));

    let mut p = Pipeline::new();
    let src = p.add(ByteSrc { data: stream, chunk, pos: 0 });
    let demux = p.add(MkvDemux::new(header));
    p.link(src.src(ByteSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");

    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), 1, "single-track remux: one discovered src pad");

    let mux = p.add(MkvMux::from_caps());
    let sink = p.add(ByteCollect { got: Arc::clone(&got) });
    p.link(added[0].element.src(added[0].pad), mux.sink(MkvMux::IN)).expect("demux -> mux");
    p.link(mux.src(MkvMux::OUT), sink.sink(ByteCollect::IN)).expect("mux -> sink");

    p.run()?;
    let out = got.lock().unwrap().clone();
    Ok(out)
}

fn read_all(stream: &[u8]) -> (Vec<pf_mkv::Track>, Vec<pf_mkv::Frame>) {
    let mut r = MatroskaReader::new();
    r.push(stream).expect("remuxed stream parses");
    let mut frames = Vec::new();
    while let Some(f) = r.next_frame() {
        frames.push(f);
    }
    (r.tracks().to_vec(), frames)
}

#[test]
fn flac_remux_is_bit_exact() {
    let (head, frames) = make_flac(48_000, 2, 6);

    let mut w = MatroskaWriter::new(vec![TrackConfig::flac(1, head.clone(), 48_000.0, 2, 16)]);
    let mut src = Vec::new();
    w.write_header(&mut src).unwrap();
    for (i, f) in frames.iter().enumerate() {
        w.write_frame(&mut src, 1, i as u64 * 85_000_000, f, true).unwrap();
    }
    w.finalize(&mut src);

    let out = remux(src, 900).expect("remux runs");
    let (tracks, got) = read_all(&out);

    assert_eq!(tracks.len(), 1);
    let t = &tracks[0];
    assert_eq!(t.codec_id, "A_FLAC");
    assert_eq!(t.codec_private, head, "CodecPrivate absorbed from the in-band head, verbatim");
    assert_eq!(
        (t.sampling_frequency, t.channels, t.bit_depth),
        (48_000.0, 2, 16),
        "audio params parsed from STREAMINFO (RFC 9639 §8.2)"
    );

    assert_eq!(got.len(), frames.len(), "one block per source frame");
    for (i, (g, want)) in got.iter().zip(&frames).enumerate() {
        assert_eq!(&g.data, want, "frame {i} bytes bit-exact");
        assert_eq!(g.pts_ns, i as u64 * 85_000_000, "frame {i} timestamp preserved");
        assert!(g.keyframe, "frame {i}: FLAC frames are all keyframes");
    }
}

#[test]
fn vp8_remux_preserves_dims_and_keyframes() {
    let frames: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 40 + i as usize]).collect();
    let key = [true, false, false, true, false, true, false, false];

    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240)]);
    let mut src = Vec::new();
    w.write_header(&mut src).unwrap();
    for (i, f) in frames.iter().enumerate() {
        w.write_frame(&mut src, 1, i as u64 * 20_000_000, f, key[i]).unwrap();
    }
    w.finalize(&mut src);

    let out = remux(src, 700).expect("remux runs");
    let (tracks, got) = read_all(&out);

    assert_eq!(tracks.len(), 1);
    let t = &tracks[0];
    assert_eq!(t.codec_id, "V_VP8");
    assert_eq!((t.pixel_width, t.pixel_height), (320, 240), "dims from the announcement");

    assert_eq!(got.len(), frames.len());
    for (i, (g, want)) in got.iter().zip(&frames).enumerate() {
        assert_eq!(&g.data, want, "frame {i} bytes bit-exact");
        assert_eq!(g.pts_ns, i as u64 * 20_000_000, "frame {i} timestamp preserved");
        assert_eq!(
            g.keyframe, key[i],
            "frame {i}: keyframe bit preserved through demux flags → mux"
        );
    }
}

#[test]
fn vp8_remux_preserves_colour() {
    let mut tc = TrackConfig::vp8(1, 1920, 1080);
    tc.video.as_mut().unwrap().colour = Some(pf_mkv::ColourConfig {
        matrix: 9,
        range: 1,
        transfer: 16,
        primaries: 9,
        ..Default::default()
    });
    let mut w = MatroskaWriter::new(vec![tc]);
    let mut src = Vec::new();
    w.write_header(&mut src).unwrap();
    for i in 0..4u8 {
        w.write_frame(&mut src, 1, i as u64 * 20_000_000, &[i; 32], i == 0).unwrap();
    }
    w.finalize(&mut src);

    let out = remux(src, 700).expect("remux runs");
    let (tracks, got) = read_all(&out);

    assert_eq!(tracks.len(), 1);
    let t = &tracks[0];
    assert_eq!(t.codec_id, "V_VP8");
    assert_eq!(
        (t.colour_matrix, t.colour_range, t.colour_transfer, t.colour_primaries),
        (9, 1, 16, 9),
        "Colour code points preserved through announce names → mux write-back"
    );
    assert_eq!(got.len(), 4);
}

#[test]
fn vp8_remux_preserves_hdr_static_metadata() {
    use profluens_video::color::{ContentLight, HdrStatic, Mastering};
    let hdr = HdrStatic {
        mastering: Mastering {
            primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
            white: [15635, 16450],
            max_luminance: 10_000_000,
            min_luminance: 50,
        },
        light: ContentLight { max_cll: 1000, max_fall: 400 },
    };
    let mut tc = TrackConfig::vp8(1, 3840, 2160);
    tc.video.as_mut().unwrap().colour = Some(pf_mkv::ColourConfig {
        matrix: 9,
        range: 1,
        transfer: 16,
        primaries: 9,
        hdr,
        ..Default::default()
    });
    let mut w = MatroskaWriter::new(vec![tc]);
    let mut src = Vec::new();
    w.write_header(&mut src).unwrap();
    for i in 0..4u8 {
        w.write_frame(&mut src, 1, i as u64 * 20_000_000, &[i; 32], i == 0).unwrap();
    }
    w.finalize(&mut src);
    let (tracks, _) = read_all(&src);
    assert_eq!(tracks[0].hdr, hdr, "the writer and the reader agree on the elements");

    let out = remux(src, 700).expect("remux runs");
    let (tracks, got) = read_all(&out);
    let t = &tracks[0];
    assert_eq!((t.colour_transfer, t.colour_primaries), (16, 9));
    assert_eq!(t.hdr, hdr, "mastering display and light level through the announcement");
    assert_eq!(got.len(), 4);
}

#[test]
fn unsupported_family_fails_loudly() {
    let mut w =
        MatroskaWriter::new(vec![TrackConfig::video(1, "V_MPEG4/ISO/ASP", Vec::new(), 640, 360)]);
    let mut src = Vec::new();
    w.write_header(&mut src).unwrap();
    w.write_frame(&mut src, 1, 0, &[0u8; 64], true).unwrap();
    w.finalize(&mut src);

    let err =
        remux(src, 4096).expect_err("an unmapped family must be a loud error, not a corrupt file");
    let typed = std::error::Error::source(&err).and_then(|s| s.downcast_ref::<MkvError>());
    assert_eq!(
        typed,
        Some(&MkvError::Mux(MuxError::UnsupportedFamily { family: "mpeg4/asp".into() })),
        "the unsupported family is named: {err:?}"
    );
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn h264_remux_rebuilds_the_avcc_and_the_blocks() {
    let rec = hex("0142c00affe100176742c00ad9020db0110000030001000003001e0f12264801000568cb83cb20");
    let frames: Vec<Vec<u8>> = (0..6u8)
        .map(|i| {
            let nal: Vec<u8> = std::iter::once(if i == 0 { 0x65 } else { 0x41 })
                .chain((0..30 + i).map(|k| k | 0x10))
                .collect();
            let mut b = (nal.len() as u32).to_be_bytes().to_vec();
            b.extend_from_slice(&nal);
            b
        })
        .collect();
    let mut w =
        MatroskaWriter::new(vec![TrackConfig::video(1, "V_MPEG4/ISO/AVC", rec.clone(), 128, 96)]);
    let mut src = Vec::new();
    w.write_header(&mut src).unwrap();
    for (i, f) in frames.iter().enumerate() {
        w.write_frame(&mut src, 1, i as u64 * 40_000_000, f, i == 0).unwrap();
    }
    w.finalize(&mut src);

    let out = remux(src, 700).expect("remux runs");
    let (tracks, got) = read_all(&out);
    assert_eq!(tracks[0].codec_id, "V_MPEG4/ISO/AVC");
    assert_eq!(tracks[0].codec_private, rec, "the avcC of the in-band sets");
    assert_eq!((tracks[0].pixel_width, tracks[0].pixel_height), (128, 96));
    assert_eq!(got.len(), frames.len(), "the head of sets is no block");
    for (i, (g, f)) in got.iter().zip(&frames).enumerate() {
        assert_eq!(&g.data, f, "block {i}");
        assert_eq!(g.keyframe, i == 0);
    }
}

#[test]
fn av1_remux_builds_the_av1c() {
    let av1c = hex("81000c000a0a0000000337fbe017c002");
    let mut first = vec![0x12, 0x00];
    first.extend_from_slice(&av1c[4..]);
    first.extend_from_slice(&[0x32, 0x03, 0xAA, 0xBB, 0xCC]);
    let next = vec![0x12, 0x00, 0x32, 0x02, 0x11, 0x22];
    let mut w = MatroskaWriter::new(vec![TrackConfig::video(1, "V_AV1", Vec::new(), 128, 96)]);
    let mut src = Vec::new();
    w.write_header(&mut src).unwrap();
    w.write_frame(&mut src, 1, 0, &first, true).unwrap();
    w.write_frame(&mut src, 1, 40_000_000, &next, false).unwrap();
    w.finalize(&mut src);

    let out = remux(src, 4096).expect("remux runs");
    let (tracks, got) = read_all(&out);
    assert_eq!(tracks[0].codec_id, "V_AV1");
    assert_eq!(tracks[0].codec_private, av1c);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].data, first[2..].to_vec(), "the temporal delimiter left out");
    assert_eq!(got[1].data, next[2..].to_vec());
}

#[test]
fn av1_without_a_sequence_header_fails_loudly() {
    let mut w = MatroskaWriter::new(vec![TrackConfig::video(1, "V_AV1", Vec::new(), 640, 360)]);
    let mut src = Vec::new();
    w.write_header(&mut src).unwrap();
    w.write_frame(&mut src, 1, 0, &[0x32, 0x01, 0x00], true).unwrap();
    w.finalize(&mut src);
    let err = remux(src, 4096).expect_err("no av1C to build");
    let typed = std::error::Error::source(&err).and_then(|s| s.downcast_ref::<MkvError>());
    assert_eq!(
        typed,
        Some(&MkvError::Mux(MuxError::Config(pf_mkv::config::ConfigError::NoSequenceHeader)))
    );
}
