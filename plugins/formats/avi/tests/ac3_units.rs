// A test's own fixture and sink, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::{Arc, Mutex};

use pf_avi::AviDemux;
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;
use profluens_elements::io::FileSrc;

const FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/out/avi_mpeg4_ac3.avi");

static OFFERS: [OfferDesc; 1] = [OfferDesc::wildcard()];
static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static DESC: ElementDesc = ElementDesc {
    name: "unitsink",
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

/// Each unit's length and first bytes.
type Units = Arc<Mutex<Vec<(usize, Vec<u8>)>>>;
struct UnitSink(Units);

impl Element for UnitSink {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut units = self.0.lock().unwrap();
        while let Some(b) = inputs.pop() {
            let data = b.memory.data();
            units.push((data.len(), data[..data.len().min(8)].to_vec()));
        }
        Ok(Flow::Ok)
    }
}

/// The fixture's AC-3 stream comes out as whole syncframes, a decoder takes each as it is.
#[test]
fn the_fixtures_ac3_units_are_whole_syncframes() {
    if !std::path::Path::new(FIXTURE).exists() {
        eprintln!("skipped, no {FIXTURE}, cargo xtask fixtures makes it");
        return;
    }
    let mut p = pf_reactor::pipeline();
    p.set_pool(1024 * 1024, 32);
    let header = std::fs::read(FIXTURE).unwrap()[..64 * 1024].to_vec();
    let src = p.add(FileSrc::new(FIXTURE));
    let demux = p.add(AviDemux::new(header));
    p.link(src.src(FileSrc::OUT), demux.sink(AviDemux::IN)).expect("src ! demux");
    let added = p.preroll().expect("preroll").srcs;
    let mut caps = Vec::new();
    for ap in &added {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let sink = p.add(UnitSink(Arc::clone(&cap)));
        p.link(ap.element.src(ap.pad), sink.sink(SinkPad(0))).expect("demux ! sink");
        caps.push(cap);
    }
    p.run().expect("run");
    let audio = caps[1].lock().unwrap();
    for (i, (len, head)) in audio.iter().take(6).enumerate() {
        eprintln!("unit {i} len={len} head={head:02x?}");
    }
    eprintln!("units {}", audio.len());
    assert!(audio.len() > 100, "{} units", audio.len());
    assert!(
        audio.iter().all(|(_, h)| h.starts_with(&[0x0b, 0x77])),
        "every unit opens on the AC-3 sync word"
    );
}
