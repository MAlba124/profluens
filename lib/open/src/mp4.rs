use std::sync::Arc;

use pf_mp4::frag::{self, Tail};
use pf_mp4::{Limits, Reframer, Tables, Track, TrackKind as Mp4Kind};

use crate::plus::{KEY_READ, Key};
use crate::{Brand, Desc, Index, OpenError, TrackDesc, TrackKind, Want};

/// RFC 7845 section 4.6 asks Opus for 80 ms of decoding ahead of a seek target, the Opus in
/// ISOBMFF mapping states the same roll.
pub(crate) const OPUS_PREROLL_NS: u64 = 80_000_000;

fn ns(ticks: i64, timescale: u32) -> u64 {
    if ticks <= 0 || timescale == 0 {
        return 0;
    }
    (u128::from(ticks as u64) * 1_000_000_000 / u128::from(timescale)).min(u128::from(u64::MAX))
        as u64
}

/// The tracks of the head through `moov`, the movie's duration, and with [`Want::index`] the
/// sample tables folded once for every run's demuxer, held to a file of `len` bytes. A
/// fragmented movie's fragments and length come from its `sidx` and the `tail` too. With
/// [`Want::hdr10plus`] the first sync sample of each video track goes to `keys`.
pub(crate) fn describe(
    head: &[u8],
    len: u64,
    want: Want,
    tail: &Tail,
    d: &mut Desc,
    keys: &mut Vec<Key>,
) -> Result<(u64, Index), OpenError> {
    // the file's length bounds the index a small hostile table can declare
    let limits = Limits { file_len: Some(len), ..Limits::default() };
    // a fragmented moov holds few samples, folded for the length its fragments state
    if !want.index && frag::length_stated(head).is_none() {
        // folded for the key samples alone, a table the fold refuses leaves them unread
        let tables = want.hdr10plus.then(|| Tables::resolve(head, &tail.mfra, limits).ok());
        let Some(tables) = tables.flatten() else {
            let tracks = Tables::tracks_of(head).map_err(OpenError::Mp4)?;
            return Ok((tracks_into(&tracks, d)?, Index::None));
        };
        let first = d.tracks.len();
        let duration = tracks_into(tables.tracks(), d)?;
        first_keys(&tables, len, first, d, keys);
        return Ok((duration, Index::None));
    }
    let mut tables = Tables::resolve(head, &tail.mfra, limits).map_err(OpenError::Mp4)?;
    tables.last_fragment(&tail.moof);
    tables.sidx_chain(tail, len);
    let first = d.tracks.len();
    let duration = tracks_into(tables.tracks(), d)?.max(tables.fragmented_duration_ns());
    if want.hdr10plus {
        first_keys(&tables, len, first, d, keys);
    }
    Ok((duration, if want.index { Index::Mp4(Arc::new(tables)) } else { Index::None }))
}

/// The first sync sample of each H.264, H.265 or AV1 track, the tracks described from `first` in
/// the tables' order. A fragmented track's samples are not in the head and give none.
fn first_keys(tables: &Tables, len: u64, first: usize, d: &Desc, keys: &mut Vec<Key>) {
    for (i, t) in d.tracks.iter().enumerate().skip(first) {
        if t.kind != TrackKind::Video || !matches!(t.family, "h264/annexb" | "h265/annexb" | "av1")
        {
            continue;
        }
        let Ok(ti) = u16::try_from(i - first) else { break };
        let s = tables.samples().iter().find(|s| s.track_index == ti && s.sync);
        if let Some(s) = s.filter(|s| s.size > 0 && s.offset < len) {
            let n = u64::from(s.size).min(len - s.offset).min(KEY_READ as u64) as usize;
            keys.push(Key { track: i, at: s.offset, len: n });
        }
    }
}

fn tracks_into(tracks: &[Track], d: &mut Desc) -> Result<u64, OpenError> {
    let mut duration = 0;
    for t in tracks {
        let family = t.family();
        let kind = match t.kind() {
            Mp4Kind::Video => TrackKind::Video,
            Mp4Kind::Audio => TrackKind::Audio,
            _ if family.starts_with("subtitle/") => TrackKind::Text,
            _ => TrackKind::Other,
        };
        let e = &t.entry;
        let mut out = TrackDesc::new(t.track_id, kind, family);
        // AV1 and Opus send no head, their av1C and OpusHead describe them as Matroska does
        out.head = match family {
            "av1" | "opus" if t.codec_head().is_empty() => d.put(&e.config_record),
            _ => d.put(t.codec_head()),
        };
        out.language = match t.language_tag.is_empty() {
            true => d.put(std::str::from_utf8(&t.language).unwrap_or_default().as_bytes()),
            false => d.put(t.language_tag.as_bytes()),
        };
        out.title = d.put(t.title.as_bytes());
        // ffmpeg marks the default track of each kind enabled, as the demuxer takes it
        out.default = t.enabled;
        out.forced = t.forced;
        out.channels = e.channels.min(u32::from(u16::MAX)) as u16;
        out.rate = e.sample_rate;
        out.depth = match kind {
            TrackKind::Video => match family {
                "h265/annexb" | "av1" => pf_mkv::config_depth(family, &e.config_record),
                _ => 0,
            },
            _ => e.bit_depth.min(255) as u8,
        };
        (out.width, out.height) = (e.width, e.height);
        if e.par.0 > 0 && e.par.1 > 0 {
            out.par = crate::ratio(u64::from(e.par.0), u64::from(e.par.1));
        }
        out.rotation = t.rotation;
        if kind == TrackKind::Video {
            out.frame_ns = ns(i64::from(t.frame_ticks), t.timescale);
        }
        out.bitrate = t.bitrate;
        out.delay_ns = ns(t.delay_ticks(), t.timescale);
        out.seek_preroll_ns = if family == "opus" { OPUS_PREROLL_NS } else { 0 };
        out.colour = e.colour;
        if family == "av1" {
            out.chroma = crate::sps::av1c_chroma(&e.config_record);
        }
        if let Reframer::Nal { length_size } = e.reframer {
            out.nal_length = length_size.min(255) as u8;
        }
        out.duration_ns = t.duration_ns;
        duration = duration.max(t.duration_ns);
        d.push(out);
    }
    if d.tracks.is_empty() {
        return Err(OpenError::NoStreams);
    }
    Ok(duration)
}

/// The class a single `ftyp` brand names, ISO/IEC 14496-12 section 4.3 and the MP4 registration
/// authority's list.
fn class(b: &[u8]) -> Option<Brand> {
    match b {
        b"qt  " => Some(Brand::QuickTime),
        b"M4A " | b"M4B " | b"M4P " => Some(Brand::M4a),
        b"mp41" | b"mp42" | b"mp71" | b"avc1" | b"dash" | b"M4V " | b"M4VH" | b"M4VP" => {
            Some(Brand::Mp4)
        }
        b if b.starts_with(b"iso") => Some(Brand::Mp4),
        _ => None,
    }
}

/// The class of the `ftyp` leading `head`, the major brand's, else the first compatible brand
/// that names one, else Mp4.
pub(crate) fn brand(head: &[u8]) -> Brand {
    let Some(hdr) = head.get(..8).filter(|h| &h[4..8] == b"ftyp") else { return Brand::Mp4 };
    let size = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
    let Some(body) = head.get(8..size.max(8).min(head.len())) else { return Brand::Mp4 };
    let major = body.get(..4).and_then(class);
    // the minor version sits between the major brand and the compatible ones
    let compatible =
        body.get(8..).unwrap_or_default().as_chunks::<4>().0.iter().find_map(|b| class(b));
    major.or(compatible).unwrap_or(Brand::Mp4)
}

/// Top level boxes walked for the `moov` and the `mdat`, as the head machine walks them.
const MAX_BOXES: usize = 256;

/// The head's length when `window` holds a `moov` ahead of the `mdat`, the bytes up to the
/// `mdat` the head machine reads then, in a file of `len` bytes.
pub(crate) fn head_in(window: &[u8], len: u64) -> Option<usize> {
    let mut at = 0usize;
    let mut moov = false;
    for _ in 0..MAX_BOXES {
        let hdr = window.get(at..at.checked_add(8)?)?;
        let (header, size) = match u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) {
            0 => (8, len.checked_sub(at as u64)?),
            1 => {
                let large = window.get(at + 8..at + 16)?;
                let mut b = [0u8; 8];
                b.copy_from_slice(large);
                (16, u64::from_be_bytes(b))
            }
            n => (8, u64::from(n)),
        };
        if size < header {
            return None;
        }
        match &hdr[4..8] {
            b"mdat" => return moov.then_some(at),
            b"moov" if size > pf_probe::head::MAX_MOOV => return None,
            b"moov" => moov = true,
            _ => {}
        }
        at = at.checked_add(usize::try_from(size).ok()?)?;
        if at as u64 > len {
            return None;
        }
    }
    None
}
