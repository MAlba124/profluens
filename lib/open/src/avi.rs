use pf_avi::{AviError, MoviLocation, StreamKind, family_for, probe_header};

use crate::{Desc, OpenError, SkipWhy, TrackDesc, TrackKind, id_of};

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
