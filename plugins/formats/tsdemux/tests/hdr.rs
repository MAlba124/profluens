//! The Dolby Vision record of a TS DOVI descriptor reaches the video pad's format.

#![allow(clippy::disallowed_methods)] // test fixtures

use std::sync::{Arc, Mutex};

use pf_hdr::dovi::DoviConfig;
use pf_tsdemux::TsDemux;
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{OfferDesc, Value};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

fn fixture() -> Vec<u8> {
    let path = format!("{}/../ts/tests/fixtures/hevc_ac3.ts", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn dovi_descriptor() -> Vec<u8> {
    let bits: u16 = 8 << 9 | 6 << 3 | 1 << 2 | 1;
    let mut d = vec![0xB0, 5, 1, 0];
    d.extend_from_slice(&bits.to_be_bytes());
    d.push(1 << 4);
    d
}

fn with_descriptor(ts: &[u8], descriptor: &[u8]) -> Vec<u8> {
    let pid_of = |p: &[u8]| u16::from(p[1] & 0x1F) << 8 | u16::from(p[2]);
    let pat = ts.chunks(188).find(|p| pid_of(p) == 0).expect("a PAT");
    let pmt_pid = u16::from(pat[15] & 0x1F) << 8 | u16::from(pat[16]);
    let mut out = ts.to_vec();
    let mut patched = 0;
    for p in out.chunks_mut(188) {
        if pid_of(p) != pmt_pid || p[1] & 0x40 == 0 || p[3] & 0x30 != 0x10 {
            continue;
        }
        let start = 5 + usize::from(p[4]);
        let section_len = usize::from(p[start + 1] & 0x0F) << 8 | usize::from(p[start + 2]);
        let end = start + 3 + section_len;
        let mut section = p[start..end - 4].to_vec();
        let info_len = usize::from(section[10] & 0x0F) << 8 | usize::from(section[11]);
        let mut at = 12 + info_len;
        while at + 5 <= section.len() {
            let es_len = usize::from(section[at + 3] & 0x0F) << 8 | usize::from(section[at + 4]);
            if section[at] == 0x24 {
                let new_len = es_len + descriptor.len();
                section[at + 3] = section[at + 3] & 0xF0 | (new_len >> 8) as u8;
                section[at + 4] = new_len as u8;
                let insert = at + 5 + es_len;
                section.splice(insert..insert, descriptor.iter().copied());
                break;
            }
            at += 5 + es_len;
        }
        let new_section_len = section.len() + 4 - 3;
        section[1] = section[1] & 0xF0 | (new_section_len >> 8) as u8;
        section[2] = new_section_len as u8;
        let crc = pf_mpegts::crc32(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        assert!(start + section.len() <= 188, "the section still fits its packet");
        p[start..start + section.len()].copy_from_slice(&section);
        p[start + section.len()..].fill(0xFF);
        patched += 1;
    }
    assert!(patched > 0, "a PMT was patched");
    out
}

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "filesrc",
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

struct Src(Vec<u8>, usize);

impl Element for Src {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.1 == self.0.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else { return Ok(Flow::Ok) };
        let n = buf.memory.capacity().min(4096).min(self.0.len() - self.1);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
        buf.memory.set_len(n);
        self.1 += n;
        ctx.out(SrcPad(0)).push(buf);
        Ok(Flow::Ok)
    }
}

static REC_OFFERS: [OfferDesc; 1] = [OfferDesc::wildcard()];
static REC_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &REC_OFFERS }];
static REC_DESC: ElementDesc = ElementDesc {
    name: "recsink",
    sinks: &REC_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

type Seen = Arc<Mutex<Vec<(String, Option<i64>)>>>;

struct Rec(Seen);

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &REC_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {}
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(f) = event {
            let dovi = match ctx.field_id(pf_hdr::color::FIELD_DOVI).and_then(|id| f.get(id)) {
                Some(Value::Int(v)) => Some(v),
                _ => None,
            };
            let family = ctx.family_name(f.family).unwrap_or("?").to_string();
            self.0.lock().unwrap().push((family, dovi));
        }
        Ok(())
    }
}

fn announced(ts: Vec<u8>) -> Vec<Vec<(String, Option<i64>)>> {
    let mut p = Pipeline::new();
    let head = ts.clone();
    let src = p.add(Src(ts, 0));
    let demux = p.add(TsDemux::new(head));
    p.link(src.src(SrcPad(0)), demux.sink(TsDemux::IN)).expect("link");
    p.set_select_all(true);
    let mut got = Vec::new();
    for a in p.preroll().expect("preroll").srcs {
        let rec = Arc::new(Mutex::new(Vec::new()));
        let sink = p.add(Rec(Arc::clone(&rec)));
        p.link(a.element.src(a.pad), sink.sink(SinkPad(0))).expect("link");
        got.push(rec);
    }
    p.run().expect("run");
    got.into_iter().map(|r| r.lock().unwrap().clone()).collect()
}

#[test]
fn the_dovi_descriptor_reaches_the_video_announcement() {
    let got = announced(with_descriptor(&fixture(), &dovi_descriptor()));
    let (family, dovi) = &got[0][0];
    assert_eq!(family, "h265/annexb");
    let d = DoviConfig::unpack(dovi.expect("the record"));
    assert_eq!((d.profile, d.level, d.rpu, d.el, d.bl, d.compat_id), (8, 6, true, false, true, 1));
    assert_eq!(got[1][0].1, None, "the audio pad states none");
    let plain = announced(fixture());
    assert_eq!(plain[0][0].1, None, "no descriptor, no record");
}
