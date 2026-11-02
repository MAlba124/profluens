// App-side setup, the Cues and the track list once per open.
#![allow(clippy::disallowed_methods)]

use pf_mkv::ebml::{id, read_element_header};
use pf_mkv::reader::MatroskaReader;
use pf_mkv::{Track, is_audio_family, is_video_family, track_family};

use crate::{Desc, OpenError, SkipWhy, TrackDesc, TrackKind, id_of, ratio};

/// Top level elements walked for the first Cluster, as `pf_probe::head::MkvHeadAt` walks them.
const MAX_TOP: usize = 4096;
/// An element head at most, a 4 octet id and an 8 octet size.
const EL_HEAD: usize = 12;

/// The head's length and the first Cluster when `window` holds every element before it, the
/// head [`pf_probe::head::MkvHeadAt`] reads then, no more than `max` long.
pub(crate) fn head_in(window: &[u8], max: u64) -> Option<usize> {
    let ebml = read_element_header(window, 0).ok()?;
    if ebml.id != id::EBML {
        return None;
    }
    let seg_at = ebml.data_end().ok()??;
    let seg = read_element_header(window, seg_at).ok()?;
    if seg.id != id::SEGMENT {
        return None;
    }
    let mut at = seg.data_start;
    for _ in 0..MAX_TOP {
        let h = read_element_header(window, at).ok()?;
        if h.id == id::CLUSTER {
            return (at as u64 <= max).then_some(at);
        }
        let end = h.data_end().ok()??;
        if end > window.len() || end <= at {
            return None;
        }
        at = end;
    }
    None
}

/// Where the Cues and the Tags of `head` lie, absolute, by its SeekHead and its own elements.
pub(crate) fn places(head: &[u8]) -> (Option<u64>, Option<u64>, Option<u64>) {
    let cues = pf_mkv::parse_seek_head(head)
        .and_then(|sh| sh.cues_pos?.checked_add(sh.segment_data_start));
    let ix = pf_mkv::tags::index_segment(head);
    (cues, ix.as_ref().and_then(|ix| ix.tags), ix.and_then(|ix| ix.attachments))
}

/// The Cues at the start of `b`, the held bytes from where the SeekHead puts them, as
/// `pf_mkv::CuesAt` reads them. None when `b` holds less than that read and is not the end of the
/// file.
pub(crate) fn cues_in(b: &[u8], eof: bool, max: u64, head: &[u8]) -> Option<Vec<pf_mkv::Cue>> {
    let hd = &b[..b.len().min(EL_HEAD)];
    if hd.len() < EL_HEAD && !eof {
        return None;
    }
    let Ok(h) = read_element_header(hd, 0) else { return Some(Vec::new()) };
    let len = h.size.and_then(|s| (h.data_start as u64).checked_add(s));
    let Some(len) = len.filter(|&len| h.id == id::CUES && len <= max) else {
        return Some(Vec::new());
    };
    let len = usize::try_from(len).ok()?;
    if b.len() < len && !eof {
        return None;
    }
    let sh = pf_mkv::parse_seek_head(head)?;
    let body = &b[..len.min(b.len())];
    Some(pf_mkv::parse_cue_tracks(body, sh.segment_data_start, sh.timestamp_scale))
}

fn kind_of(family: &str) -> TrackKind {
    match family {
        f if is_video_family(f) => TrackKind::Video,
        f if is_audio_family(f) => TrackKind::Audio,
        f if f.starts_with("subtitle/") => TrackKind::Text,
        _ => TrackKind::Other,
    }
}

fn track(t: &Track, d: &mut Desc) {
    let family = track_family(t);
    let Some(id) = id_of(t.track_number) else {
        return d.skip(t.track_number, family, SkipWhy::TooLarge);
    };
    let kind = kind_of(family);
    let mut out = TrackDesc::new(id, kind, family);
    match family {
        "h264/annexb" | "h265/annexb" => {
            match pf_mkv::nal_head_from_config(&t.codec_private, family == "h265/annexb") {
                Ok((head, n)) => (out.head, out.nal_length) = (d.put(&head), n as u8),
                Err(_) => return d.skip(t.track_number, family, SkipWhy::BadHead),
            }
        }
        _ => out.head = d.put(&t.codec_private),
    }
    out.language = d.put(t.language.as_bytes());
    out.title = d.put(t.name.as_bytes());
    out.default = t.flag_default;
    out.forced = t.flag_forced;
    out.channels = t.channels.min(u32::from(u16::MAX)) as u16;
    out.rate = t.sampling_frequency as u32;
    out.depth = match kind {
        TrackKind::Video => pf_mkv::config_depth(family, &t.codec_private),
        _ => t.bit_depth.min(255) as u8,
    };
    (out.width, out.height) = (t.pixel_width, t.pixel_height);
    if kind == TrackKind::Video && t.display_width > 0 && t.display_height > 0 {
        // pixels or an aspect ratio, either is the display shape
        let (w, h) = (u64::from(t.pixel_width), u64::from(t.pixel_height));
        out.par = ratio(u64::from(t.display_width) * h, u64::from(t.display_height) * w);
    }
    out.rotation = t.rotation;
    out.frame_ns = t.default_duration_ns.unwrap_or(0);
    out.bitrate = t.bitrate;
    out.delay_ns = t.codec_delay_ns;
    out.seek_preroll_ns = t.seek_pre_roll_ns;
    out.colour = pf_mkv::track_colour(t);
    d.push(out);
}

/// The tracks and the duration of the head, the TrackUID of each track described.
pub(crate) fn describe(head: &[u8], d: &mut Desc) -> Result<(u64, Vec<u64>), OpenError> {
    let mut r = MatroskaReader::new();
    r.push(head).map_err(|_| OpenError::Malformed { at: 0 })?;
    if !r.tracks_ready() {
        return Err(OpenError::NoStreams);
    }
    let mut uids = Vec::with_capacity(r.tracks().len());
    for t in r.tracks() {
        let n = d.tracks.len();
        track(t, d);
        if d.tracks.len() > n {
            uids.push(t.uid);
        }
    }
    if d.tracks.is_empty() {
        return Err(OpenError::NoStreams);
    }
    Ok((r.duration_ns().unwrap_or(0), uids))
}
