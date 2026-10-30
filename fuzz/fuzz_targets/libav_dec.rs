//! pf-libav's decoder elements through pf-harness, the threadless element rig, over the C shim
//! and libavcodec. Input is a knob byte then units, each behind a `u16` LE length. `knob & 15`
//! picks the decoder (see `KINDS`), bits 4 and 5 the sample formats the audio output takes, bit 6
//! a second run that must put out the same, the stream whole for the parsed audio codecs and the
//! same units again for the rest. A picture must be the size its announced format says, audio
//! whole samples with stamps that never step back.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::hash::{DefaultHasher, Hasher};

use libfuzzer_sys::fuzz_target;
use pf_harness::Harness;
use pf_libav::{
    LibavAudioDec, LibavH264Dec, LibavH265Dec, LibavMpeg2Dec, LibavMpeg4Dec, LibavVp8Dec,
    LibavVp9Dec,
};
use profluens_core::buffer::Buffer;
use profluens_core::bus::BusMessage;
use profluens_core::element::Element;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::time::Timestamp;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    H264,
    H265,
    Vp8,
    Vp9,
    Mpeg2,
    Mpeg4,
    Aac,
    Ac3,
    Mp3,
    Mp3Indexed,
    Dts,
    TrueHd,
    Mlp,
    Vorbis,
    Alac,
}

/// The decoder of `knob & 15`, 15 wraps to the first.
const KINDS: [Kind; 15] = [
    Kind::H264,
    Kind::H265,
    Kind::Vp8,
    Kind::Vp9,
    Kind::Mpeg2,
    Kind::Mpeg4,
    Kind::Aac,
    Kind::Ac3,
    Kind::Mp3,
    Kind::Mp3Indexed,
    Kind::Dts,
    Kind::TrueHd,
    Kind::Mlp,
    Kind::Vorbis,
    Kind::Alac,
];

const TAKES: [&[&str]; 4] = [&[], &["s16"], &["f32", "s32", "s16"], &["u8", "s24", "s32", "f32"]];

/// Big enough for a 1080p picture, larger ones go to libavcodec's own allocator.
const VIDEO_SLOT: usize = 4 << 20;
const VIDEO_SLOTS: u32 = 48;
const AUDIO_SLOT: usize = 64 << 10;
const FRAME_NS: u64 = 40_000_000;
/// Cranks after a push while input waits, the scheduler would run it again.
const CRANKS: usize = 64;

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let kind = KINDS[usize::from(knob & 15) % KINDS.len()];
    let units: Vec<&[u8]> = records(rest).collect();
    let takes = TAKES[usize::from(knob >> 4 & 3)];
    let again = knob & 0x40 != 0;
    if say() {
        eprintln!("libav {kind:?} units {}", units.len());
    }
    match kind {
        Kind::H264 | Kind::H265 | Kind::Vp8 | Kind::Vp9 | Kind::Mpeg2 | Kind::Mpeg4 => {
            let first = video(kind, &units);
            if again {
                assert!(video(kind, &units) == first, "{kind:?} decoded otherwise a second time");
            }
        }
        _ => {
            let first = audio(kind, &units, takes, rest.len());
            if !again {
                return;
            }
            if parsed(kind) {
                let whole: Vec<u8> = units.concat();
                let second = audio(kind, &[&whole], takes, rest.len());
                if say() && first.pcm != second.pcm {
                    let at = first.pcm.iter().zip(&second.pcm).position(|(a, b)| a != b);
                    eprintln!("libav first difference at {at:?}");
                }
                // A frame at a format change is dropped while output gathers, which depends on
                // the cut, and libavcodec's parsers resync on damage by how much they are offered,
                // so only a clean stream of one format compares.
                // libavcodec's MLP decoder only logs a failed parity or checksum, so damage
                // passes for clean there.
                let clean = |a: &Audio| a.changes <= 1 && !a.warned;
                let silent = matches!(kind, Kind::Mlp | Kind::TrueHd);
                if clean(&first) && clean(&second) && !silent {
                    assert!(first.pcm == second.pcm, "{kind:?} decoded otherwise whole");
                }
            } else {
                let second = audio(kind, &units, takes, rest.len());
                assert!(second.pcm == first.pcm, "{kind:?} decoded otherwise a second time");
            }
        }
    }
});

fn records(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let (len, rest) = match data.split_first_chunk::<2>() {
            Some((len, rest)) => (usize::from(u16::from_le_bytes(*len)), rest),
            None if data.is_empty() => return None,
            None => (data.len(), std::mem::take(&mut data)),
        };
        let (rec, tail) = rest.split_at(len.min(rest.len()));
        data = tail;
        Some(rec)
    })
}

/// Codecs whose bytes go through a libavcodec parser, so any cut of them is one stream.
fn parsed(kind: Kind) -> bool {
    matches!(kind, Kind::Ac3 | Kind::Mp3 | Kind::Mp3Indexed | Kind::Dts | Kind::TrueHd | Kind::Mlp)
}

fn int(h: &Harness, field: &str) -> Option<i64> {
    let f = h.announced()?;
    match f.get(h.vocabulary().field_id(field)?)? {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

fn id(h: &Harness, field: &str) -> Option<String> {
    let f = h.announced()?;
    let Value::Id(v) = f.get(h.vocabulary().field_id(field)?)? else {
        return None;
    };
    h.vocabulary().value_name(v).map(str::to_string)
}

/// Pushes a unit, cranks while input waits. False once the element failed.
fn push(h: &mut Harness, unit: &[u8], pts: Timestamp, duration: Timestamp) -> bool {
    let mut buf = h.alloc(unit);
    (buf.pts, buf.duration) = (pts, duration);
    if h.push(buf).is_err() {
        return false;
    }
    for _ in 0..CRANKS {
        if h.input_len() == 0 {
            break;
        }
        if h.crank().is_err() {
            return false;
        }
    }
    true
}

/// A picture's bytes for the announced format.
fn picture_len(h: &Harness) -> Option<usize> {
    let (w, ht) = (int(h, "width")? as usize, int(h, "height")? as usize);
    let chroma = 2 * w.div_ceil(2) * ht.div_ceil(2);
    match id(h, "pixfmt")?.as_str() {
        "i420" => Some(w * ht + chroma),
        "p010" => Some(2 * (w * ht + chroma)),
        "gray8" => Some(w * ht),
        other => panic!("a picture announced as {other}"),
    }
}

fn video(kind: Kind, units: &[&[u8]]) -> Vec<u64> {
    match kind {
        Kind::H264 => video_with(LibavH264Dec::new(), kind, units),
        Kind::H265 => video_with(LibavH265Dec::new(), kind, units),
        Kind::Vp8 => video_with(LibavVp8Dec::new(), kind, units),
        Kind::Vp9 => video_with(LibavVp9Dec::new(), kind, units),
        Kind::Mpeg2 => video_with(LibavMpeg2Dec::new(), kind, units),
        _ => video_with(LibavMpeg4Dec::new(), kind, units),
    }
}

fn video_with(e: impl Element + 'static, kind: Kind, units: &[&[u8]]) -> Vec<u64> {
    let mut h = Harness::with_pool(e, VIDEO_SLOT, VIDEO_SLOTS);
    let mut out = Vec::new();
    let mut take = |h: &mut Harness, bufs: Vec<Buffer>, changes: u32| {
        let len = picture_len(h);
        for b in bufs {
            let data = b.memory.data();
            // Only a format that held through the push sizes its pictures.
            if let (true, Some(len)) = (h.format_changes() == changes, len) {
                assert_eq!(data.len(), len, "{kind:?} picture of {} bytes", data.len());
            }
            let mut hash = DefaultHasher::new();
            hash.write(data);
            hash.write_u64(b.pts.0);
            out.push(hash.finish());
        }
    };
    let frame = Timestamp::from_nanos(FRAME_NS);
    for (i, u) in units.iter().enumerate() {
        let changes = h.format_changes();
        let ok = push(&mut h, u, Timestamp::from_nanos(i as u64 * FRAME_NS), frame);
        let bufs = h.drain_outputs();
        take(&mut h, bufs, changes);
        if !ok {
            return out;
        }
    }
    let changes = h.format_changes();
    if let Ok(bufs) = h.eos() {
        take(&mut h, bufs, changes);
    }
    if say() {
        eprintln!("libav {kind:?} pictures {}", out.len());
    }
    // A unit gives at most two pictures, a packed MPEG-4 B-frame, and the drain a whole DPB.
    assert!(out.len() <= 2 * units.len() + 20, "{} pictures of {} units", out.len(), units.len());
    out
}

fn audio_element(kind: Kind, len: usize) -> LibavAudioDec {
    match kind {
        Kind::Aac => LibavAudioDec::aac(),
        Kind::Ac3 => LibavAudioDec::ac3(),
        Kind::Mp3 => LibavAudioDec::mp3(),
        Kind::Mp3Indexed => LibavAudioDec::mp3().indexing(Some(len as u64)),
        Kind::Dts => LibavAudioDec::dts(),
        Kind::TrueHd => LibavAudioDec::truehd(),
        Kind::Mlp => LibavAudioDec::mlp(),
        Kind::Vorbis => LibavAudioDec::vorbis(),
        _ => LibavAudioDec::alac(),
    }
}

struct Audio {
    pcm: Vec<u8>,
    changes: u32,
    /// A warning or error was posted, a packet failed or a parser lost sync.
    warned: bool,
}

fn sample_bytes(name: &str) -> usize {
    match name {
        "u8" => 1,
        "s16" => 2,
        "s24" => 3,
        "s32" | "f32" => 4,
        other => panic!("audio announced as {other}"),
    }
}

fn audio(kind: Kind, units: &[&[u8]], takes: &[&'static str], len: usize) -> Audio {
    let mut h = Harness::with_slot_size(audio_element(kind, len), AUDIO_SLOT);
    if !takes.is_empty() {
        let values: Vec<ValueDesc> = takes.iter().map(|n| ValueDesc::Id(n)).collect();
        h.fix_src_takes(LibavAudioDec::OUT, "sample", &values);
    }
    let mut pcm = Vec::new();
    let mut last = Timestamp::NONE;
    let mut take = |h: &mut Harness, bufs: Vec<Buffer>, changes: u32| {
        let shape = (int(h, "channels"), int(h, "rate"), id(h, "sample"));
        for b in bufs {
            let data = b.memory.data();
            if let (true, (Some(ch), Some(rate), Some(s))) = (h.format_changes() == changes, &shape)
            {
                assert!((1..=255).contains(ch) && *rate > 0, "{ch} channels at {rate} Hz");
                if !takes.is_empty() {
                    assert!(takes.contains(&s.as_str()), "{s} is not taken");
                }
                let stride = *ch as usize * sample_bytes(s);
                assert_eq!(data.len() % stride, 0, "{} bytes of {stride} byte frames", data.len());
                if b.pts != Timestamp::NONE && last != Timestamp::NONE {
                    assert!(b.pts >= last, "a stamp stepped back from {last:?} to {:?}", b.pts);
                }
            }
            if b.pts != Timestamp::NONE {
                last = b.pts;
            }
            pcm.extend_from_slice(data);
        }
    };
    for (i, u) in units.iter().enumerate() {
        let changes = h.format_changes();
        let pts = if i == 0 { Timestamp::ZERO } else { Timestamp::NONE };
        let ok = push(&mut h, u, pts, Timestamp::NONE);
        let bufs = h.drain_outputs();
        take(&mut h, bufs, changes);
        if !ok {
            return Audio { pcm, changes: u32::MAX, warned: true };
        }
    }
    let changes = h.format_changes();
    match h.eos() {
        Ok(bufs) => take(&mut h, bufs, changes),
        Err(_) => return Audio { pcm, changes: u32::MAX, warned: true },
    }
    if say() {
        eprintln!("libav {kind:?} pcm {} bytes, {} formats", pcm.len(), h.format_changes());
    }
    let warned = h
        .bus_messages()
        .iter()
        .any(|m| matches!(m, BusMessage::Warning { .. } | BusMessage::Error { .. }));
    Audio { pcm, changes: h.format_changes(), warned }
}

fn say() -> bool {
    static SAY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}
