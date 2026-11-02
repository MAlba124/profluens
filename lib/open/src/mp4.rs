use pf_mp4::{Mp4Reader, Reframer, TrackKind as Mp4Kind};

use crate::{Brand, Desc, OpenError, TrackDesc, TrackKind};

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

/// The tracks of the head through `moov` and the longest track's duration.
pub(crate) fn describe(head: &[u8], d: &mut Desc) -> Result<u64, OpenError> {
    let r = Mp4Reader::new(head).map_err(OpenError::Mp4)?;
    let mut duration = 0;
    for t in r.tracks() {
        let family = t.family();
        let kind = match t.kind() {
            Mp4Kind::Video => TrackKind::Video,
            Mp4Kind::Audio => TrackKind::Audio,
            _ if family.starts_with("subtitle/") => TrackKind::Text,
            _ => TrackKind::Other,
        };
        let e = &t.entry;
        let mut out = TrackDesc::new(t.track_id, kind, family);
        out.head = d.put(t.codec_head());
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
        out.rotation = t.rotation;
        out.bitrate = t.bitrate;
        out.delay_ns = ns(t.delay_ticks(), t.timescale);
        out.seek_preroll_ns = if family == "opus" { OPUS_PREROLL_NS } else { 0 };
        out.colour = e.colour;
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
