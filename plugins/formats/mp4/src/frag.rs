//! Movie fragments (ISO/IEC 14496-12 8.8), `sidx` (8.16.3) and `mfra` (8.8.9).
//! A `trun` can declare four billion samples in twenty octets, so rows are bounded by the `mdat`
//! octets and [`MAX_FRAGMENT_SAMPLES`].

use std::convert::Infallible;

use profluens_core::fetch::{Fetch, Need, Step};

use crate::boxes::{self, BoxError, Cursor, boxtype, read_full_box_head};
use crate::reader::{Mp4Error, Sample, Tally, Track, ticks_to_ns};

pub const MAX_FRAGMENT_SAMPLES: usize = 1 << 20;

pub const MAX_FRAGMENT_HEAD: u64 = 16 << 20;

const EMPTY_ROWS: u32 = 4096;

/// `sample_is_non_sync_sample` (8.8.3.1).
const NON_SYNC: u32 = 0x0001_0000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trex {
    pub track_id: u32,
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
}

pub fn parse_trex(body: &[u8]) -> Result<Trex, BoxError> {
    let mut r = Cursor::new(body);
    read_full_box_head(&mut r)?;
    let track_id = r.u32("trex track_ID")?;
    r.skip(4, "trex default_sample_description_index")?;
    let duration = r.u32("trex default_sample_duration")?;
    let size = r.u32("trex default_sample_size")?;
    let flags = r.u32("trex default_sample_flags")?;
    Ok(Trex { track_id, duration, size, flags })
}

pub fn parse_mehd(body: &[u8]) -> Result<u64, BoxError> {
    let mut r = Cursor::new(body);
    match read_full_box_head(&mut r)?.version {
        0 => Ok(u64::from(r.u32("mehd fragment_duration")?)),
        1 => r.u64("mehd fragment_duration"),
        _ => Err(BoxError::Malformed("unsupported mehd version")),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tfhd {
    pub track_id: u32,
    pub base_data_offset: Option<u64>,
    pub duration: Option<u32>,
    pub size: Option<u32>,
    pub flags: Option<u32>,
    pub empty: bool,
    pub base_is_moof: bool,
}

pub fn parse_tfhd(body: &[u8]) -> Result<Tfhd, BoxError> {
    let mut r = Cursor::new(body);
    let flags = read_full_box_head(&mut r)?.flags;
    let track_id = r.u32("tfhd track_ID")?;
    let mut out = Tfhd {
        track_id,
        empty: flags & 0x01_0000 != 0,
        base_is_moof: flags & 0x02_0000 != 0,
        ..Tfhd::default()
    };
    if flags & 0x01 != 0 {
        out.base_data_offset = Some(r.u64("tfhd base_data_offset")?);
    }
    if flags & 0x02 != 0 {
        r.skip(4, "tfhd sample_description_index")?;
    }
    if flags & 0x08 != 0 {
        out.duration = Some(r.u32("tfhd default_sample_duration")?);
    }
    if flags & 0x10 != 0 {
        out.size = Some(r.u32("tfhd default_sample_size")?);
    }
    if flags & 0x20 != 0 {
        out.flags = Some(r.u32("tfhd default_sample_flags")?);
    }
    Ok(out)
}

pub fn parse_tfdt(body: &[u8]) -> Result<u64, BoxError> {
    let mut r = Cursor::new(body);
    match read_full_box_head(&mut r)?.version {
        0 => Ok(u64::from(r.u32("tfdt baseMediaDecodeTime")?)),
        1 => r.u64("tfdt baseMediaDecodeTime"),
        _ => Err(BoxError::Malformed("unsupported tfdt version")),
    }
}

pub struct Trun<'a> {
    rows: Cursor<'a>,
    version: u8,
    flags: u32,
    pub count: u32,
    pub data_offset: Option<i32>,
    pub first_flags: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrunRow {
    pub duration: Option<u32>,
    pub size: Option<u32>,
    pub flags: Option<u32>,
    pub offset: i64,
}

pub fn parse_trun(body: &[u8]) -> Result<Trun<'_>, BoxError> {
    let mut r = Cursor::new(body);
    let head = read_full_box_head(&mut r)?;
    let count = r.u32("trun sample_count")?;
    let data_offset = match head.flags & 0x01 != 0 {
        true => Some(r.i32("trun data_offset")?),
        false => None,
    };
    let first_flags = match head.flags & 0x04 != 0 {
        true => Some(r.u32("trun first_sample_flags")?),
        false => None,
    };
    Ok(Trun { rows: r, version: head.version, flags: head.flags, count, data_offset, first_flags })
}

impl Trun<'_> {
    pub fn row(&mut self) -> Result<TrunRow, BoxError> {
        let mut row = TrunRow::default();
        if self.flags & 0x100 != 0 {
            row.duration = Some(self.rows.u32("trun sample_duration")?);
        }
        if self.flags & 0x200 != 0 {
            row.size = Some(self.rows.u32("trun sample_size")?);
        }
        if self.flags & 0x400 != 0 {
            row.flags = Some(self.rows.u32("trun sample_flags")?);
        }
        if self.flags & 0x800 != 0 {
            // Unsigned in version 0 and signed from version 1 on.
            row.offset = match self.version {
                0 => i64::from(self.rows.u32("trun sample_composition_time_offset")?),
                _ => i64::from(self.rows.i32("trun sample_composition_time_offset")?),
            };
        }
        Ok(row)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sidx {
    /// The track_ID of the stream the index times.
    pub reference_id: u32,
    pub timescale: u32,
    pub earliest: u64,
    pub first_offset: u64,
    pub refs: Vec<SidxRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidxRef {
    pub index: bool,
    pub size: u32,
    pub duration: u32,
    pub starts_with_sap: bool,
}

// Cold path, once per index.
#[allow(clippy::disallowed_methods)]
pub fn parse_sidx(body: &[u8]) -> Result<Sidx, BoxError> {
    let mut r = Cursor::new(body);
    let version = read_full_box_head(&mut r)?.version;
    let reference_id = r.u32("sidx reference_ID")?;
    let timescale = r.u32("sidx timescale")?;
    let (earliest, first_offset) = match version {
        0 => (
            u64::from(r.u32("sidx earliest_presentation_time")?),
            u64::from(r.u32("sidx first_offset")?),
        ),
        1 => (r.u64("sidx earliest_presentation_time")?, r.u64("sidx first_offset")?),
        _ => return Err(BoxError::Malformed("unsupported sidx version")),
    };
    r.skip(2, "sidx reserved")?;
    let count = usize::from(r.u16("sidx reference_count")?);
    let mut refs = Vec::with_capacity(r.rows(count, 12));
    for _ in 0..count {
        let reference = r.u32("sidx reference")?;
        let duration = r.u32("sidx subsegment_duration")?;
        let sap = r.u32("sidx SAP")?;
        refs.push(SidxRef {
            index: reference >> 31 != 0,
            size: reference & 0x7FFF_FFFF,
            duration,
            starts_with_sap: sap >> 31 != 0,
        });
    }
    Ok(Sidx { reference_id, timescale, earliest, first_offset, refs })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FragmentRef {
    pub time_ns: u64,
    pub offset: u64,
}

/// The fragments a `sidx` anchored at `anchor` indexes, stamped as `track`'s samples are.
pub fn sidx_fragments(sidx: &Sidx, anchor: u64, track: Option<&Track>, out: &mut Vec<FragmentRef>) {
    let (mut time, mut offset) = (sidx.earliest, anchor.saturating_add(sidx.first_offset));
    for r in &sidx.refs {
        if !r.index {
            out.push(FragmentRef { time_ns: sidx_ns(sidx, track, time), offset });
        }
        time = time.saturating_add(u64::from(r.duration));
        offset = offset.saturating_add(u64::from(r.size));
    }
}

/// `ticks` of a `sidx` in ns, on the timeline of `track` where it is the stream the index times
/// in its own timescale, its edit's media time taken off as a fragment's samples have it.
pub fn sidx_ns(sidx: &Sidx, track: Option<&Track>, ticks: u64) -> u64 {
    let t = ticks.min(i64::MAX as u64) as i64;
    let named = |tr: &&Track| tr.track_id == sidx.reference_id && tr.timescale == sidx.timescale;
    match track.filter(named) {
        Some(tr) => tr.stamp_ns(t.saturating_sub(tr.media_time)),
        None => ticks_to_ns(t, sidx.timescale),
    }
}

/// The end of a `sidx`'s last reference in its ticks.
pub fn sidx_end(sidx: &Sidx) -> u64 {
    sidx.refs.iter().fold(sidx.earliest, |t, r| t.saturating_add(u64::from(r.duration)))
}

// Cold path, once per index.
#[allow(clippy::disallowed_methods)]
pub fn parse_tfra(body: &[u8]) -> Result<(u32, Vec<(u64, u64)>), BoxError> {
    let mut r = Cursor::new(body);
    let version = read_full_box_head(&mut r)?.version;
    let track_id = r.u32("tfra track_ID")?;
    let lengths = r.u32("tfra lengths")?;
    // The three numbers behind each row are one to four octets each.
    let tail = (((lengths >> 4) & 3) + ((lengths >> 2) & 3) + (lengths & 3) + 3) as usize;
    let count = r.u32("tfra number_of_entry")? as usize;
    let width = if version == 1 { 16 } else { 8 } + tail;
    let mut rows = Vec::with_capacity(r.rows(count, width));
    for _ in 0..count {
        let row = match version {
            0 => (u64::from(r.u32("tfra time")?), u64::from(r.u32("tfra moof_offset")?)),
            1 => (r.u64("tfra time")?, r.u64("tfra moof_offset")?),
            _ => return Err(BoxError::Malformed("unsupported tfra version")),
        };
        r.skip(tail, "tfra numbers")?;
        rows.push(row);
    }
    Ok((track_id, rows))
}

/// The `mfra` size from the closing `mfro` (8.8.11) in the file's last octets.
pub fn mfra_size(tail: &[u8]) -> Option<u64> {
    let mfro = tail.len().checked_sub(16).and_then(|at| tail.get(at..))?;
    if mfro[..8] != [0, 0, 0, 16, b'm', b'f', b'r', b'o'] {
        return None;
    }
    Some(u64::from(u32::from_be_bytes([mfro[12], mfro[13], mfro[14], mfro[15]])))
}

pub struct Movie<'a> {
    pub tracks: &'a [Track],
    pub trex: &'a [Trex],
}

#[derive(Clone, Copy)]
pub struct Place {
    pub moof: u64,
    pub mdat: (u64, u64),
}

pub fn expand(
    moof: &[u8],
    movie: &Movie<'_>,
    place: Place,
    next_dts: &mut [u64],
    out: &mut Vec<Sample>,
) -> Result<Option<u64>, Mp4Error> {
    // A sample holds an octet at least, and they are disjoint.
    let room = place.mdat.1.saturating_sub(place.mdat.0).min(MAX_FRAGMENT_SAMPLES as u64) as usize;
    let mut first: Option<u64> = None;
    let mut data_end = place.moof;
    let (mut at, mut trafs) = (0usize, 0usize);
    // One budget for the whole moof, a run of 16 octet truns is as cheap as one.
    let mut empty = 0u32;
    while at + 8 <= moof.len() {
        let h = boxes::read_box_header(moof, at)?;
        at = h.end;
        if h.kind != boxtype::TRAF {
            continue;
        }
        let traf = h.body(moof);
        let Some(tfhd) = boxes::find_child(traf, boxtype::TFHD)? else {
            return Err(Mp4Error::Missing("tfhd"));
        };
        let tfhd = parse_tfhd(tfhd.body(traf))?;
        trafs += 1;
        let Some(index) = movie.tracks.iter().position(|t| t.track_id == tfhd.track_id) else {
            continue;
        };
        let (track, trex) =
            (&movie.tracks[index], movie.trex.get(index).copied().unwrap_or_default());
        let base = match (tfhd.base_data_offset, tfhd.base_is_moof, trafs) {
            (Some(base), _, _) => base,
            (None, true, _) | (None, false, 1) => place.moof,
            (None, false, _) => data_end,
        };
        let mut dts = match boxes::find_child(traf, boxtype::TFDT)? {
            Some(tfdt) => parse_tfdt(tfdt.body(traf))?,
            None => next_dts.get(index).copied().unwrap_or(0),
        };
        if !tfhd.empty {
            let start =
                crate::reader::ticks_to_ns(dts.min(i64::MAX as u64) as i64, track.timescale);
            first = Some(first.map_or(start, |f| f.min(start)));
        }
        let mut offset = base;
        let mut child = 0usize;
        while child + 8 <= traf.len() && !tfhd.empty {
            let h = boxes::read_box_header(traf, child)?;
            child = h.end;
            if h.kind != boxtype::TRUN {
                continue;
            }
            let mut run = parse_trun(h.body(traf))?;
            if let Some(by) = run.data_offset {
                offset = base
                    .checked_add_signed(i64::from(by))
                    .ok_or(Mp4Error::Inconsistent("trun data offset"))?;
            }
            for n in 0..run.count {
                let row = run.row()?;
                let size = row.size.or(tfhd.size).unwrap_or(trex.size);
                let duration = row.duration.or(tfhd.duration).unwrap_or(trex.duration);
                let flags = match (row.flags, n, run.first_flags) {
                    (Some(flags), _, _) | (None, 0, Some(flags)) => flags,
                    _ => tfhd.flags.unwrap_or(trex.flags),
                };
                let raw = i128::from(dts) - i128::from(track.media_time);
                let stamp = |t: i128| t.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
                dts = dts
                    .checked_add(u64::from(duration))
                    .ok_or(Mp4Error::Inconsistent("decode time overflow"))?;
                if size == 0 {
                    empty += 1;
                    if empty > EMPTY_ROWS {
                        return Err(Mp4Error::Inconsistent("a run of samples of no octets"));
                    }
                    continue;
                }
                let end = offset
                    .checked_add(u64::from(size))
                    .ok_or(Mp4Error::Inconsistent("sample offset overflow"))?;
                if offset < place.mdat.0 || end > place.mdat.1 {
                    return Err(Mp4Error::Inconsistent("a sample outside its mdat"));
                }
                if out.len() >= room {
                    return Err(Mp4Error::TooManySamples { limit: room });
                }
                out.push(Sample {
                    track_index: index as u16,
                    offset,
                    size,
                    dts: stamp(raw),
                    pts: stamp(raw + i128::from(row.offset)),
                    sync: flags & NON_SYNC == 0,
                    hidden: false,
                });
                offset = end;
            }
        }
        data_end = offset;
        if let Some(next) = next_dts.get_mut(index) {
            *next = dts;
        }
    }
    Ok(first)
}

/// None for a head through `moov` that is not a fragmented movie, else whether its `mehd`
/// states the length.
pub fn length_stated(head: &[u8]) -> Option<bool> {
    let mut at = 0usize;
    while let Ok(h) = boxes::read_box_header(head, at) {
        if h.kind == boxtype::MOOV {
            let moov = h.body(head);
            let mvex = boxes::find_child(moov, boxtype::MVEX).ok().flatten()?;
            let mvex = mvex.body(moov);
            let mehd = boxes::find_child(mvex, boxtype::MEHD).ok().flatten();
            return Some(mehd.and_then(|m| parse_mehd(m.body(mvex)).ok()).is_some_and(|d| d != 0));
        }
        at = h.end;
    }
    None
}

/// Each track's presentation end in media ticks into `ends`, where a `tfdt` places its rows, and
/// its rows' durations into `deltas`, from the body of one `moof`. Rows past
/// [`MAX_FRAGMENT_SAMPLES`] are not read.
pub(crate) fn summarize(
    moof: &[u8],
    movie: &Movie<'_>,
    ends: &mut [Option<i64>],
    deltas: &mut [Tally],
) -> Result<(), BoxError> {
    let mut rows = 0usize;
    let mut at = 0usize;
    while at + 8 <= moof.len() {
        let h = boxes::read_box_header(moof, at)?;
        at = h.end;
        if h.kind != boxtype::TRAF {
            continue;
        }
        let traf = h.body(moof);
        let Some(tfhd) = boxes::find_child(traf, boxtype::TFHD)? else { continue };
        let tfhd = parse_tfhd(tfhd.body(traf))?;
        let Some(index) = movie.tracks.iter().position(|t| t.track_id == tfhd.track_id) else {
            continue;
        };
        let trex = movie.trex.get(index).copied().unwrap_or_default();
        let mut dts = match boxes::find_child(traf, boxtype::TFDT)? {
            Some(tfdt) => Some(i128::from(parse_tfdt(tfdt.body(traf))?)),
            None => None,
        };
        let mut child = 0usize;
        while child + 8 <= traf.len() && !tfhd.empty {
            let h = boxes::read_box_header(traf, child)?;
            child = h.end;
            if h.kind != boxtype::TRUN {
                continue;
            }
            let mut run = parse_trun(h.body(traf))?;
            for _ in 0..run.count {
                rows += 1;
                if rows > MAX_FRAGMENT_SAMPLES {
                    return Ok(());
                }
                let row = run.row()?;
                let duration = row.duration.or(tfhd.duration).unwrap_or(trex.duration);
                if let Some(t) = deltas.get_mut(index) {
                    t.add(duration, 1);
                }
                let Some(at) = dts else { continue };
                let end = at + i128::from(row.offset) + i128::from(duration);
                let end = end.clamp(0, i128::from(i64::MAX)) as i64;
                if let Some(e) = ends.get_mut(index) {
                    *e = Some(e.map_or(end, |e| e.max(end)));
                }
                dts = Some(at + i128::from(duration));
            }
        }
    }
    Ok(())
}

/// The most an `mfra` is read to, as `pf_probe` reads it.
const MAX_MFRA: u64 = 16 << 20;
/// The first read at the last `moof`, most hold the whole atom.
const MOOF_FIRST: u64 = 64 << 10;

/// What [`TailAt`] read, each empty where the file has none.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tail {
    pub mfra: Vec<u8>,
    /// The last `moof` atom the `mfra` names, header and all.
    pub moof: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TailStep {
    Start,
    Mfro,
    Mfra,
    /// The first read at `at`, the `moof` there or behind the atoms a segment leads with.
    Moof {
        at: u64,
    },
    Whole,
}

/// Reads the `mfra` a fragmented file of `len` bytes ends in (8.8.9), then the last `moof` its
/// rows name, where the length of a movie its `mehd` does not state ends.
#[derive(Debug)]
pub struct TailAt {
    len: u64,
    step: TailStep,
    mfro: [u8; 16],
    tail: Tail,
    got: usize,
}

impl TailAt {
    pub fn new(len: u64) -> Self {
        TailAt { len, step: TailStep::Start, mfro: [0; 16], tail: Tail::default(), got: 0 }
    }

    // Cold path, once per open.
    #[allow(clippy::disallowed_methods)]
    fn read(&mut self, step: TailStep, at: u64, len: u64, into_moof: bool) -> Step<Tail> {
        let buf = vec![0; len as usize];
        match into_moof {
            true => self.tail.moof = buf,
            false => self.tail.mfra = buf,
        }
        self.step = step;
        Step::Need(Need { at, len: len as usize })
    }

    fn done(&mut self) -> Step<Tail> {
        self.step = TailStep::Start;
        Step::Done(std::mem::take(&mut self.tail))
    }

    /// The `moof` the first read at `at` holds, whole or read again by its size.
    // Cold path, once per open.
    #[allow(clippy::disallowed_methods)]
    fn moof_at(&mut self, at: u64) -> Step<Tail> {
        let held = std::mem::take(&mut self.tail.moof);
        let mut from = 0usize;
        while let Some((kind, header, size)) =
            held.get(from..).and_then(|rest| box_head(rest, self.len - at - from as u64))
        {
            if kind == boxtype::MOOF {
                if size - header > MAX_FRAGMENT_HEAD {
                    break;
                }
                let end = from as u64 + size;
                if end <= held.len() as u64 {
                    self.tail.moof = held[from..end as usize].to_vec();
                    return self.done();
                }
                return self.read(TailStep::Whole, at + from as u64, size, true);
            }
            from = match usize::try_from(size).ok().and_then(|n| from.checked_add(n)) {
                Some(next) => next,
                None => break,
            };
        }
        self.done()
    }
}

/// The type, header length and size of the atom `b` starts with, `left` the bytes to the end of
/// the file. None where the header is cut or does not hold together.
fn box_head(b: &[u8], left: u64) -> Option<(boxes::FourCc, u64, u64)> {
    let h = b.get(..8)?;
    let kind = [h[4], h[5], h[6], h[7]];
    let (header, size) = match u32::from_be_bytes([h[0], h[1], h[2], h[3]]) {
        0 => (8, left),
        1 => (16, u64::from_be_bytes(b.get(8..16)?.try_into().ok()?)),
        n => (8, u64::from(n)),
    };
    (size >= header && size <= left).then_some((kind, header, size))
}

/// The largest `moof` offset any `tfra` of an `mfra` atom names, below `len`.
fn last_moof(mfra: &[u8], len: u64) -> Option<u64> {
    let h = boxes::read_box_header(mfra, 0).ok().filter(|h| h.kind == boxtype::MFRA)?;
    let body = h.body(mfra);
    let mut last = None;
    let _ = boxes::for_each_child(body, |child| {
        if child.kind == boxtype::TFRA {
            let (_, rows) = parse_tfra(child.body(body))?;
            let most = rows.iter().map(|r| r.1).filter(|&at| at < len).max();
            last = last.max(most);
        }
        Ok(())
    });
    last
}

impl Fetch for TailAt {
    type Out = Tail;
    type Err = Infallible;

    fn poll(&mut self) -> Result<Step<Tail>, Infallible> {
        let len = self.len;
        Ok(match self.step {
            TailStep::Start => match len.checked_sub(16) {
                Some(at) => {
                    self.step = TailStep::Mfro;
                    Step::Need(Need { at, len: 16 })
                }
                None => Step::Done(Tail::default()),
            },
            TailStep::Mfro => match mfra_size(&self.mfro[..self.got]) {
                Some(size) if (16..=MAX_MFRA.min(len)).contains(&size) => {
                    self.read(TailStep::Mfra, len - size, size, false)
                }
                _ => self.done(),
            },
            TailStep::Mfra => {
                self.tail.mfra.truncate(self.got);
                match last_moof(&self.tail.mfra, len) {
                    Some(at) => {
                        self.read(TailStep::Moof { at }, at, MOOF_FIRST.min(len - at), true)
                    }
                    None => self.done(),
                }
            }
            TailStep::Moof { at } => {
                self.tail.moof.truncate(self.got);
                self.moof_at(at)
            }
            TailStep::Whole => {
                if self.got < self.tail.moof.len() {
                    self.tail.moof.clear();
                }
                self.done()
            }
        })
    }

    fn buf(&mut self) -> &mut [u8] {
        match self.step {
            TailStep::Mfro => &mut self.mfro,
            TailStep::Mfra => &mut self.tail.mfra,
            TailStep::Moof { .. } | TailStep::Whole => &mut self.tail.moof,
            TailStep::Start => &mut [],
        }
    }

    fn filled(&mut self, n: usize) {
        self.got = n.min(self.buf().len());
    }
}

#[cfg(test)]
// Test code builds its inputs on the heap.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn be32(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    #[test]
    fn a_track_fragment_header_carries_what_its_flags_say() {
        let mut body = be32(&[0x0002_0029, 7]);
        body.extend(0x1234u64.to_be_bytes());
        body.extend(be32(&[512, 0x0101_0000]));
        let tfhd = parse_tfhd(&body).expect("a whole tfhd");
        assert_eq!(tfhd.track_id, 7);
        assert_eq!(tfhd.base_data_offset, Some(0x1234));
        assert_eq!((tfhd.duration, tfhd.size, tfhd.flags), (Some(512), None, Some(0x0101_0000)));
        assert!(tfhd.base_is_moof && !tfhd.empty);
        assert!(parse_tfhd(&body[..10]).is_err());
    }

    #[test]
    fn a_run_yields_the_fields_it_carries_and_ends_with_its_box() {
        let mut body = be32(&[0x0100_0A05, 2, 100, 0x0200_0000]);
        body.extend(be32(&[10]));
        body.extend((-3i32).to_be_bytes());
        body.extend(be32(&[20, 5]));
        let mut run = parse_trun(&body).expect("a whole trun");
        assert_eq!(
            (run.count, run.data_offset, run.first_flags),
            (2, Some(100), Some(0x0200_0000))
        );
        assert_eq!(
            run.row(),
            Ok(TrunRow { duration: None, size: Some(10), flags: None, offset: -3 })
        );
        assert_eq!(
            run.row(),
            Ok(TrunRow { duration: None, size: Some(20), flags: None, offset: 5 })
        );
        assert!(run.row().is_err(), "a third row is past the box");
    }

    #[test]
    fn a_segment_index_names_each_fragment_by_time_and_offset() {
        let mut body = be32(&[0, 1, 1000, 500, 16]);
        body.extend([0, 0, 0, 2]);
        body.extend(be32(&[300, 2000, 0x9000_0000, 400, 1000, 0x9000_0000]));
        let sidx = parse_sidx(&body).expect("a whole sidx");
        let mut out = Vec::new();
        sidx_fragments(&sidx, 1000, None, &mut out);
        assert_eq!(
            out,
            [
                FragmentRef { time_ns: 500_000_000, offset: 1016 },
                FragmentRef { time_ns: 2_500_000_000, offset: 1316 },
            ]
        );
    }

    #[test]
    fn the_last_octets_of_a_file_say_where_its_index_starts() {
        let mut tail = vec![9u8; 5];
        tail.extend([0, 0, 0, 16]);
        tail.extend(b"mfro");
        tail.extend(be32(&[0, 77]));
        assert_eq!(mfra_size(&tail), Some(77));
        assert_eq!(mfra_size(&tail[..20]), None);
    }
}
