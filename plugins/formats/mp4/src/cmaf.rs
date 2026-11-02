//! CMAF tracks and segments (ISO/IEC 23000-19) over [`crate::writer`], one track per file. An
//! init segment is `ftyp` and a `moov` with an `mvex`, a media segment is `styp`, one `moof` and
//! its `mdat`. Pure functions over caller buffers. The sample entries and samples of elementary
//! streams are [`crate::es`].
//!
//! A segment's decode times come from its own samples alone, the presentation stamps sorted, so
//! the same samples give the same bytes whichever job cut them. A sample that presents before it
//! decodes gets a negative composition offset, a version 1 `trun`, which CMAF allows and which
//! needs no edit list.

use crate::writer::{self, Edit, Entry, Run, TrackHead, TrackMoov};

/// The one track of every CMAF file.
pub const TRACK_ID: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmafError {
    /// A segment with no samples.
    Empty,
}

impl std::fmt::Display for CmafError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("empty segment"),
        }
    }
}

impl std::error::Error for CmafError {}

/// The samples of one segment as columns, ns stamps, the data back to back in the form the
/// sample entry wants. Kept and cleared between segments, the columns keep their room.
#[derive(Debug, Default, Clone)]
pub struct Rows {
    pub pts: Vec<i64>,
    pub dur: Vec<i64>,
    pub sync: Vec<bool>,
    pub size: Vec<u32>,
    pub data: Vec<u8>,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.pts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pts.is_empty()
    }

    pub fn clear(&mut self) {
        self.pts.clear();
        self.dur.clear();
        self.sync.clear();
        self.size.clear();
        self.data.clear();
    }

    /// A sample whose bytes `fill` appends to `data`.
    pub fn push(&mut self, pts: i64, dur: i64, sync: bool, fill: impl FnOnce(&mut Vec<u8>)) {
        let at = self.data.len();
        fill(&mut self.data);
        self.pts.push(pts);
        self.dur.push(dur);
        self.sync.push(sync);
        self.size.push((self.data.len() - at) as u32);
    }
}

/// Scratch columns of the segment writer, media ticks.
#[derive(Debug, Default)]
pub struct Scratch {
    pts: Vec<i64>,
    dts: Vec<i64>,
    dur: Vec<u32>,
    cts: Vec<i64>,
}

pub fn video_head(entry: Entry, width: u32, height: u32) -> TrackHead {
    writer::video_head(TRACK_ID, entry, width, height)
}

pub fn audio_head(entry: Entry, rate: u32, channels: u16) -> TrackHead {
    writer::audio_head(TRACK_ID, entry, rate, channels)
}

/// `ftyp` and `moov` of a single track movie in fragments, into `out`, cleared first.
pub fn init(out: &mut Vec<u8>, head: &TrackHead) {
    out.clear();
    writer::ftyp(out, true);
    let t = TrackMoov { head, edit: Edit::default(), table: None, dts: &[], media_duration: 0 };
    writer::moov(out, &[t], true);
}

/// `styp`, the CMAF media segment brand (ISO/IEC 23000-19 section 7.3.2.2).
fn styp(out: &mut Vec<u8>) {
    out.extend_from_slice(&24u32.to_be_bytes());
    out.extend_from_slice(b"styp");
    out.extend_from_slice(b"cmfs");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"cmfs");
    out.extend_from_slice(b"msdh");
}

/// Media segment `seq` of `rows` into `out`, cleared first. `end` is where the next segment
/// starts, ns, for the last sample's duration, None at the end of the media.
pub fn segment(
    out: &mut Vec<u8>,
    seq: u32,
    timescale: u32,
    rows: &Rows,
    end: Option<i64>,
    s: &mut Scratch,
) -> Result<(), CmafError> {
    chunk(out, true, seq, timescale, rows, end, s)
}

/// A CMAF chunk, one fragment of a segment, into `out`, cleared first. The first of a segment
/// leads with its `styp`, the segment is its chunks back to back. `end` is where the next chunk
/// starts.
pub fn chunk(
    out: &mut Vec<u8>,
    first: bool,
    seq: u32,
    timescale: u32,
    rows: &Rows,
    end: Option<i64>,
    s: &mut Scratch,
) -> Result<(), CmafError> {
    let n = rows.len();
    if n == 0 {
        return Err(CmafError::Empty);
    }
    s.pts.clear();
    s.pts.extend(rows.pts.iter().map(|&p| writer::ticks(p, timescale)));
    s.dts.clear();
    s.dts.extend_from_slice(&s.pts);
    s.dts.sort_unstable();
    for i in 1..n {
        if s.dts[i] <= s.dts[i - 1] {
            s.dts[i] = s.dts[i - 1] + 1;
        }
    }
    s.dur.clear();
    for i in 0..n - 1 {
        s.dur.push((s.dts[i + 1] - s.dts[i]) as u32);
    }
    let last = match end.map(|e| writer::ticks(e, timescale) - s.dts[n - 1]) {
        Some(d) if d > 0 => d,
        _ => writer::ticks(rows.dur[n - 1], timescale)
            .max(s.dur.last().map_or(1, |&d| i64::from(d)))
            .max(1),
    };
    s.dur.push(last as u32);
    s.cts.clear();
    s.cts.extend(s.pts.iter().zip(&s.dts).map(|(p, d)| p - d));
    out.clear();
    if first {
        styp(out);
    }
    let run = Run {
        track: TRACK_ID,
        base_dts: s.dts[0].max(0) as u64,
        sizes: &rows.size,
        durations: &s.dur,
        cts: &s.cts,
        sync: &rows.sync,
    };
    writer::moof(out, seq, &[run]);
    out.extend_from_slice(&rows.data);
    Ok(())
}

#[cfg(test)]
mod tests;
