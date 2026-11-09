use std::convert::Infallible;

use profluens_core::fetch::{Fetch, Need, Step};

use crate::boxes::{self, BoxError, boxtype, fourcc, read_box_header};

/// Samples of a chapter track kept, the rest are left out.
pub const MAX_SAMPLES: usize = 1024;
/// Bytes read of a chapter sample, its length prefix and a title of any sane size.
pub const MAX_SAMPLE_READ: u32 = 2048;

const CHAP: [u8; 4] = fourcc(b"chap");
const CHPL: [u8; 4] = fourcc(b"chpl");
const TREF: [u8; 4] = fourcc(b"tref");

/// A `chpl` title and where its chapter starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChplEntry<'a> {
    pub start_ns: u64,
    pub title: &'a [u8],
}

/// The Nero chapters of a whole `moov` box, `udta/chpl`, in file order. A truncated list keeps
/// the entries before the cut.
// Cold path, once per item.
#[allow(clippy::disallowed_methods)]
pub fn chpl(moov: &[u8]) -> Vec<ChplEntry<'_>> {
    let mut out = Vec::new();
    let Some(body) = body_of(moov) else { return out };
    let Some(udta) = child(body, boxtype::UDTA) else { return out };
    let Some(b) = child(udta, CHPL) else { return out };
    let Some(&version) = b.first() else { return out };
    // version 1 has a reserved word before the count
    let mut at = if version == 0 { 4 } else { 8 };
    let Some(&count) = b.get(at) else { return out };
    at += 1;
    for _ in 0..count {
        let Some(start) = b.get(at..at + 8) else { break };
        let Some(&len) = b.get(at + 8) else { break };
        let from = at + 9;
        let Some(title) = b.get(from..from + usize::from(len)) else { break };
        let start = u64::from_be_bytes([
            start[0], start[1], start[2], start[3], start[4], start[5], start[6], start[7],
        ]);
        // 100 ns units
        out.push(ChplEntry { start_ns: start.saturating_mul(100), title });
        at = from + usize::from(len);
    }
    out
}

/// One sample of a chapter track, its span and the bytes of it to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChapterSample {
    pub start_ns: u64,
    pub end_ns: u64,
    pub at: u64,
    /// The sample's size, at most [`MAX_SAMPLE_READ`].
    pub len: u32,
}

/// A QuickTime chapter track, the text track a `tref/chap` names, one chapter per sample.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChapterTrack {
    pub track_id: u32,
    /// The `mdhd` ISO 639-2/T code, `und` for none.
    pub language: [u8; 3],
    pub samples: Vec<ChapterSample>,
    /// The samples' bytes back to back, `len` each, once [`TextAt`] read them. Bytes past the
    /// end of the file stay zero.
    pub bytes: Vec<u8>,
}

impl ChapterTrack {
    /// Each sample with its bytes, empty ones before [`TextAt`] read them.
    pub fn texts(&self) -> impl Iterator<Item = (&ChapterSample, &[u8])> {
        let mut from = 0usize;
        self.samples.iter().map(move |s| {
            let to = from + s.len as usize;
            let b = self.bytes.get(from..to).unwrap_or_default();
            from = to;
            (s, b)
        })
    }
}

fn body_of(boxed: &[u8]) -> Option<&[u8]> {
    let h = read_box_header(boxed, 0).ok()?;
    Some(h.body(boxed))
}

fn child(body: &[u8], kind: [u8; 4]) -> Option<&[u8]> {
    boxes::find_child(body, kind).ok().flatten().map(|h| h.body(body))
}

/// The `tkhd` id of a `trak` body and the ids its `tref/chap` names.
fn ids(trak: &[u8]) -> (Option<u32>, &[u8]) {
    let id = child(trak, boxtype::TKHD).and_then(|t| boxes::parse_tkhd(t).ok()).map(|t| t.track_id);
    let chap = child(trak, TREF).and_then(|t| child(t, CHAP)).unwrap_or_default();
    (id, chap)
}

/// The chapter track of a whole `moov` box, the first track a `tref/chap` names whose handler
/// is text or subtitles, image chapter tracks passed over. None when there is none or its
/// tables do not parse.
pub fn chapter_track(moov: &[u8]) -> Option<ChapterTrack> {
    let body = body_of(moov)?;
    let traks = || {
        let mut at = 0usize;
        std::iter::from_fn(move || {
            while at + 8 <= body.len() {
                let h = read_box_header(body, at).ok()?;
                at = h.end;
                if h.kind == boxtype::TRAK {
                    return Some(h.body(body));
                }
            }
            None
        })
    };
    let chap = traks().map(|t| ids(t).1).find(|c| !c.is_empty())?;
    chap.as_chunks::<4>().0.iter().find_map(|&id| {
        let id = u32::from_be_bytes(id);
        let trak = traks().find(|t| ids(t).0 == Some(id))?;
        text_track(trak, id).ok().flatten()
    })
}

// Cold path, once per item.
#[allow(clippy::disallowed_methods)]
fn text_track(trak: &[u8], track_id: u32) -> Result<Option<ChapterTrack>, BoxError> {
    let Some(mdia) = child(trak, boxtype::MDIA) else { return Ok(None) };
    let handler = child(mdia, boxtype::HDLR).and_then(|h| h.get(8..12)).unwrap_or_default();
    if handler != b"text" && handler != b"sbtl" {
        return Ok(None);
    }
    let Some(mdhd) = child(mdia, boxtype::MDHD) else { return Ok(None) };
    let mdhd = boxes::parse_mdhd(mdhd)?;
    let stbl = child(mdia, boxtype::MINF).and_then(|m| child(m, boxtype::STBL));
    let Some(stbl) = stbl.filter(|_| mdhd.timescale > 0) else { return Ok(None) };
    let need = |kind| child(stbl, kind).ok_or(BoxError::Malformed("a chapter table missing"));
    let stts = boxes::parse_stts(need(boxtype::STTS)?)?;
    let stsc = boxes::parse_stsc(need(boxtype::STSC)?)?;
    let sizes = match child(stbl, boxtype::STSZ) {
        Some(b) => boxes::parse_stsz(b)?,
        None => boxes::parse_stz2(need(boxtype::STZ2)?)?,
    };
    let offsets = match child(stbl, boxtype::STCO) {
        Some(b) => boxes::parse_chunk_offsets(b, false)?,
        None => boxes::parse_chunk_offsets(need(boxtype::CO64)?, true)?,
    };
    let ns = |t: u64| (u128::from(t) * 1_000_000_000 / u128::from(mdhd.timescale)) as u64;
    let mut samples = Vec::with_capacity(sizes.count().min(MAX_SAMPLES));
    let (mut run, mut left, mut t) = (0usize, stts.first().map_or(0, |e| e.count), 0u64);
    let mut entry = 0usize;
    let mut s = 0usize;
    'chunks: for (i, &chunk) in offsets.iter().enumerate() {
        let number = i as u64 + 1;
        while stsc.get(entry + 1).is_some_and(|e| u64::from(e.first_chunk) <= number) {
            entry += 1;
        }
        let per_chunk = stsc.get(entry).map_or(0, |e| e.samples_per_chunk);
        let mut at = chunk;
        for _ in 0..per_chunk {
            let Some(size) = sizes.get(s).filter(|_| samples.len() < MAX_SAMPLES) else {
                break 'chunks;
            };
            while left == 0 && run + 1 < stts.len() {
                run += 1;
                left = stts[run].count;
            }
            let delta = match left {
                0 => 0,
                _ => {
                    left -= 1;
                    u64::from(stts[run].delta)
                }
            };
            let end = t.saturating_add(delta);
            let len = size.min(MAX_SAMPLE_READ);
            samples.push(ChapterSample { start_ns: ns(t), end_ns: ns(end), at, len });
            (t, at, s) = (end, at.saturating_add(u64::from(size)), s + 1);
        }
    }
    let language = mdhd.language;
    Ok(Some(ChapterTrack { track_id, language, samples, bytes: Vec::new() }))
}

/// Reads the samples of a [`ChapterTrack`], one Need per run of adjacent samples.
#[derive(Debug, Default)]
pub struct TextAt {
    track: ChapterTrack,
    /// Each run's file offset, where it starts in the track's bytes and its length.
    runs: Vec<(u64, usize, usize)>,
    next: usize,
    asked: bool,
}

impl TextAt {
    // Cold path, once per item.
    #[allow(clippy::disallowed_methods)]
    pub fn new(mut track: ChapterTrack) -> TextAt {
        let mut runs: Vec<(u64, usize, usize)> = Vec::new();
        let mut from = 0usize;
        for s in &track.samples {
            let len = s.len as usize;
            match runs.last_mut() {
                Some(r) if r.0.checked_add(r.2 as u64) == Some(s.at) => r.2 += len,
                _ if len > 0 => runs.push((s.at, from, len)),
                _ => {}
            }
            from += len;
        }
        track.bytes = vec![0; from];
        TextAt { track, runs, next: 0, asked: false }
    }
}

impl Fetch for TextAt {
    type Out = ChapterTrack;
    type Err = Infallible;

    fn poll(&mut self) -> Result<Step<ChapterTrack>, Infallible> {
        if std::mem::take(&mut self.asked) {
            self.next += 1;
        }
        match self.runs.get(self.next) {
            Some(&(at, _, len)) => {
                self.asked = true;
                Ok(Step::Need(Need { at, len }))
            }
            None => Ok(Step::Done(std::mem::take(&mut self.track))),
        }
    }

    fn buf(&mut self) -> &mut [u8] {
        match self.runs.get(self.next) {
            Some(&(_, from, len)) => &mut self.track.bytes[from..from + len],
            None => &mut [],
        }
    }

    fn filled(&mut self, _n: usize) {}
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests;
