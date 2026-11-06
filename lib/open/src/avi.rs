use pf_avi::{
    AviError, MoviLocation, StreamKind, family_for, first_chunk_of, probe_header, vol_prefix,
};

use crate::{Desc, OpenError, SkipWhy, TrackDesc, TrackKind, id_of};

/// Bytes read past `movi` for the VOL of an MPEG-4 stream whose `strf` carries none, the
/// first video chunk's start lies within them in a file XviD or ffmpeg wrote.
pub(crate) const VOL_WINDOW: u64 = 64 << 10;

/// Whether a described MPEG-4 Part 2 track has no VOL in its head, XviD files keep it in the
/// first key chunk only.
pub(crate) fn wants_vol(d: &Desc) -> bool {
    d.tracks.iter().any(|t| t.family == "mpeg4/asp" && vol_prefix(head_of(d, t)).is_none())
}

fn head_of<'a>(d: &'a Desc, t: &TrackDesc) -> &'a [u8] {
    d.arena.get(t.head.range()).unwrap_or_default()
}

/// The VOL of each such track from `movi`, the bytes behind the list's four-CC, as its head.
/// Whether every one was found or ruled out, a chunk cut by the slice's end decides nothing.
pub(crate) fn vol_from_movi(movi: &[u8], d: &mut Desc) -> bool {
    let mut settled = true;
    for i in 0..d.tracks.len() {
        let t = &d.tracks[i];
        if t.family != "mpeg4/asp" || vol_prefix(head_of(d, t)).is_some() {
            continue;
        }
        let Some(chunk) = first_chunk_of(movi, t.stream as usize) else {
            settled = false;
            continue;
        };
        match vol_prefix(chunk) {
            Some(vol) => {
                let head = d.put(vol);
                d.tracks[i].head = head;
            }
            None => settled = false,
        }
    }
    settled
}

/// The head's length through the `movi` four-CC when `window` holds it, the head
/// [`pf_probe::head::AviHeadAt`] reads then.
pub(crate) fn movi_in(window: &[u8], max: usize) -> Option<usize> {
    let window = &window[..window.len().min(max)];
    window.windows(4).position(|w| w == b"movi").map(|p| p + 4)
}

/// The streams of the head through `movi`, the duration `avih` and `strh` state and where the
/// `movi` list lies.
pub(crate) fn describe(
    head: &[u8],
    d: &mut Desc,
) -> Result<(u64, Option<MoviLocation>), OpenError> {
    let (h, movi) = probe_header(head).map_err(|e| match e {
        AviError::NotAvi => OpenError::NoStreams,
        _ => OpenError::Malformed { at: 0 },
    })?;
    for s in &h.streams {
        let family = family_for(s);
        let Some(id) = id_of(s.index as u64 + 1) else {
            d.skip(s.index as u64 + 1, family, SkipWhy::TooLarge);
            continue;
        };
        let kind = match s.kind {
            StreamKind::Video => TrackKind::Video,
            StreamKind::Audio => TrackKind::Audio,
            StreamKind::Other => TrackKind::Other,
        };
        let mut out = TrackDesc::new(id, kind, family);
        out.stream = s.index as u32;
        out.head = d.put(s.codec_head());
        out.title = d.put(s.name.as_bytes());
        out.channels = s.channels;
        if kind == TrackKind::Audio {
            out.rate = s.samples_per_sec;
            out.depth = s.bits_per_sample.min(255) as u8;
            out.bitrate = u64::from(s.avg_bytes_per_sec) * 8;
        }
        (out.width, out.height) = (s.width, s.height);
        if kind == TrackKind::Video {
            out.frame_ns = s.sample_duration_ns().unwrap_or(0);
        }
        d.push(out);
    }
    if d.tracks.is_empty() {
        return Err(OpenError::NoStreams);
    }
    Ok((h.duration_ns().unwrap_or(0), movi))
}
