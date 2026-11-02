#![cfg(feature = "pipeline")]
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::expect_used
)]

mod common;

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pf_open::pipeline::{BuildError, Out, demuxer, route};
use pf_open::{Caps, Kind, OpenAt, Opened, TrackKind, Want};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::fetch::drive_file_log;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;

static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static DESC: ElementDesc = ElementDesc {
    name: "count",
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

/// Counts the buffers a track sends.
struct Count(Arc<AtomicU64>);

impl Element for Count {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }
}

fn path_of(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

/// `rel` opened through its file as a source opens it.
fn opened(rel: &str) -> Opened {
    let f = File::open(path_of(rel)).unwrap();
    let len = f.metadata().unwrap().len();
    let mut log = Vec::new();
    drive_file_log(&mut OpenAt::new(len, Want::ALL, Caps::default()), &f, &mut log).unwrap()
}

/// Every audio and video track of `rel` routed to a counter and the file played to its end.
fn plays(rel: &str, kind: Kind) {
    let o = opened(rel);
    assert_eq!(o.kind, kind, "{rel}");
    let mut p = pf_reactor::pipeline();
    p.set_select_all(false);
    let path = path_of(rel);
    let demux = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, false).unwrap();
    p.preroll().unwrap();
    let mut counts = Vec::new();
    for t in o.tracks.iter().filter(|t| matches!(t.kind, TrackKind::Audio | TrackKind::Video)) {
        let out =
            route(&p, demux, &o, t).unwrap_or_else(|| panic!("{rel} track {} unrouted", t.id));
        let pad = out.pad(&mut p, demux).unwrap();
        let n = Arc::new(AtomicU64::new(0));
        let sink = p.add(Count(Arc::clone(&n)));
        p.link(demux.src(pad), sink.sink(SinkPad(0))).unwrap();
        counts.push((t.id, n));
    }
    assert!(!counts.is_empty(), "{rel}");
    p.run().unwrap();
    for (id, n) in counts {
        assert!(n.load(Ordering::Relaxed) > 0, "{rel} track {id} sent nothing");
    }
}

#[test]
fn every_container_plays_its_tracks_through_the_routes() {
    plays("plugins/formats/mp4/tests/fixtures/cues_last.mkv", Kind::Mkv);
    plays("lib/open/tests/fixtures/tone.webm", Kind::Mkv);
    plays("plugins/formats/mp4/tests/fixtures/tiny_av.mp4", Kind::Mp4);
    plays("plugins/formats/mp4/tests/fixtures/moov_last.mp4", Kind::Mp4);
    plays("plugins/formats/avi/tests/fixtures/tiny.avi", Kind::Avi);
    plays("lib/open/tests/fixtures/tone.opus", Kind::Ogg);
    plays("plugins/formats/flv/tests/fixtures/h264_aac.flv", Kind::Flv);
    plays("plugins/formats/bare/tests/fixtures/tone.flac", Kind::Flac);
    plays("plugins/formats/bare/tests/fixtures/tone.mp3", Kind::Mp3);
    plays("plugins/formats/bare/tests/fixtures/tone.aac", Kind::AdtsAac);
}

/// The Matroska demuxer publishes the Cues the open read, the AVI one the idx1 index.
#[test]
fn the_index_read_at_open_is_published() {
    for rel in [
        "plugins/formats/mp4/tests/fixtures/cues_last.mkv",
        "plugins/formats/avi/tests/fixtures/tiny.avi",
    ] {
        let o = opened(rel);
        let mut p = pf_reactor::pipeline();
        let path = path_of(rel);
        let demux = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, false).unwrap();
        p.preroll().unwrap();
        let ix = p.index(demux).unwrap_or_else(|| panic!("{rel} published nothing"));
        assert!(!ix.entries.is_empty() && ix.complete, "{rel}");
    }
}

#[test]
fn a_track_of_another_kind_or_number_has_no_route() {
    let rel = "plugins/formats/mp4/tests/fixtures/tiny_av.mp4";
    let o = opened(rel);
    let mut p = pf_reactor::pipeline();
    let path = path_of(rel);
    let demux = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, true).unwrap();
    p.preroll().unwrap();
    let mut t = o.tracks[0];
    assert!(matches!(route(&p, demux, &o, &t), Some(Out::Stream(_))));
    t.id = 99;
    assert_eq!(route(&p, demux, &o, &t), None);
    let o = Opened { kind: Kind::Ts, ..o };
    let e = demuxer(&mut p, File::open(&path).unwrap(), &path, &o, false).unwrap_err();
    assert!(matches!(e, BuildError::Unsupported(Kind::Ts)), "{e}");
}
