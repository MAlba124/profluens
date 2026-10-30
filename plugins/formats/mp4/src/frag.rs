//! Movie fragments (ISO/IEC 14496-12 8.8), `sidx` (8.16.3) and `mfra` (8.8.9).
//! A `trun` can declare four billion samples in twenty octets, so rows are bounded by the `mdat`
//! octets and [`MAX_FRAGMENT_SAMPLES`].

use crate::boxes::{self, BoxError, Cursor, boxtype, read_full_box_head};
use crate::reader::{Mp4Error, Sample, Track};

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
    r.skip(4, "sidx reference_ID")?;
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
    Ok(Sidx { timescale, earliest, first_offset, refs })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FragmentRef {
    pub time_ns: u64,
    pub offset: u64,
}

pub fn sidx_fragments(sidx: &Sidx, anchor: u64, out: &mut Vec<FragmentRef>) {
    let (mut time, mut offset) = (sidx.earliest, anchor.saturating_add(sidx.first_offset));
    for r in &sidx.refs {
        if !r.index {
            out.push(FragmentRef {
                time_ns: crate::reader::ticks_to_ns(
                    time.min(i64::MAX as u64) as i64,
                    sidx.timescale,
                ),
                offset,
            });
        }
        time = time.saturating_add(u64::from(r.duration));
        offset = offset.saturating_add(u64::from(r.size));
    }
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
        sidx_fragments(&sidx, 1000, &mut out);
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
