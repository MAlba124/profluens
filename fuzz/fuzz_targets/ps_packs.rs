//! The pf-ps parsers on raw bytes, without the element. Pack, system header and map parsing, the
//! private stream 1 split, DVD LPCM conversion, the demuxer under any cut of its input, the video
//! and subpicture framers, the head scan and the SCR bisection.
//! Knob bits 0-2 pick the piece size, 3-4 the video framing, 5-7 the bisection target.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_ps::es::{Au, Framer, Spu, Stamp, Video};
use pf_ps::lpcm::{self, Lpcm};
use pf_ps::pack::{self, Need, Psm};
use pf_ps::seek::{Base, Bisect, PROBES_MAX, Step};
use pf_ps::{Demux, Event, last_ticks, scan_head, streams};

const PIECES: [usize; 8] = [1, 3, 7, 64, 188, 2048, 4096, usize::MAX];
const VIDEO: [Video; 4] = [Video::Mpeg12, Video::Mpeg4, Video::H264, Video::Hevc];

/// One event, its kind, stream, stamp, length and duration.
type Row = (u8, u16, u64, usize, Option<u64>);

/// What the demuxer said, in order, one row per event.
fn events(data: &[u8], piece: usize) -> (Vec<Row>, pf_ps::Stats) {
    let mut d = Demux::new();
    let mut out = Vec::new();
    let mut sink = |e: Event<'_>| {
        out.push(match e {
            Event::Pack { offset, pack } => (0, 0, offset, 0, Some(pack.scr)),
            Event::SystemHeader(h) => (1, u16::from(h.streams), 0, 0, None),
            Event::Psm(p) => (2, u16::from(p.version), 0, 0, None),
            Event::Unit(u) => (3, u.key, u.offset, u.payload.len(), u.pts),
            Event::End { offset } => (4, 0, offset, 0, None),
            Event::Anomaly(a) => (5, a.kind as u16, a.offset, 0, None),
        })
    };
    for c in data.chunks(piece.min(data.len().max(1))) {
        d.push(c, &mut sink);
    }
    d.flush(&mut sink);
    (out, d.stats())
}

fn parsers(data: &[u8]) {
    if let Need::Len(n) = pack::item_len(data) {
        assert!((4..=pack::ITEM_MAX).contains(&n), "an item of {n}");
    }
    if let Ok(p) = pack::parse_pack(data) {
        assert!(p.scr < 1 << 33 && p.scr_ext < 512 && p.mux_rate < 1 << 22);
    }
    let _ = pack::parse_system_header(data);
    let mut psm = Psm::default();
    if pack::parse_psm(data, &mut psm).is_ok() {
        assert!(psm.present);
    }
    if let Some((_, header, rest)) = pack::split_sub(data) {
        assert_eq!(header.len() + rest.len(), data.len());
    }
}

fn pcm(data: &[u8]) {
    let Ok(l) = Lpcm::parse(data) else { return };
    let (bin, bout) = l.block();
    assert!(bin > 0 && bin <= lpcm::BLOCK_MAX && bout % l.width() == 0);
    let src = &data[lpcm::HEADER.min(data.len())..];
    let mut out = vec![0u8; src.len() / bin * bout + 1];
    let (used, wrote) = l.convert(src, &mut out);
    assert_eq!((used % bin, wrote, used / bin * bout), (0, used / bin * bout, wrote));
    assert!(src.len() - used < bin, "a whole block left over");
}

fn frame(data: &[u8], kind: Video) {
    let (mut fed, mut out, mut spu_fed, mut spu_out) = (0usize, 0usize, 0usize, 0usize);
    let mut framer = Framer::new(kind, 0, 1 << 20);
    let mut spu = Spu::new();
    let mut d = Demux::new();
    let mut sink = |e: Event<'_>| {
        let Event::Unit(u) = e else { return };
        let stamp = u.pts.map(|pts| Stamp { pts, dts: u.dts });
        match u.key >> 8 {
            0xE0..=0xEF => {
                fed += u.payload.len();
                framer.push(u.payload, stamp, u.offset, &mut |au: Au<'_>| out += au.bytes.len());
            }
            0xBD if u.key & 0xE0 == 0x20 => {
                spu_fed += u.payload.len();
                spu.push(u.payload, stamp, u.offset, &mut |au: Au<'_>| spu_out += au.bytes.len());
            }
            0xBD if u.key & 0xF8 == 0xA0 => {
                pcm(&[u.header.get(4..).unwrap_or_default(), u.payload].concat())
            }
            _ => {}
        }
    };
    d.push(data, &mut sink);
    d.flush(&mut sink);
    framer.flush(&mut |au: Au<'_>| out += au.bytes.len());
    assert!(out <= fed, "{out} unit bytes out of {fed}");
    assert!(spu_out <= spu_fed, "{spu_out} subpicture bytes out of {spu_fed}");
}

fn bisect(data: &[u8], knob: u8) {
    let base = Base { origin: i64::from(knob) * 9_000, slack: 900_000 };
    let target = i64::from(knob >> 5) * 450_000 - 90_000;
    let mut b = Bisect::new(base, target, data.len() as u64);
    for _ in 0..=PROBES_MAX {
        match b.step() {
            Step::Probe { at, len } => {
                assert!(at < data.len() as u64 && len > 0, "a probe at {at} of {len}");
                let end = (at + u64::from(len)).min(data.len() as u64);
                b.served(at, &data[at as usize..end as usize]);
            }
            Step::Land(l) => {
                if let Some(l) = l {
                    assert!(l.at < data.len() as u64 && l.ticks <= target);
                }
                return;
            }
        }
    }
    panic!("no landing after {PROBES_MAX} probes");
}

fuzz_target!(|input: &[u8]| {
    let Some((&knob, data)) = input.split_first() else { return };
    parsers(data);
    pcm(data);
    let (whole, stats) = events(data, usize::MAX);
    assert_eq!(stats.bytes, data.len() as u64);
    let (cut, _) = events(data, PIECES[usize::from(knob & 7)]);
    assert_eq!(whole, cut, "the cut of the input changes nothing");
    for e in &whole {
        assert!(e.2 <= data.len() as u64 && e.3 <= data.len(), "{e:?}");
    }
    frame(data, VIDEO[usize::from(knob >> 3 & 3)]);
    bisect(data, knob);
    let head = scan_head(data);
    for s in &head.seen {
        let _ = streams::codec_of(s.key, head.psm[usize::from(s.key >> 8)], &s.sniff, head.mpeg2);
    }
    if let Some((_, least)) = head.opening {
        let _ = last_ticks(data, Base { origin: least, slack: 900_000 });
    }
});
