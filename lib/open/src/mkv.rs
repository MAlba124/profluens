// App-side setup, the Cues and the track list once per open.
#![allow(clippy::disallowed_methods)]

use pf_mkv::ebml::{id, read_element_header, read_vint};
use pf_mkv::reader::block_addition;
use pf_mkv::{DemuxError, Parsed, Track, is_audio_family, is_video_family, track_family};

use crate::{Desc, Hdr10Plus, OpenError, SkipWhy, TrackDesc, TrackKind, id_of, plus, ratio};

/// Top level elements walked for the first Cluster, as `pf_probe::head::MkvHeadAt` walks them.
const MAX_TOP: usize = 4096;
/// An element head at most, a 4 octet id and an 8 octet size.
const EL_HEAD: usize = 12;

/// Where the head walk passed the Tags, the Attachments and the Chapters, for a head cut before
/// them whose SeekHead names none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Passed {
    pub(crate) tags: Option<u64>,
    pub(crate) attachments: Option<u64>,
    pub(crate) chapters: Option<u64>,
}

/// The head [`pf_probe::head::MkvHeadAt`] reads when `window` holds every element before the
/// first Cluster, no more than `max` long. Its length, the first Cluster and what the walk passed,
/// the head ending before the Attachments once Tracks is read.
pub(crate) fn head_in(window: &[u8], max: u64) -> Option<(usize, u64, Passed)> {
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
    let (mut tracks, mut cut, mut passed) = (false, None, Passed::default());
    for _ in 0..MAX_TOP {
        let h = read_element_header(window, at).ok()?;
        if h.id == id::CLUSTER {
            let len = cut.unwrap_or(at);
            return (len as u64 <= max).then_some((len, at as u64, passed));
        }
        if h.id == id::ATTACHMENTS {
            passed.attachments = passed.attachments.or(Some(at as u64));
            cut = cut.or(tracks.then_some(at));
        }
        if h.id == id::TAGS {
            passed.tags = passed.tags.or(Some(at as u64));
        }
        if h.id == id::CHAPTERS {
            passed.chapters = passed.chapters.or(Some(at as u64));
        }
        tracks |= h.id == id::TRACKS;
        let end = h.data_end().ok()??;
        if end > window.len() || end <= at {
            return None;
        }
        at = end;
    }
    None
}

/// The DocType of the EBML header leading `head`, None where it states none or is cut.
pub(crate) fn doc_type(head: &[u8]) -> Option<&[u8]> {
    let ebml = read_element_header(head, 0).ok()?;
    if ebml.id != id::EBML {
        return None;
    }
    let end = ebml.data_end().ok()??.min(head.len());
    let mut at = ebml.data_start;
    while at < end {
        let h = read_element_header(head, at).ok()?;
        let next = h.data_end().ok()??;
        if h.id == id::DOC_TYPE {
            return head.get(h.data_start..next).map(pf_mkv::ebml::read_string);
        }
        at = next;
    }
    None
}

/// Where the Cues lie and the metadata the open reads, absolute, by the head's SeekHead and its
/// own elements.
pub(crate) fn places(p: &Parsed) -> (Option<u64>, Passed) {
    let ix = p.index;
    let passed = Passed {
        tags: ix.and_then(|ix| ix.tags),
        attachments: ix.and_then(|ix| ix.attachments),
        chapters: ix.and_then(|ix| ix.chapters),
    };
    (p.cues_pos(), passed)
}

/// The Cues at the start of `b`, the held bytes from where the SeekHead puts them, as
/// `pf_mkv::CuesAt` reads them. None when `b` holds less than that read and is not the end of the
/// file.
pub(crate) fn cues_in(b: &[u8], eof: bool, max: u64, p: &Parsed) -> Option<Vec<pf_mkv::Cue>> {
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
    let sh = p.seek_head?;
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
    // an HE-AAC track decodes at its output rate, twice its core rate, stated or signalled
    let output = t.output_sampling_frequency;
    out.rate = if output > 0.0 { output } else { t.sampling_frequency } as u32;
    if family == "aac"
        && output <= 0.0
        && let Some(asc) = profluens_audio::asc::parse(&t.codec_private).filter(|a| a.sbr)
    {
        out.rate = asc.out_rate;
    }
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

/// The walk of the first Cluster for [`TrackDesc::hdr10plus`], an element head at a time. A block
/// is read for its frame's lead and its BlockAdditions alone, the frame's bulk is jumped over.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Walk {
    /// The file offset of the next element head.
    at: u64,
    /// Where the Cluster ends, the file's end before its head is read or for an unknown size.
    end: u64,
    /// Whether the Cluster's head was read.
    inside: bool,
    /// The video tracks undecided, a bit by place among the first 64.
    left: u64,
    group: Option<Group>,
    reads: u8,
}

/// A BlockGroup being walked, its track once its Block was read.
#[derive(Clone, Copy, Debug)]
struct Group {
    end: u64,
    track: Option<usize>,
    stream: bool,
    /// The tracks whose T.35 BlockAddID an addition ahead of the Block carries HDR10+ at.
    added: u64,
}

/// Reads a walk makes at most, a first Cluster led by other tracks' blocks stops it.
const WALK_READS: u8 = 8;

impl Walk {
    pub(crate) fn new(first: u64, len: u64, tracks: &[TrackDesc]) -> Walk {
        let video = tracks.iter().take(64).enumerate().filter(|(_, t)| t.kind == TrackKind::Video);
        let left = video.fold(0u64, |m, (i, _)| m | 1 << i);
        Walk { at: first, end: len, inside: false, left, group: None, reads: 0 }
    }

    /// The bytes wanted from the next element head on, None when the walk is over.
    pub(crate) fn want(&self) -> Option<(u64, usize)> {
        (self.left != 0 && self.at < self.end && self.reads < WALK_READS)
            .then(|| (self.at, (self.end - self.at).min(plus::KEY_READ as u64) as usize))
    }

    /// Whether `b`, the bytes from the next element head on, `eof` when they run to the file's
    /// end, are enough for [`Walk::step`], the head, a block's lead or BlockAdditions whole.
    pub(crate) fn enough(&self, b: &[u8], eof: bool) -> bool {
        // a head that does not parse from a whole head's bytes never will
        let Ok(h) = read_element_header(b, 0) else { return eof || b.len() >= EL_HEAD };
        let Some(size) = h.size.filter(|_| self.inside) else { return true };
        let total = (h.data_start as u64).saturating_add(size).min(self.end - self.at);
        let need = match (h.id, self.group.is_some()) {
            (i, false) if i == id::SIMPLE_BLOCK => total.min(plus::LEAD as u64),
            (i, true) if i == id::BLOCK => total.min(plus::LEAD as u64),
            // a longer one never comes whole and is passed over
            (i, true) if i == id::BLOCK_ADDITIONS && total <= plus::KEY_READ as u64 => total,
            _ => 0,
        };
        eof || b.len() as u64 >= need
    }

    /// Counts a read the walk asked for.
    pub(crate) fn read(&mut self) {
        self.reads += 1;
    }

    /// One element from `b`, its bytes from [`Walk::want`]'s offset, short only at the file's end.
    pub(crate) fn step(&mut self, b: &[u8], p: &Parsed, tracks: &mut [TrackDesc]) {
        let stop = self.end;
        let Ok(h) = read_element_header(b, 0) else { return self.at = stop };
        let size = h.size.map(|s| (h.data_start as u64).saturating_add(s));
        let body = |n: u64| &b[h.data_start.min(b.len())..(n as usize).min(b.len())];
        if !self.inside {
            if h.id != id::CLUSTER {
                return self.at = stop;
            }
            self.inside = true;
            self.end = size.map_or(self.end, |n| self.at.saturating_add(n).min(self.end));
            self.at += h.data_start as u64;
            return;
        }
        // only a Cluster may be of unknown size, the next one ends this
        let Some(n) = size.filter(|_| h.id != id::CLUSTER) else { return self.at = stop };
        let next = self.at.saturating_add(n);
        match (h.id, self.group.as_mut()) {
            (i, None) if i == id::SIMPLE_BLOCK => {
                if let Some((t, stream)) = block_lead(body(n), p, tracks, self.left) {
                    tracks[t].hdr10plus = if stream { Hdr10Plus::Stream } else { Hdr10Plus::None };
                    self.left &= !(1 << t);
                }
            }
            (i, None) if i == id::BLOCK_GROUP => {
                let g = Group { end: next, track: None, stream: false, added: 0 };
                self.group = Some(g);
                self.at += h.data_start as u64;
                return;
            }
            (i, Some(g)) if i == id::BLOCK => match block_lead(body(n), p, tracks, self.left) {
                Some((t, stream)) => (g.track, g.stream) = (Some(t), stream),
                // another track's group, or one decided before
                None => {
                    self.at = g.end;
                    return self.group = None;
                }
            },
            (i, Some(g)) if i == id::BLOCK_ADDITIONS && n as usize <= b.len() => {
                g.added |= added(body(n), p, tracks, self.left);
            }
            _ => {}
        }
        self.at = next;
        if let Some(g) = self.group.filter(|g| self.at >= g.end) {
            self.close(g, tracks);
            self.at = g.end;
        }
    }

    /// Decides a BlockGroup still open when the walk ends, cut by the file's end or the reads.
    pub(crate) fn finish(&mut self, tracks: &mut [TrackDesc]) {
        if let Some(g) = self.group {
            self.close(g, tracks);
        }
    }

    fn close(&mut self, g: Group, tracks: &mut [TrackDesc]) {
        if let Some(t) = g.track {
            tracks[t].hdr10plus = match (g.stream, g.added & 1 << t != 0) {
                (true, _) => Hdr10Plus::Stream,
                (false, true) => Hdr10Plus::Addition,
                (false, false) => Hdr10Plus::None,
            };
            self.left &= !(1 << t);
        }
        self.group = None;
    }
}

/// The place of the undecided video track `block` belongs to and whether its frame's lead, the
/// bytes of it in `block`, carries HDR10+. A laced or coded frame is not read.
fn block_lead(block: &[u8], p: &Parsed, tracks: &[TrackDesc], left: u64) -> Option<(usize, bool)> {
    let (number, n, _) = read_vint(block, 0).ok()?;
    let flags = *block.get(n + 2)?;
    let i = tracks.iter().take(64).position(|t| u64::from(t.id) == number)?;
    if left & 1 << i == 0 {
        return None;
    }
    let mt = p.tracks.iter().find(|t| t.track_number == number);
    let readable = flags & 0x06 == 0 && mt.is_some_and(|m| !m.coding.transforms());
    let frame = block.get(n + 3..).filter(|_| readable).unwrap_or_default();
    let t = &tracks[i];
    Some((i, plus::in_unit(t.family, t.nal_length, frame)))
}

/// The undecided video tracks whose T.35 BlockAddID `additions` carries HDR10+ at, a bit by place.
fn added(additions: &[u8], p: &Parsed, tracks: &[TrackDesc], left: u64) -> u64 {
    let mut out = 0;
    for (i, t) in tracks.iter().take(64).enumerate().filter(|&(i, _)| left & 1 << i != 0) {
        let Some(mt) = p.tracks.iter().find(|m| m.track_number == u64::from(t.id)) else {
            continue;
        };
        if block_addition(additions, mt.t35_add_id).is_some_and(|r| plus::is_plus(&additions[r])) {
            out |= 1 << i;
        }
    }
    out
}

/// The head parsed once for every run's demuxer.
pub(crate) fn parse(head: &[u8]) -> Result<Parsed, OpenError> {
    Parsed::of(head).map_err(|e| match e {
        DemuxError::TracksIncomplete => OpenError::NoStreams,
        _ => OpenError::Malformed { at: 0 },
    })
}

/// The tracks and the duration of the head, the TrackUID of each track described.
pub(crate) fn describe(p: &Parsed, d: &mut Desc) -> Result<(u64, Vec<u64>), OpenError> {
    let mut uids = Vec::with_capacity(p.tracks.len());
    for t in &p.tracks {
        let n = d.tracks.len();
        track(t, d);
        if d.tracks.len() > n {
            uids.push(t.uid);
        }
    }
    if d.tracks.is_empty() {
        return Err(OpenError::NoStreams);
    }
    Ok((p.duration_ns.unwrap_or(0), uids))
}
