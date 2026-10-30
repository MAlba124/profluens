//! Write side of the ISO/IEC 14496-12 box grammar, pure functions over a byte `Vec` and per-track
//! tables. Decode times are derived from presentation stamps, since neither demuxer here sends
//! them.

// Cold path, once per file or fragment.
#![allow(clippy::disallowed_methods)]

use stein_arena::Grow;

pub const MOVIE_TIMESCALE: u32 = 1000;
/// The MPEG 90 kHz, which holds a millisecond and the common frame rates exactly.
pub const VIDEO_TIMESCALE: u32 = 90_000;
pub const TEXT_TIMESCALE: u32 = 1000;
pub const TABLE_SAMPLES: usize = 1 << 22;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    TableFull { track: u32 },
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TableFull { track } => {
                write!(f, "track {track} holds more samples than its table")
            }
        }
    }
}

impl std::error::Error for WriteError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handler {
    Video,
    Audio,
    Text,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Avc(Vec<u8>),
    Hevc {
        record: Vec<u8>,
        inband: bool,
    },
    Av1(Vec<u8>),
    /// `vp09` with the `vpcC` body after its FullBox head (VP Codec ISO Media File Format Binding
    /// 2.3.3).
    Vp9(Vec<u8>),
    Aac(Vec<u8>),
    Mp3,
    /// Encapsulation of Opus in ISOBMFF 4.3.2.
    Opus(Vec<u8>),
    Flac(Vec<u8>),
    /// 3GPP TS 26.245.
    Tx3g,
}

#[derive(Clone, Debug)]
pub struct TrackHead {
    pub id: u32,
    pub handler: Handler,
    pub entry: Entry,
    pub timescale: u32,
    pub width: u32,
    pub height: u32,
    pub channels: u16,
    pub rate: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edit {
    pub empty: u64,
    pub media_time: i64,
    pub duration: u64,
}

impl Edit {
    fn needed(&self) -> bool {
        self.empty != 0 || self.media_time != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub offset: u64,
    pub first: u32,
}

pub struct Table {
    pub sizes: Grow<u32>,
    pub pts: Grow<i64>,
    pub hints: Grow<u32>,
    pub sync: Grow<u32>,
    pub chunks: Grow<Chunk>,
}

impl Table {
    pub fn new(track: u32) -> Result<Table, WriteError> {
        let full = |_| WriteError::TableFull { track };
        Ok(Table {
            sizes: Grow::reserve(TABLE_SAMPLES).map_err(full)?,
            pts: Grow::reserve(TABLE_SAMPLES).map_err(full)?,
            hints: Grow::reserve(TABLE_SAMPLES).map_err(full)?,
            sync: Grow::reserve(TABLE_SAMPLES).map_err(full)?,
            chunks: Grow::reserve(TABLE_SAMPLES).map_err(full)?,
        })
    }

    pub fn len(&self) -> usize {
        self.sizes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sizes.is_empty()
    }

    pub fn push(&mut self, track: u32, s: Sample, chunk: Option<u64>) -> Result<(), WriteError> {
        let full = |_| WriteError::TableFull { track };
        let n = self.sizes.len() as u32;
        if let Some(offset) = chunk {
            self.chunks.push(Chunk { offset, first: n }).map_err(full)?;
        }
        self.sizes.push(s.size).map_err(full)?;
        self.pts.push(s.pts).map_err(full)?;
        self.hints.push(s.hint).map_err(full)?;
        if s.sync {
            self.sync.push(n + 1).map_err(full)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub size: u32,
    pub pts: i64,
    pub hint: u32,
    pub sync: bool,
}

/// Decode times of a run in decode order, from presentation stamps and hints, returning the reorder
/// shift. Video sorts the stamps and moves them back so no sample decodes after it shows.
pub fn decode_times(
    handler: Handler,
    timescale: u32,
    pts: &[i64],
    hints: &[u32],
    shift: Option<i64>,
    prev: Option<(i64, u32)>,
    dts: &mut [i64],
) -> i64 {
    let n = pts.len().min(dts.len());
    let (pts, dts) = (&pts[..n], &mut dts[..n]);
    let mut used = shift.unwrap_or(0);
    match handler {
        Handler::Video => {
            dts.copy_from_slice(pts);
            dts.sort_unstable();
            if shift.is_none() {
                used = dts.iter().zip(pts).map(|(s, p)| s - p).max().unwrap_or(0).max(0);
            }
            dts.iter_mut().for_each(|d| *d -= used);
        }
        Handler::Audio => {
            let tolerance = i64::from(timescale / 1000) + 1;
            for i in 0..n {
                let (last, hint) = match i {
                    0 => prev.unwrap_or((0, 0)),
                    _ => (dts[i - 1], hints[i - 1]),
                };
                let expected = (hint != 0).then_some(last + i64::from(hint));
                dts[i] = match expected {
                    Some(e) if (pts[i] - e).abs() <= tolerance => e,
                    _ => pts[i],
                };
            }
        }
        Handler::Text => dts.copy_from_slice(pts),
    }
    let mut floor = prev.map(|p| p.0);
    for d in dts.iter_mut() {
        if let Some(f) = floor {
            *d = (*d).max(f + 1);
        }
        floor = Some(*d);
    }
    used
}

pub fn duration_at(dts: &[i64], hints: &[u32], i: usize, fallback: u32) -> u32 {
    if i + 1 < dts.len() {
        return (dts[i + 1] - dts[i]).clamp(0, u32::MAX as i64) as u32;
    }
    match hints.get(i) {
        Some(&h) if h != 0 => h,
        _ if i > 0 => (dts[i] - dts[i - 1]).clamp(0, u32::MAX as i64) as u32,
        _ => fallback,
    }
}

pub fn progressive_edit(
    head: &TrackHead,
    table: &Table,
    dts: &[i64],
    delay: i64,
    empty: u64,
) -> (Edit, u64) {
    let n = table.len().min(dts.len());
    if n == 0 {
        return (Edit::default(), 0);
    }
    let (pts, hints) = (table.pts.as_slice(), table.hints.as_slice());
    let fallback = head.timescale / 25;
    let shown = |i: usize| {
        if head.handler == Handler::Video { pts[i] } else { dts[i] }
    };
    let start = (0..n).map(shown).min().unwrap_or(0);
    let end = (0..n)
        .map(|i| shown(i) + i64::from(duration_at(dts, hints, i, fallback)))
        .max()
        .unwrap_or(0);
    let media_duration =
        (dts[n - 1] + i64::from(duration_at(dts, hints, n - 1, fallback)) - dts[0]).max(0) as u64;
    let shown_len = (end - start - delay).max(0);
    let edit = Edit {
        empty,
        media_time: start + delay - dts[0],
        duration: rescale(shown_len, head.timescale, MOVIE_TIMESCALE).max(1) as u64,
    };
    (edit, media_duration)
}

pub fn ticks(ns: i64, timescale: u32) -> i64 {
    let t = i128::from(ns) * i128::from(timescale);
    let half = if t >= 0 { 500_000_000 } else { -500_000_000 };
    ((t + half) / 1_000_000_000) as i64
}

pub fn rescale(v: i64, from: u32, to: u32) -> i64 {
    let t = i128::from(v) * i128::from(to);
    let f = i128::from(from.max(1));
    ((t + f / 2) / f) as i64
}

fn open(out: &mut Vec<u8>, kind: &[u8; 4]) -> usize {
    let at = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(kind);
    at
}

/// ISO/IEC 14496-12 4.2.
fn open_full(out: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32) -> usize {
    let at = open(out, kind);
    out.extend_from_slice(&((u32::from(version) << 24) | (flags & 0xFF_FFFF)).to_be_bytes());
    at
}

fn close(out: &mut [u8], at: usize) {
    let n = (out.len() - at) as u32;
    out[at..at + 4].copy_from_slice(&n.to_be_bytes());
}

fn u16be(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn u32be(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn u64be(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// ISO/IEC 14496-12 8.2.2.2.
fn matrix(out: &mut Vec<u8>) {
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        u32be(out, v);
    }
}

pub fn ftyp(out: &mut Vec<u8>, fragmented: bool) {
    let at = open(out, b"ftyp");
    out.extend_from_slice(b"isom");
    u32be(out, 0x200);
    out.extend_from_slice(b"isom");
    out.extend_from_slice(if fragmented { b"iso6" } else { b"iso2" });
    out.extend_from_slice(b"avc1");
    out.extend_from_slice(b"mp41");
    close(out, at);
}

/// `mdat` head with a 64-bit `largesize`, patched at the returned offset.
pub fn mdat_head(out: &mut Vec<u8>) -> usize {
    u32be(out, 1);
    out.extend_from_slice(b"mdat");
    let at = out.len();
    u64be(out, 0);
    at
}

pub const MDAT_HEAD: u64 = 16;

pub struct TrackMoov<'a> {
    pub head: &'a TrackHead,
    pub edit: Edit,
    pub table: Option<&'a Table>,
    pub dts: &'a [i64],
    pub media_duration: u64,
}

pub fn moov(out: &mut Vec<u8>, tracks: &[TrackMoov<'_>], fragmented: bool) {
    let at = open(out, b"moov");
    let movie_duration = tracks.iter().map(|t| t.edit.empty + edit_duration(t)).max().unwrap_or(0);
    mvhd(out, movie_duration, tracks.iter().map(|t| t.head.id).max().unwrap_or(0) + 1);
    for t in tracks {
        trak(out, t);
    }
    if fragmented {
        let mvex = open(out, b"mvex");
        for t in tracks {
            let trex = open_full(out, b"trex", 0, 0);
            u32be(out, t.head.id);
            u32be(out, 1);
            u32be(out, 0);
            u32be(out, 0);
            u32be(out, 0);
            close(out, trex);
        }
        close(out, mvex);
    }
    close(out, at);
}

fn edit_duration(t: &TrackMoov<'_>) -> u64 {
    if t.edit.duration != 0 {
        return t.edit.duration;
    }
    let shown = (t.media_duration as i64 - t.edit.media_time).max(0);
    rescale(shown, t.head.timescale, MOVIE_TIMESCALE) as u64
}

fn mvhd(out: &mut Vec<u8>, duration: u64, next_track: u32) {
    let v1 = duration > u64::from(u32::MAX);
    let at = open_full(out, b"mvhd", u8::from(v1), 0);
    if v1 {
        u64be(out, 0);
        u64be(out, 0);
        u32be(out, MOVIE_TIMESCALE);
        u64be(out, duration);
    } else {
        u32be(out, 0);
        u32be(out, 0);
        u32be(out, MOVIE_TIMESCALE);
        u32be(out, duration as u32);
    }
    u32be(out, 0x0001_0000);
    u16be(out, 0x0100);
    out.extend_from_slice(&[0; 10]);
    matrix(out);
    out.extend_from_slice(&[0; 24]);
    u32be(out, next_track);
    close(out, at);
}

fn trak(out: &mut Vec<u8>, t: &TrackMoov<'_>) {
    let at = open(out, b"trak");
    let h = t.head;
    let duration =
        if t.table.is_some_and(|tb| !tb.is_empty()) { t.edit.empty + edit_duration(t) } else { 0 };
    let v1 = duration > u64::from(u32::MAX);
    let tkhd = open_full(out, b"tkhd", u8::from(v1), 3);
    if v1 {
        u64be(out, 0);
        u64be(out, 0);
        u32be(out, h.id);
        u32be(out, 0);
        u64be(out, duration);
    } else {
        u32be(out, 0);
        u32be(out, 0);
        u32be(out, h.id);
        u32be(out, 0);
        u32be(out, duration as u32);
    }
    out.extend_from_slice(&[0; 8]);
    u16be(out, 0);
    u16be(out, 0);
    u16be(out, if h.handler == Handler::Audio { 0x0100 } else { 0 });
    u16be(out, 0);
    matrix(out);
    let (w, hh) = if h.handler == Handler::Video { (h.width, h.height) } else { (0, 0) };
    // 16.16 fixed point, a dimension past 65535 would wrap.
    u32be(out, w.min(0xFFFF) << 16);
    u32be(out, hh.min(0xFFFF) << 16);
    close(out, tkhd);
    if t.edit.needed() {
        let edts = open(out, b"edts");
        let entries = 1 + u32::from(t.edit.empty != 0);
        let wide = t.edit.empty > u64::from(u32::MAX) || edit_duration(t) > u64::from(u32::MAX);
        let elst = open_full(out, b"elst", u8::from(wide), 0);
        u32be(out, entries);
        let entry = |out: &mut Vec<u8>, duration: u64, media: i64| {
            if wide {
                u64be(out, duration);
                u64be(out, media as u64);
            } else {
                u32be(out, duration as u32);
                u32be(out, media as i32 as u32);
            }
            u32be(out, 0x0001_0000);
        };
        if t.edit.empty != 0 {
            entry(out, t.edit.empty, -1);
        }
        let d = if t.table.is_some_and(|tb| !tb.is_empty()) { edit_duration(t) } else { 0 };
        entry(out, d, t.edit.media_time);
        close(out, elst);
        close(out, edts);
    }
    let mdia = open(out, b"mdia");
    let mv1 = t.media_duration > u64::from(u32::MAX);
    let mdhd = open_full(out, b"mdhd", u8::from(mv1), 0);
    if mv1 {
        u64be(out, 0);
        u64be(out, 0);
        u32be(out, h.timescale);
        u64be(out, t.media_duration);
    } else {
        u32be(out, 0);
        u32be(out, 0);
        u32be(out, h.timescale);
        u32be(out, t.media_duration as u32);
    }
    // `und`, three letters of five bits less 0x60.
    u16be(out, 0x55C4);
    u16be(out, 0);
    close(out, mdhd);
    let (kind, name): (&[u8; 4], &[u8]) = match h.handler {
        Handler::Video => (b"vide", b"VideoHandler\0"),
        Handler::Audio => (b"soun", b"SoundHandler\0"),
        Handler::Text => (b"text", b"TextHandler\0"),
    };
    let hdlr = open_full(out, b"hdlr", 0, 0);
    u32be(out, 0);
    out.extend_from_slice(kind);
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(name);
    close(out, hdlr);
    let minf = open(out, b"minf");
    match h.handler {
        Handler::Video => {
            let b = open_full(out, b"vmhd", 0, 1);
            out.extend_from_slice(&[0; 8]);
            close(out, b);
        }
        Handler::Audio => {
            let b = open_full(out, b"smhd", 0, 0);
            out.extend_from_slice(&[0; 4]);
            close(out, b);
        }
        Handler::Text => {
            let b = open_full(out, b"nmhd", 0, 0);
            close(out, b);
        }
    }
    let dinf = open(out, b"dinf");
    let dref = open_full(out, b"dref", 0, 0);
    u32be(out, 1);
    let url = open_full(out, b"url ", 0, 1);
    close(out, url);
    close(out, dref);
    close(out, dinf);
    stbl(out, t);
    close(out, minf);
    close(out, mdia);
    close(out, at);
}

type Columns<'a> = (&'a [u32], &'a [i64], &'a [u32], &'a [u32], &'a [Chunk]);

fn stbl(out: &mut Vec<u8>, t: &TrackMoov<'_>) {
    let at = open(out, b"stbl");
    let stsd = open_full(out, b"stsd", 0, 0);
    u32be(out, 1);
    sample_entry(out, t.head);
    close(out, stsd);
    let (sizes, pts, hints, sync, chunks): Columns<'_> = match t.table {
        Some(tb) => (&tb.sizes, &tb.pts, &tb.hints, &tb.sync, &tb.chunks),
        None => (&[], &[], &[], &[], &[]),
    };
    let n = sizes.len();
    let fallback = t.head.timescale / 25;
    let stts = open_full(out, b"stts", 0, 0);
    let count_at = out.len();
    u32be(out, 0);
    let mut runs = 0u32;
    let mut i = 0usize;
    while i < n {
        let d = duration_at(t.dts, hints, i, fallback);
        let mut j = i + 1;
        while j < n && duration_at(t.dts, hints, j, fallback) == d {
            j += 1;
        }
        u32be(out, (j - i) as u32);
        u32be(out, d);
        runs += 1;
        i = j;
    }
    out[count_at..count_at + 4].copy_from_slice(&runs.to_be_bytes());
    close(out, stts);
    // Audio and text present as they decode, their stamps snapped onto the decode times.
    if t.head.handler == Handler::Video && (0..n).any(|i| pts[i] != t.dts[i]) {
        let signed = (0..n).any(|i| pts[i] < t.dts[i]);
        let ctts = open_full(out, b"ctts", u8::from(signed), 0);
        let count_at = out.len();
        u32be(out, 0);
        let mut runs = 0u32;
        let mut i = 0usize;
        while i < n {
            let c = pts[i] - t.dts[i];
            let mut j = i + 1;
            while j < n && pts[j] - t.dts[j] == c {
                j += 1;
            }
            u32be(out, (j - i) as u32);
            u32be(out, c as i32 as u32);
            runs += 1;
            i = j;
        }
        out[count_at..count_at + 4].copy_from_slice(&runs.to_be_bytes());
        close(out, ctts);
    }
    if sync.len() != n {
        let stss = open_full(out, b"stss", 0, 0);
        u32be(out, sync.len() as u32);
        sync.iter().for_each(|&s| u32be(out, s));
        close(out, stss);
    }
    let stsc = open_full(out, b"stsc", 0, 0);
    let count_at = out.len();
    u32be(out, 0);
    let mut rows = 0u32;
    let mut prev = u32::MAX;
    for (c, chunk) in chunks.iter().enumerate() {
        let end = chunks.get(c + 1).map_or(n as u32, |next| next.first);
        let per = end - chunk.first;
        if per != prev {
            u32be(out, c as u32 + 1);
            u32be(out, per);
            u32be(out, 1);
            rows += 1;
            prev = per;
        }
    }
    out[count_at..count_at + 4].copy_from_slice(&rows.to_be_bytes());
    close(out, stsc);
    let stsz = open_full(out, b"stsz", 0, 0);
    let same = n > 0 && sizes.iter().all(|&s| s == sizes[0]);
    u32be(out, if same { sizes[0] } else { 0 });
    u32be(out, n as u32);
    if !same {
        sizes.iter().for_each(|&s| u32be(out, s));
    }
    close(out, stsz);
    let wide = chunks.iter().any(|c| c.offset > u64::from(u32::MAX));
    let stco = open_full(out, if wide { b"co64" } else { b"stco" }, 0, 0);
    u32be(out, chunks.len() as u32);
    for c in chunks {
        if wide {
            u64be(out, c.offset);
        } else {
            u32be(out, c.offset as u32);
        }
    }
    close(out, stco);
    close(out, at);
}

/// ISO/IEC 14496-1 8.3.3. Size in four bytes, as ffmpeg writes it.
fn descriptor(out: &mut Vec<u8>, tag: u8, size: usize) {
    out.push(tag);
    for shift in [21, 14, 7] {
        out.push(0x80 | ((size >> shift) & 0x7F) as u8);
    }
    out.push((size & 0x7F) as u8);
}

/// ISO/IEC 14496-14 5.6.
fn esds(out: &mut Vec<u8>, id: u32, oti: u8, asc: &[u8]) {
    let at = open_full(out, b"esds", 0, 0);
    let dsi = if asc.is_empty() { 0 } else { 5 + asc.len() };
    let dcd = 13 + dsi;
    descriptor(out, 0x03, 3 + 5 + dcd + 5 + 1);
    u16be(out, id as u16);
    out.push(0);
    descriptor(out, 0x04, dcd);
    out.push(oti);
    // streamType audio, upStream 0, reserved 1.
    out.push(0x15);
    out.extend_from_slice(&[0; 3]);
    u32be(out, 0);
    u32be(out, 0);
    if !asc.is_empty() {
        descriptor(out, 0x05, asc.len());
        out.extend_from_slice(asc);
    }
    descriptor(out, 0x06, 1);
    out.push(2);
    close(out, at);
}

fn sample_entry(out: &mut Vec<u8>, h: &TrackHead) {
    let kind: &[u8; 4] = match &h.entry {
        Entry::Avc(_) => b"avc1",
        Entry::Hevc { inband: true, .. } => b"hev1",
        Entry::Hevc { .. } => b"hvc1",
        Entry::Av1(_) => b"av01",
        Entry::Vp9(_) => b"vp09",
        Entry::Aac(_) | Entry::Mp3 => b"mp4a",
        Entry::Opus(_) => b"Opus",
        Entry::Flac(_) => b"fLaC",
        Entry::Tx3g => b"tx3g",
    };
    let at = open(out, kind);
    out.extend_from_slice(&[0; 6]);
    u16be(out, 1);
    match h.handler {
        Handler::Video => {
            out.extend_from_slice(&[0; 16]);
            u16be(out, h.width.min(0xFFFF) as u16);
            u16be(out, h.height.min(0xFFFF) as u16);
            u32be(out, 0x0048_0000);
            u32be(out, 0x0048_0000);
            u32be(out, 0);
            u16be(out, 1);
            out.extend_from_slice(&[0; 32]);
            u16be(out, 0x0018);
            u16be(out, 0xFFFF);
            let (cfg, full, body): (&[u8; 4], bool, &[u8]) = match &h.entry {
                Entry::Avc(r) => (b"avcC", false, r),
                Entry::Hevc { record, .. } => (b"hvcC", false, record),
                Entry::Av1(r) => (b"av1C", false, r),
                Entry::Vp9(r) => (b"vpcC", true, r),
                _ => (b"free", false, &[]),
            };
            let c = if full { open_full(out, cfg, 1, 0) } else { open(out, cfg) };
            out.extend_from_slice(body);
            close(out, c);
        }
        Handler::Audio => {
            out.extend_from_slice(&[0; 8]);
            u16be(out, h.channels);
            u16be(out, 16);
            u32be(out, 0);
            u32be(out, if h.rate <= 0xFFFF { h.rate << 16 } else { 0 });
            match &h.entry {
                Entry::Aac(asc) => esds(out, h.id, 0x40, asc),
                Entry::Mp3 => esds(out, h.id, 0x6B, &[]),
                Entry::Opus(dops) => {
                    let c = open(out, b"dOps");
                    out.extend_from_slice(dops);
                    close(out, c);
                }
                Entry::Flac(blocks) => {
                    let c = open_full(out, b"dfLa", 0, 0);
                    out.extend_from_slice(blocks);
                    close(out, c);
                }
                _ => {}
            }
        }
        Handler::Text => {
            // Default style as ffmpeg writes it, so its reader shows plain text.
            u32be(out, 0);
            out.extend_from_slice(&[1, 0xFF]);
            out.extend_from_slice(&[0, 0, 0, 0xFF]);
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0x10, 0xFF, 0xFF, 0xFF, 0xFF]);
            let ftab = open(out, b"ftab");
            u16be(out, 1);
            u16be(out, 1);
            out.push(5);
            out.extend_from_slice(b"Arial");
            close(out, ftab);
        }
    }
    close(out, at);
}

/// RFC 7845 5.1. `None` for a short head or another magic.
pub fn dops_from_opus_head(head: &[u8]) -> Option<Vec<u8>> {
    if head.len() < 19 || &head[..8] != b"OpusHead" {
        return None;
    }
    let channels = head[9];
    let pre_skip = u16::from_le_bytes([head[10], head[11]]);
    let rate = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
    let gain = i16::from_le_bytes([head[16], head[17]]);
    let family = head[18];
    let mut out = Vec::with_capacity(11 + 2 + usize::from(channels));
    out.push(0);
    out.push(channels);
    out.extend_from_slice(&pre_skip.to_be_bytes());
    out.extend_from_slice(&rate.to_be_bytes());
    out.extend_from_slice(&gain.to_be_bytes());
    out.push(family);
    if family != 0 {
        let table = head.get(19..21 + usize::from(channels))?;
        out.extend_from_slice(table);
    }
    Some(out)
}

pub fn dops(channels: u8, pre_skip: u16, rate: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(11);
    out.push(0);
    out.push(channels);
    out.extend_from_slice(&pre_skip.to_be_bytes());
    out.extend_from_slice(&rate.to_be_bytes());
    out.extend_from_slice(&0i16.to_be_bytes());
    out.push(0);
    out
}

#[derive(Clone, Copy)]
pub struct Run<'a> {
    pub track: u32,
    pub base_dts: u64,
    pub sizes: &'a [u32],
    pub durations: &'a [u32],
    pub cts: &'a [i64],
    pub sync: &'a [bool],
}

/// Data offsets count from the `moof` (`default-base-is-moof`). Returns the `mdat` payload size.
pub fn moof(out: &mut Vec<u8>, sequence: u32, runs: &[Run<'_>]) -> u64 {
    let start = out.len();
    let at = open(out, b"moof");
    let mfhd = open_full(out, b"mfhd", 0, 0);
    u32be(out, sequence);
    close(out, mfhd);
    let mut offsets = [0usize; 16];
    for (k, r) in runs.iter().enumerate() {
        let traf = open(out, b"traf");
        let tfhd = open_full(out, b"tfhd", 0, 0x02_0000);
        u32be(out, r.track);
        close(out, tfhd);
        let tfdt = open_full(out, b"tfdt", 1, 0);
        u64be(out, r.base_dts);
        close(out, tfdt);
        let any_cts = r.cts.iter().any(|&c| c != 0);
        let signed = r.cts.iter().any(|&c| c < 0);
        let flags = 0x001 | 0x100 | 0x200 | 0x400 | if any_cts { 0x800 } else { 0 };
        let trun = open_full(out, b"trun", u8::from(signed), flags);
        u32be(out, r.sizes.len() as u32);
        if let Some(o) = offsets.get_mut(k) {
            *o = out.len();
        }
        u32be(out, 0);
        for i in 0..r.sizes.len() {
            u32be(out, r.durations[i]);
            u32be(out, r.sizes[i]);
            // sample_depends_on 2 for a sync sample, else 1 and non-sync.
            u32be(out, if r.sync[i] { 0x0200_0000 } else { 0x0101_0000 });
            if any_cts {
                u32be(out, r.cts[i] as i32 as u32);
            }
        }
        close(out, trun);
        close(out, traf);
    }
    close(out, at);
    let moof_len = out.len() - start;
    let payload: u64 =
        runs.iter().map(|r| r.sizes.iter().map(|&s| u64::from(s)).sum::<u64>()).sum();
    let wide = payload + 8 > u64::from(u32::MAX);
    let head = if wide { 16 } else { 8 };
    let mut data = (moof_len + head) as u64;
    for (k, r) in runs.iter().enumerate() {
        if let Some(&o) = offsets.get(k) {
            out[o..o + 4].copy_from_slice(&(data as u32).to_be_bytes());
        }
        data += r.sizes.iter().map(|&s| u64::from(s)).sum::<u64>();
    }
    if wide {
        u32be(out, 1);
        out.extend_from_slice(b"mdat");
        u64be(out, payload + 16);
    } else {
        u32be(out, (payload + 8) as u32);
        out.extend_from_slice(b"mdat");
    }
    payload
}

#[cfg(test)]
mod tests;
