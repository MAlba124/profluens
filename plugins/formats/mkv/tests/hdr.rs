//! HDR metadata through the Matroska demuxer and muxer, the Colour element, Dolby Vision and
//! HDR10+.

// Test code. The allocation ban is for element hot paths.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_hdr::side::FrameMeta;
use pf_mkv::ebml::{self, id};
use pf_mkv::{MatroskaReader, MatroskaWriter, MkvDemux, TrackConfig};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;
use profluens_elements::io::FileSrc;
use profluens_video::color::{
    ChromaSite, DoviConfig, Matrix, Primaries, Range, StreamColor, TransferFn,
};

fn element(out: &mut Vec<u8>, element_id: &[u8], body: &[u8]) {
    ebml::write_binary(out, element_id, body);
}

fn master(out: &mut Vec<u8>, element_id: &[u8], fill: impl FnOnce(&mut Vec<u8>)) {
    let mut body = Vec::new();
    fill(&mut body);
    element(out, element_id, &body);
}

fn block(track: u8, rel: i16, flags: u8, frame: &[u8]) -> Vec<u8> {
    let mut b = vec![0x80 | track];
    b.extend_from_slice(&rel.to_be_bytes());
    b.push(flags);
    b.extend_from_slice(frame);
    b
}

fn hdr10plus(maxscl: u32) -> Vec<u8> {
    let mut m = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
    m.extend(pf_hdr::hdr10plus::sample_payload([maxscl, maxscl / 2, maxscl / 4], 400));
    m
}

fn dovi() -> DoviConfig {
    DoviConfig {
        major: 1,
        minor: 0,
        profile: 8,
        level: 6,
        rpu: true,
        el: false,
        bl: true,
        compat_id: 1,
        compression: 0,
    }
}

fn file(t35_id: Option<u64>) -> Vec<u8> {
    let mut seg = Vec::new();
    master(&mut seg, id::INFO, |i| ebml::write_uint(i, id::TIMESTAMP_SCALE, 1_000_000));
    master(&mut seg, id::TRACKS, |t| {
        master(t, id::TRACK_ENTRY, |e| {
            ebml::write_uint(e, id::TRACK_NUMBER, 1);
            ebml::write_uint(e, id::TRACK_TYPE, 1);
            ebml::write_string(e, id::CODEC_ID, "V_VP9");
            if let Some(v) = t35_id {
                master(e, id::BLOCK_ADDITION_MAPPING, |m| {
                    ebml::write_uint(m, id::BLOCK_ADD_ID_VALUE, v);
                    ebml::write_uint(m, id::BLOCK_ADD_ID_TYPE, 4);
                });
            }
            master(e, id::BLOCK_ADDITION_MAPPING, |m| {
                ebml::write_string(m, id::BLOCK_ADD_ID_NAME, "Dolby Vision configuration");
                ebml::write_uint(m, id::BLOCK_ADD_ID_TYPE, u64::from(u32::from_be_bytes(*b"dvvC")));
                ebml::write_binary(m, id::BLOCK_ADD_ID_EXTRA_DATA, &dovi().to_record());
            });
            master(e, id::VIDEO, |v| {
                ebml::write_uint(v, id::PIXEL_WIDTH, 64);
                ebml::write_uint(v, id::PIXEL_HEIGHT, 64);
                master(v, id::COLOUR, |c| {
                    ebml::write_uint(c, id::MATRIX_COEFFICIENTS, 9);
                    ebml::write_uint(c, id::COLOUR_RANGE, 1);
                    ebml::write_uint(c, id::TRANSFER_CHARACTERISTICS, 16);
                    ebml::write_uint(c, id::PRIMARIES, 9);
                    ebml::write_uint(c, id::CHROMA_SITING_HORZ, 1);
                    ebml::write_uint(c, id::CHROMA_SITING_VERT, 1);
                });
            });
        });
    });
    let add_id = t35_id.unwrap_or(4);
    master(&mut seg, id::CLUSTER, |c| {
        ebml::write_uint(c, id::TIMESTAMP, 0);
        for k in 0..6u8 {
            master(c, id::BLOCK_GROUP, |g| {
                element(g, id::BLOCK, &block(1, i16::from(k) * 40, 0, &[k; 40]));
                if k > 0 {
                    ebml::write_binary(g, id::REFERENCE_BLOCK, &[0xD8]);
                }
                master(g, id::BLOCK_ADDITIONS, |a| {
                    master(a, id::BLOCK_MORE, |m| match k % 2 {
                        0 => {
                            ebml::write_uint(m, id::BLOCK_ADD_ID, add_id);
                            ebml::write_binary(
                                m,
                                id::BLOCK_ADDITIONAL,
                                &hdr10plus(10_000 * u32::from(k + 1)),
                            );
                        }
                        _ => ebml::write_binary(m, id::BLOCK_ADDITIONAL, b"opaque, BlockAddID 1"),
                    });
                });
            });
        }
    });
    let mut out = Vec::new();
    master(&mut out, id::EBML, |b| ebml::write_string(b, id::DOC_TYPE, "webm"));
    element(&mut out, id::SEGMENT, &seg);
    out
}

fn header(file: &[u8]) -> Vec<u8> {
    let at = file.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster");
    file[..at].to_vec()
}

#[test]
fn the_reader_takes_the_mapping_the_siting_and_each_blocks_t35() {
    for t35_id in [Some(5), None] {
        let mut r = MatroskaReader::new();
        r.push(&file(t35_id)).expect("reads");
        let t = &r.tracks()[0];
        assert_eq!(t.dovi, Some(dovi()));
        assert_eq!(t.chroma_siting, (1, 1));
        assert_eq!(t.t35_add_id, t35_id.unwrap_or(4), "the mapped id, else WebM's 4");
        let mut n = 0;
        while let Some(f) = r.next_frame() {
            assert_eq!(f.data, [n; 40]);
            match n % 2 {
                0 => assert_eq!(f.t35, hdr10plus(10_000 * u32::from(n + 1)), "frame {n}"),
                _ => assert!(f.t35.is_empty(), "an addition under another id is not T.35"),
            }
            n += 1;
        }
        assert_eq!(n, 6);
    }
}

static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static REC: ElementDesc = ElementDesc {
    name: "rec",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default)]
struct Got {
    peaks: Vec<Option<u32>>,
    colour: Option<StreamColor>,
}

struct Rec(Arc<Mutex<Got>>);

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &REC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let meta = FrameMeta::read(buf.memory.side()).expect("valid side data");
            let peak = meta.hdr10plus().map(|h| h.expect("parses").windows[0].maxscl[0]);
            self.0.lock().unwrap().peaks.push(peak);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(f) = event {
            self.0.lock().unwrap().colour = Some(StreamColor::read(ctx, f));
        }
        Ok(())
    }
}

fn demux(name: &str, bytes: &[u8], pull: bool) -> Got {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mkv_hdr");
    std::fs::create_dir_all(&dir).expect("a tmp dir");
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write the fixture");
    let mut p = pf_reactor::pipeline();
    let src = FileSrc::new(path.to_str().expect("utf-8"));
    let src = p.add(if pull { src } else { src.push_only() });
    let demux = p.add(MkvDemux::new(header(bytes)));
    p.link(src.src(FileSrc::OUT), demux.sink(MkvDemux::IN)).expect("link");
    let got = Arc::new(Mutex::new(Got::default()));
    for ap in p.preroll().expect("preroll").srcs {
        let rec = p.add(Rec(Arc::clone(&got)));
        p.link(ap.element.src(ap.pad), rec.sink(SinkPad(0))).expect("link");
    }
    p.run().expect("run");
    Arc::try_unwrap(got).ok().expect("one owner").into_inner().unwrap()
}

#[test]
fn hdr10plus_additions_ride_each_frame_as_side_data_pushed_and_pulled() {
    let bytes = file(Some(5));
    for pull in [false, true] {
        let got = demux(if pull { "pulled.webm" } else { "pushed.webm" }, &bytes, pull);
        let want: Vec<Option<u32>> =
            (0..6u32).map(|k| (k % 2 == 0).then_some(10_000 * (k + 1))).collect();
        assert_eq!(got.peaks, want, "pull {pull}");
        let c = got.colour.expect("announced");
        assert_eq!(
            (c.matrix, c.range, c.transfer, c.primaries),
            (
                Some(Matrix::Bt2020),
                Some(Range::Limited),
                Some(TransferFn::Pq),
                Some(Primaries::Bt2020)
            )
        );
        assert_eq!(c.chroma, Some(ChromaSite::TopLeft));
        assert_eq!(c.dovi, Some(dovi()), "the record reaches the announcement");
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../video/hdr/tests/fixtures")
        .join(name);
    std::fs::read(path).expect("the fixture")
}

#[test]
fn an_ffmpeg_hdr10_file_states_what_its_colour_element_holds() {
    let mut r = MatroskaReader::new();
    r.push(&fixture("hdr10_x265.mkv")).expect("reads");
    let t = &r.tracks()[0];
    assert_eq!((t.colour_matrix, t.colour_range, t.chroma_siting), (9, 1, (1, 1)));
    let got = demux("x265.mkv", &fixture("hdr10_x265.mkv"), true);
    let c = got.colour.expect("announced");
    assert_eq!(
        (c.matrix, c.range, c.chroma),
        (Some(Matrix::Bt2020), Some(Range::Limited), Some(ChromaSite::TopLeft))
    );
    assert_eq!((c.transfer, c.primaries), (None, None));
    assert_eq!(got.peaks, [None; 4]);
}

#[test]
fn the_writer_writes_the_siting_and_the_mapping_back() {
    let mut tc = TrackConfig::vp8(1, 64, 64);
    let colour = pf_mkv::ColourConfig {
        matrix: 9,
        range: 1,
        chroma_siting: (1, 2),
        dovi: Some(dovi()),
        ..Default::default()
    };
    tc.video.as_mut().unwrap().colour = Some(colour);
    let mut w = MatroskaWriter::new(vec![tc]);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    w.write_frame(&mut out, 1, 0, &[1; 8], true).unwrap();
    w.finalize(&mut out);
    let mut r = MatroskaReader::new();
    r.push(&out).unwrap();
    let t = &r.tracks()[0];
    assert_eq!((t.chroma_siting, t.dovi), ((1, 2), Some(dovi())));
    let at = out.windows(2).position(|w| w == id::BLOCK_ADD_ID_TYPE).expect("a type");
    assert_eq!(&out[at + 3..at + 7], b"dvvC");
    let mut tc = TrackConfig::vp8(1, 64, 64);
    tc.video.as_mut().unwrap().colour =
        Some(pf_mkv::ColourConfig { dovi: Some(dovi()), ..Default::default() });
    let mut w = MatroskaWriter::new(vec![tc]);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    assert!(!out.windows(2).any(|w| w == id::COLOUR));
    assert!(out.windows(2).any(|w| w == id::BLOCK_ADDITION_MAPPING));
}
