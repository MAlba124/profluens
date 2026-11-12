//! The hardware decoders read a stream as stored, length prefixed and led by its configuration
//! record (`h264/avcc`, `h265/hvcc`), to the pictures its Annex B form decodes to. Skips without a
//! device.

// Test code.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::element::Element;
use profluens_core::id::SinkPad;
use profluens_core::time::Timestamp;

use pf_vaapi::{VaapiH264Dec, VaapiH265Dec, h264parse, h265parse};

const IPB_H264: &[u8] = include_bytes!("fixtures/ipb_320x240.h264");
const OPEN_GOP_HEVC: &[u8] = include_bytes!("fixtures/opengop_320x240.hevc");
const SLOT: usize = 1 << 20;
const FRAME_NS: u64 = 40_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A parameter set, which the record holds.
    Set,
    /// The first slice of a picture.
    First,
    Slice,
    Other,
}

fn kind_h264(nal: &[u8]) -> Kind {
    match nal[0] & 0x1F {
        7 | 8 => Kind::Set,
        // first_mb_in_slice is 0, a one bit
        1 | 5 if nal[1] & 0x80 != 0 => Kind::First,
        1 | 5 => Kind::Slice,
        _ => Kind::Other,
    }
}

fn kind_hevc(nal: &[u8]) -> Kind {
    match (nal[0] >> 1) & 0x3F {
        32..=34 => Kind::Set,
        // first_slice_segment_in_pic_flag
        0..=31 if nal[2] & 0x80 != 0 => Kind::First,
        0..=31 => Kind::Slice,
        _ => Kind::Other,
    }
}

/// The NALs of each access unit, cut ahead of what leads a picture's first slice.
fn access_units(nals: Vec<&[u8]>, kind: fn(&[u8]) -> Kind) -> Vec<Vec<&[u8]>> {
    let (mut units, mut sliced) = (vec![Vec::new()], false);
    for nal in nals {
        let k = kind(nal);
        if sliced && k != Kind::Slice {
            units.push(Vec::new());
            sliced = false;
        }
        sliced |= matches!(k, Kind::First | Kind::Slice);
        units.last_mut().expect("one unit").push(nal);
    }
    units
}

fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect()
}

fn prefixed(nals: &[&[u8]], size: usize) -> Vec<u8> {
    let lead = |n: &&[u8]| (n.len() as u32).to_be_bytes()[4 - size..].to_vec();
    nals.iter().flat_map(|n| [&lead(n)[..], n].concat()).collect()
}

fn sized(set: &[u8]) -> Vec<u8> {
    [&(set.len() as u16).to_be_bytes()[..], set].concat()
}

/// An avcC of the sets, the first of each kind of id.
fn avcc(sets: &[&[u8]], size: usize) -> Vec<u8> {
    let of = |t: u8| sets.iter().filter(move |s| s[0] & 0x1F == t);
    let sps = of(7).next().expect("an SPS");
    let mut r =
        vec![1, sps[1], sps[2], sps[3], 0xFC | (size as u8 - 1), 0xE0 | of(7).count() as u8];
    r.extend(of(7).flat_map(|s| sized(s)));
    r.push(of(8).count() as u8);
    r.extend(of(8).flat_map(|s| sized(s)));
    r
}

/// An hvcC of the sets, an array a NAL type.
fn hvcc(sets: &[&[u8]], size: usize) -> Vec<u8> {
    let mut r = vec![0u8; 23];
    (r[0], r[21], r[22]) = (1, 0xFC | (size as u8 - 1), 3);
    for t in 32..=34u8 {
        let of: Vec<&&[u8]> = sets.iter().filter(|s| (s[0] >> 1) & 0x3F == t).collect();
        r.push(0x80 | t);
        r.extend_from_slice(&(of.len() as u16).to_be_bytes());
        r.extend(of.iter().flat_map(|s| sized(s)));
    }
    r
}

/// A stream both ways, its units and the record that leads the stored ones.
struct Both {
    annex_b: Vec<Vec<u8>>,
    record: Vec<u8>,
    stored: Vec<Vec<u8>>,
}

fn both(
    units: Vec<Vec<&[u8]>>,
    kind: fn(&[u8]) -> Kind,
    record: fn(&[&[u8]], usize) -> Vec<u8>,
    size: usize,
) -> Both {
    let mut sets: Vec<&[u8]> = Vec::new();
    for nal in units.iter().flatten().filter(|n| kind(n) == Kind::Set) {
        if !sets.contains(nal) {
            sets.push(nal);
        }
    }
    // the sets leave the units, so the record alone gives them
    let bare = |u: &Vec<&[u8]>| {
        let kept: Vec<&[u8]> = u.iter().copied().filter(|n| kind(n) != Kind::Set).collect();
        prefixed(&kept, size)
    };
    Both {
        annex_b: units.iter().map(|u| annex_b(u)).collect(),
        record: record(&sets, size),
        stored: units.iter().map(bare).collect(),
    }
}

type Pictures = Vec<(Timestamp, Vec<u8>)>;

/// What a buffer into the decoder is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Is {
    Frame,
    /// The record as an encoder sends it, unflagged.
    Record,
    /// The record as a demuxer sends it, flagged a head.
    Head,
}

/// What the decoder puts out of `units`, linked as `family` when given.
fn decode(dec: impl Element + 'static, family: Option<&str>, units: &[(&[u8], Is)]) -> Pictures {
    let mut h = Harness::with_slot_size(dec, SLOT);
    if let Some(family) = family {
        h.fix_sink_format(SinkPad(0), family, &[]);
    }
    h.start().expect("start");
    let mut n = 0;
    for &(unit, is) in units {
        let mut buf = h.alloc(unit);
        match is {
            Is::Frame => {
                buf.pts = Timestamp::from_nanos(n * FRAME_NS);
                n += 1;
            }
            Is::Record => {}
            Is::Head => buf.flags = BufferFlags::HEADER,
        }
        h.push(buf).expect("push");
    }
    let mut out = h.drain_outputs();
    out.extend(h.eos().expect("eos"));
    let warned: Vec<String> = h
        .bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            BusMessage::Warning { error, .. } | BusMessage::Error { error, .. } => {
                Some(error.to_string())
            }
            _ => None,
        })
        .collect();
    assert!(warned.is_empty(), "as {family:?}: {warned:?}");
    out.iter().map(|b| (b.pts, b.memory.data().to_vec())).collect()
}

/// The stream decoded as Annex B, then as stored with the record unflagged ahead, as an encoder
/// sends it, and with the record flagged a head ahead and again midway, as a demuxer does.
fn check<D: Element + 'static>(make: fn() -> D, stored: &str, s: &Both, pictures: usize) {
    let plain: Vec<(&[u8], Is)> = s.annex_b.iter().map(|u| (&u[..], Is::Frame)).collect();
    let want = decode(make(), None, &plain);
    assert_eq!(want.len(), pictures, "every picture of the Annex B stream");

    let units = s.stored.iter().map(|u| (&u[..], Is::Frame));
    let led: Vec<(&[u8], Is)> = std::iter::once((&s.record[..], Is::Record)).chain(units).collect();
    let got = decode(make(), Some(stored), &led);
    assert_eq!(got.len(), want.len(), "{stored}: every picture");
    let off: Vec<usize> = (0..want.len()).filter(|&i| got[i] != want[i]).collect();
    assert!(off.is_empty(), "{stored}: pictures {off:?} differ from the Annex B stream's");

    let mut headed = led.clone();
    headed[0].1 = Is::Head;
    headed.insert(1 + s.stored.len() / 2, (&s.record[..], Is::Head));
    let got = decode(make(), Some(stored), &headed);
    assert!(got == want, "{stored}: the record as a head, ahead and midway");
}

#[test]
fn h264_as_stored_decodes_to_the_pictures_of_its_annex_b_form() {
    if !pf_vaapi::probe().is_some_and(|caps| caps.supports("h264/annexb")) {
        return eprintln!("skip, no VA-API device decodes H.264");
    }
    let nals = h264parse::split_nals(IPB_H264);
    let nals = nals.iter().map(|n| n.raw(IPB_H264)).collect();
    let units = access_units(nals, kind_h264);
    let pictures = units.len();
    assert!(pictures > 10, "the fixture's pictures, {pictures}");
    // two octet lengths, which only the record states
    let s = both(units, kind_h264, avcc, 2);
    check(VaapiH264Dec::new, "h264/avcc", &s, pictures);
}

#[test]
fn hevc_as_stored_decodes_to_the_pictures_of_its_annex_b_form() {
    if !pf_vaapi::probe().is_some_and(|caps| caps.supports("h265/annexb")) {
        return eprintln!("skip, no VA-API device decodes HEVC");
    }
    let nals = h265parse::split_nals(OPEN_GOP_HEVC);
    let nals = nals.iter().map(|n| n.raw(OPEN_GOP_HEVC)).collect();
    let units = access_units(nals, kind_hevc);
    assert_eq!(units.len(), 50, "the fixture's pictures");
    let s = both(units, kind_hevc, hvcc, 4);
    check(VaapiH265Dec::new, "h265/hvcc", &s, 50);
}
