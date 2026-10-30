//! Seek index and frame clock of a bare MP3. A CBR stream indexes
//! frames on a straight line, else the Xing table, else a bitrate estimate. Times exclude the
//! header frame, LAME's encoder delay and the decoder's 529 samples, as FFmpeg does.

use profluens_core::ctx::Ctx;
use profluens_core::pipeline::{Index, IndexEntry};
use profluens_core::time::Timestamp;

use crate::frame::{Framed, Header, next_frame};
use crate::props::{AudioProps, TAG_FRAME_PROBE, TagFrame, XingToc, probe_props, tag_frame};

/// Audio past the tag searched for the first frame. A Layer III frame is at most 1441 bytes.
pub const WINDOW: usize = 8 * 1024;

const TAG_HEAD: usize = 10;

/// Decoder synthesis delay FFmpeg drops with LAME's encoder delay (mp3dec.c, `528 + 1`).
pub const DECODER_DELAY: u64 = 529;

pub const MAX_ENTRIES: u64 = 4096;

pub const PREROLL_MAX: u64 = 16;

pub const RESYNC_MAX: usize = 64 * 1024;

/// Grown entries between publications.
const GROW_EVERY: usize = 1;

/// How far an entry's restart is ahead of its computed frame start, for padding rounding.
const MARGIN: u64 = 1;

/// How far off the line a frame found behind a seek may start and still be that frame.
const SLACK: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scan {
    Tag,
    Skip(u64),
    Audio,
    Done,
}

pub struct HeadScan {
    len: Option<u64>,
    scan: Scan,
    head: [u8; TAG_HEAD],
    have: usize,
    tag: u64,
    window: Vec<u8>,
    layout: Option<Layout>,
}

impl HeadScan {
    // Cold path, once per stream.
    #[allow(clippy::disallowed_methods)]
    pub fn new(len: Option<u64>) -> Self {
        HeadScan {
            len,
            scan: Scan::Tag,
            head: [0; TAG_HEAD],
            have: 0,
            tag: 0,
            window: Vec::with_capacity(WINDOW),
            layout: None,
        }
    }

    pub fn done(&self) -> bool {
        self.scan == Scan::Done
    }

    pub fn layout(&self) -> Option<Layout> {
        self.layout
    }

    pub fn feed(&mut self, mut bytes: &[u8]) -> Option<Index> {
        while !bytes.is_empty() {
            match self.scan {
                Scan::Tag => {
                    let n = (TAG_HEAD - self.have).min(bytes.len());
                    self.head[self.have..self.have + n].copy_from_slice(&bytes[..n]);
                    self.have += n;
                    if self.have < TAG_HEAD {
                        return None;
                    }
                    self.tag = crate::id3::v2_total_len(&self.head).map_or(0, |n| n as u64);
                    match self.tag {
                        0 => {
                            self.window.extend_from_slice(&self.head);
                            self.scan = Scan::Audio;
                        }
                        n => self.scan = Scan::Skip(n - TAG_HEAD as u64),
                    }
                    bytes = &bytes[n..];
                }
                Scan::Skip(left) => {
                    let n = left.min(bytes.len() as u64);
                    bytes = &bytes[n as usize..];
                    self.scan = if n == left { Scan::Audio } else { Scan::Skip(left - n) };
                }
                Scan::Audio => {
                    let n = (WINDOW - self.window.len()).min(bytes.len());
                    self.window.extend_from_slice(&bytes[..n]);
                    bytes = &bytes[n..];
                    if self.window.len() == WINDOW {
                        return self.end();
                    }
                }
                Scan::Done => return None,
            }
        }
        if self.scan == Scan::Skip(0) {
            self.scan = Scan::Audio;
        }
        None
    }

    // Cold path, once per stream.
    #[allow(clippy::disallowed_methods)]
    pub fn end(&mut self) -> Option<Index> {
        if self.scan == Scan::Done {
            return None;
        }
        let index = match self.scan {
            Scan::Audio => {
                let (index, layout) = head_of(&self.window, self.tag, self.len);
                self.layout = layout;
                index
            }
            _ => Index { file_len: self.len, ..Index::default() },
        };
        self.scan = Scan::Done;
        self.window = Vec::new();
        Some(index)
    }
}

pub fn publish(ctx: &mut Ctx, index: Index) {
    if let Some(ns) = index.duration.nanos() {
        ctx.set_duration(ns);
    }
    ctx.publish_index(index);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub first: u64,
    pub audio: u64,
    pub header: bool,
    pub spf: u32,
    pub rate: u32,
    pub stream: Header,
    /// Samples ahead of time zero, the header frame's plus any LAME and decoder delay.
    pub lead: u64,
    pub cbr: Option<(u64, u64)>,
    pub kbps: u32,
    pub frames: Option<u64>,
    pub preroll: u64,
    pub table: Option<u64>,
}

impl Layout {
    fn of(window: &[u8], tag: u64, props: &AudioProps) -> Option<Layout> {
        let off = props.first_frame_offset;
        let rest = window.get(off..)?;
        let stream = Header::parse(rest)?;
        let size = match tag_frame(rest, rest.len() < TAG_FRAME_PROBE) {
            TagFrame::Frame(n) => n,
            TagFrame::Need | TagFrame::Not => 0,
        };
        let header = size > 0;
        let spf = stream.samples_per_frame();
        let audio_head =
            window.get(off + size..).and_then(Header::parse).filter(|h| h.same_stream(&stream));
        let kbps = audio_head.and_then(|h| h.bitrate_kbps).unwrap_or(0);
        let cbr =
            audio_head.and_then(|h| constant(window, off + size, &h, props, header, &stream, rest));
        let delay = props.delay.map_or(0, |d| u64::from(d) + DECODER_DELAY);
        let mut layout = Layout {
            first: tag + off as u64,
            audio: tag + (off + size) as u64,
            header,
            spf,
            rate: stream.sample_rate_hz,
            stream,
            lead: u64::from(header) * u64::from(spf) + delay,
            cbr,
            kbps,
            frames: props.frames.map(u64::from),
            preroll: 0,
            table: None,
        };
        layout.preroll = layout.preroll_frames();
        Some(layout)
    }

    /// Frames to decode ahead so a frame comes out whole, reaching back over the bit reservoir
    /// (ISO/IEC 11172-3 2.4.2.7 `main_data_begin`) at the smallest frame.
    fn preroll_frames(&self) -> u64 {
        let reservoir: u64 = match (self.stream.layer, self.stream.lsf) {
            (3, false) => 511,
            (3, true) => 255,
            _ => 0,
        };
        let smallest = match self.cbr {
            Some((num, den)) => num / den,
            None => {
                u64::from(if self.stream.lsf { 8u32 } else { 32 })
                    * if self.stream.lsf { 72_000 } else { 144_000 }
                    / u64::from(self.rate.max(1))
            }
        };
        (2 + reservoir.div_ceil(smallest.max(1))).min(PREROLL_MAX)
    }

    pub fn coded(&self, k: u64) -> u64 {
        k.saturating_mul(u64::from(self.spf))
    }

    pub fn time_ns(&self, k: u64) -> u64 {
        let samples = self.coded(k).saturating_sub(self.lead);
        (u128::from(samples) * 1_000_000_000 / u128::from(self.rate.max(1)))
            .min(u128::from(u64::MAX)) as u64
    }

    pub fn byte_of(&self, k: u64) -> Option<u64> {
        let (num, den) = self.cbr?;
        let Some(m) = k.checked_sub(u64::from(self.header)) else {
            return Some(self.first);
        };
        let off = u128::from(m) * u128::from(num) / u128::from(den);
        u64::try_from(off).ok()?.checked_add(self.audio)
    }

    pub fn frame_at(&self, byte: u64) -> Option<u64> {
        if byte == self.first {
            return Some(0);
        }
        let (num, den) = self.cbr?;
        let from = u128::from(byte.checked_sub(self.audio)?);
        let m = (from * u128::from(den) * 2 + u128::from(num)) / (2 * u128::from(num));
        let k = u64::try_from(m).ok()?.checked_add(u64::from(self.header))?;
        (self.byte_of(k)?.abs_diff(byte) <= SLACK).then_some(k)
    }

    pub fn entry(&self, r: u64, byte: u64) -> IndexEntry {
        match r {
            0 => IndexEntry::key(0, byte),
            _ => IndexEntry {
                time_ns: self.time_ns(r + self.preroll),
                byte,
                restart: byte,
                restart_ns: self.time_ns(r),
            },
        }
    }

    fn step(&self, frames: u64) -> u64 {
        let second = (u64::from(self.rate) + u64::from(self.spf) / 2) / u64::from(self.spf.max(1));
        second.max(frames.div_ceil(MAX_ENTRIES)).max(1)
    }

    fn audio_frames(&self, len: Option<u64>) -> Option<u64> {
        let (num, den) = self.cbr?;
        let counted = len.and_then(|len| len.checked_sub(self.audio)).map(|n| {
            (u128::from(n) * u128::from(den) / u128::from(num)).min(u128::from(u64::MAX)) as u64
        });
        self.frames.or(counted)
    }

    // Cold path, once per stream.
    #[allow(clippy::disallowed_methods)]
    fn line(&self, len: Option<u64>) -> Option<Vec<IndexEntry>> {
        let frames = self.audio_frames(len)?;
        let step = self.step(frames);
        let header = u64::from(self.header);
        let mut entries = Vec::with_capacity((frames / step + 1).min(MAX_ENTRIES + 1) as usize);
        push_ascending(&mut entries, self.entry(0, self.first), len);
        let mut m = step;
        while m < frames {
            let r = m + header;
            let byte = self.byte_of(r)?.saturating_sub(MARGIN).max(self.audio);
            push_ascending(&mut entries, self.entry(r, byte), len);
            m += step;
        }
        Some(entries)
    }
}

/// The frame size when every frame is of one bitrate, by the window's frames and the VBR header.
fn constant(
    window: &[u8],
    at: usize,
    h: &Header,
    props: &AudioProps,
    header: bool,
    stream: &Header,
    first: &[u8],
) -> Option<(u64, u64)> {
    let size = h.frame_bytes()?;
    let mut pos = at;
    while let Some(next) = window.get(pos..).and_then(Header::parse) {
        if next.bitrate_kbps != h.bitrate_kbps || !next.same_stream(h) {
            return None;
        }
        pos += next.frame_len()?;
    }
    if !header {
        return Some(size);
    }
    let lines = match (props.stream_bytes, props.frames) {
        (Some(bytes), Some(frames)) => {
            let line = u128::from(frames) * u128::from(size.0) / u128::from(size.1);
            let audio = u128::from(bytes);
            let tag = (at - props.first_frame_offset) as u128;
            audio.abs_diff(line) <= u128::from(SLACK)
                || audio.abs_diff(line + tag) <= u128::from(SLACK)
        }
        _ => info(first, stream),
    };
    lines.then_some(size)
}

fn info(frame: &[u8], h: &Header) -> bool {
    let side = match (h.lsf, h.mono) {
        (false, true) => 17,
        (false, false) => 32,
        (true, true) => 9,
        (true, false) => 17,
    };
    let at = 4 + usize::from(h.crc) * 2 + side;
    frame.get(at..at + 4) == Some(b"Info")
}

pub fn index_of(audio: &[u8], tag: u64, len: Option<u64>) -> Index {
    head_of(audio, tag, len).0
}

// Cold path, once per stream.
#[allow(clippy::disallowed_methods)]
pub fn head_of(audio: &[u8], tag: u64, len: Option<u64>) -> (Index, Option<Layout>) {
    let none = Index { file_len: len, ..Index::default() };
    let tag = len.map_or(tag, |len| tag.min(len));
    let region = len.map_or(0, |len| len - tag);
    let Some(props) = probe_props(audio, region) else {
        return (none, None);
    };
    let mut layout = Layout::of(audio, tag, &props);
    let Some(duration_ns) = props.duration_ns.filter(|&d| d > 0) else {
        return (none, layout);
    };
    let duration = Timestamp(duration_ns);
    if let Some(entries) = layout.and_then(|l| l.line(len)).filter(|e| !e.is_empty()) {
        let complete = props.exact;
        return (Index { entries, file_len: len, duration, complete, ..Index::default() }, layout);
    }
    let start = tag.saturating_add(props.first_frame_offset as u64);
    let start = len.map_or(start, |len| start.min(len));
    let span = len.map(|len| len - start);
    // The table's fractions are of the encoder's length, unless the file was cut after encoding.
    let stated =
        props.stream_bytes.map(u64::from).filter(|&b| b > 0 && span.is_none_or(|s| b <= s));
    let mut entries = Vec::with_capacity(XingToc::LEN);
    let table = props.toc.zip(stated.or(span));
    match (table, span) {
        (Some((toc, bytes)), _) => {
            for i in 0..XingToc::LEN {
                if let (Some(t), Some(off)) = (toc.time_at(i, duration_ns), toc.byte_at(i, bytes)) {
                    push_ascending(
                        &mut entries,
                        IndexEntry::key(t, start.saturating_add(off)),
                        len,
                    );
                }
            }
        }
        (None, Some(span)) => {
            for i in 0..XingToc::LEN as u128 {
                let t = (i * u128::from(duration_ns) / XingToc::LEN as u128) as u64;
                let off = (i * u128::from(span) / XingToc::LEN as u128) as u64;
                push_ascending(&mut entries, IndexEntry::key(t, start.saturating_add(off)), len);
            }
        }
        (None, None) => {}
    }
    let complete = table.is_some() && !entries.is_empty();
    if let (Some(layout), Some((_, bytes))) = (layout.as_mut(), table.filter(|_| complete)) {
        layout.table = Some(bytes / 256);
    }
    (Index { entries, file_len: len, duration, complete, ..Index::default() }, layout)
}

/// Append only entries that move on in time and bytes, so a corrupt table cannot seek backwards.
fn push_ascending(entries: &mut Vec<IndexEntry>, entry: IndexEntry, len: Option<u64>) {
    if len.is_some_and(|len| entry.byte >= len) {
        return;
    }
    match entries.last() {
        Some(e) if entry.time_ns <= e.time_ns || entry.byte < e.byte => {}
        _ => entries.push(entry),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Walk {
    Seek(usize),
    Exact,
    Counting,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resync {
    Drop(usize),
    Frame { skip: usize, k: Option<u64> },
    GaveUp,
}

/// The frames of a stream as its decoder passes them, growing the index for a VBR stream.
pub struct Clock {
    layout: Layout,
    head: Index,
    grown: Vec<(u64, u64)>,
    told: usize,
    step: u64,
    broken: bool,
    fresh: bool,
    walk: Walk,
    next: u64,
    k: u64,
    bytes: [u8; 4],
    have: usize,
}

impl Clock {
    // Cold path, once per stream.
    #[allow(clippy::disallowed_methods)]
    pub fn new(layout: Layout, head: &Index) -> Clock {
        let frames = head.duration.nanos().map_or(0, |ns| {
            (u128::from(ns) * u128::from(layout.rate)
                / 1_000_000_000
                / u128::from(layout.spf.max(1))) as u64
        });
        let step = layout.step(frames);
        Clock {
            layout,
            head: head.clone(),
            grown: Vec::with_capacity((frames / step + 2).min(MAX_ENTRIES) as usize),
            told: 0,
            step,
            broken: false,
            fresh: false,
            walk: Walk::Seek(0),
            next: 0,
            k: 0,
            bytes: [0; 4],
            have: 0,
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn restart(&mut self) {
        self.walk = Walk::Seek(0);
    }

    pub fn seeking(&self) -> bool {
        matches!(self.walk, Walk::Seek(_))
    }

    pub fn lost(&mut self) {
        self.walk = Walk::Off;
    }

    pub fn frame_at(&self, byte: u64) -> Option<u64> {
        if let Some(k) = self.layout.frame_at(byte) {
            return Some(k);
        }
        let i = self.grown.binary_search_by_key(&byte, |g| g.1).ok()?;
        Some(self.grown[i].0)
    }

    /// The decode sample of the frame at `byte` by the Xing table. Points are rounded down to a
    /// 256th (LAME `Xing_seek_table`), so each is taken half a 256th on and interpolated.
    pub fn estimate(&self, byte: u64) -> Option<u64> {
        let half = self.layout.table? / 2;
        let points = &self.head.entries;
        let at =
            |i: usize| points.get(i).map(|e| (e.byte + if i == 0 { 0 } else { half }, e.time_ns));
        let end = self.head.file_len.zip(self.head.duration.nanos());
        let n = points.partition_point(|e| e.byte + half <= byte).max(1);
        let (b0, t0) = at(n - 1)?;
        let (b1, t1) = at(n).or(end)?;
        let ns = match b1 > b0 {
            true => {
                let along = u128::from(byte.saturating_sub(b0).min(b1 - b0));
                t0 + (along * u128::from(t1.saturating_sub(t0)) / u128::from(b1 - b0)) as u64
            }
            false => t0,
        };
        let samples = u128::from(ns) * u128::from(self.layout.rate) / 1_000_000_000;
        u64::try_from(samples).ok()?.checked_add(self.layout.lead)
    }

    pub fn resync(&mut self, buf: &[u8], base: u64) -> Resync {
        let Walk::Seek(seen) = self.walk else {
            return Resync::Drop(0);
        };
        if base < self.layout.first {
            let n = (self.layout.first - base).min(buf.len() as u64) as usize;
            return Resync::Drop(n);
        }
        let mut i = 0usize;
        let found = loop {
            match next_frame(&buf[i..]) {
                Framed::Frame { offset, header } if header.same_stream(&self.layout.stream) => {
                    break Some(i + offset);
                }
                Framed::Frame { offset, .. } => i += offset + 1,
                Framed::NeedMore { offset } => {
                    i += offset;
                    break None;
                }
                Framed::NoSync => {
                    // A sync word split at the end waits for its other bytes.
                    i = buf.len().saturating_sub(3).max(i);
                    break None;
                }
            }
        };
        match found {
            Some(skip) => {
                let at = base + skip as u64;
                let k = self.frame_at(at);
                (self.next, self.have) = (at, 0);
                self.k = k.unwrap_or(0);
                self.walk = if k.is_some() { Walk::Exact } else { Walk::Counting };
                Resync::Frame { skip, k }
            }
            None if seen + i > RESYNC_MAX => {
                self.walk = Walk::Off;
                Resync::GaveUp
            }
            None => {
                self.walk = Walk::Seek(seen + i);
                Resync::Drop(i)
            }
        }
    }

    pub fn walk(&mut self, bytes: &[u8], at: u64) -> bool {
        let end = at + bytes.len() as u64;
        let mut due = false;
        while matches!(self.walk, Walk::Exact | Walk::Counting) {
            let pos = self.next + self.have as u64;
            if pos >= end {
                break;
            }
            if pos < at {
                self.walk = Walk::Off;
                break;
            }
            let i = (pos - at) as usize;
            let n = (4 - self.have).min(bytes.len() - i);
            self.bytes[self.have..self.have + n].copy_from_slice(&bytes[i..i + n]);
            self.have += n;
            if self.have < 4 {
                break;
            }
            self.have = 0;
            let framed = Header::parse(&self.bytes)
                .filter(|h| h.same_stream(&self.layout.stream))
                .and_then(|h| Some((h, h.frame_len().filter(|&n| n >= 4)?)));
            let Some((header, len)) = framed else {
                self.walk = Walk::Off;
                break;
            };
            due |= self.passed(&header);
            self.next += len as u64;
            self.k += 1;
        }
        due
    }

    fn passed(&mut self, header: &Header) -> bool {
        let audio = self.k >= u64::from(self.layout.header);
        if self.layout.cbr.is_some() && audio && header.bitrate_kbps != Some(self.layout.kbps) {
            self.layout.cbr = None;
            (self.broken, self.fresh) = (true, true);
            return true;
        }
        if self.walk != Walk::Exact
            || self.layout.cbr.is_some()
            || self.grown.len() == self.grown.capacity()
        {
            return false;
        }
        let on = self.k.is_multiple_of(self.step) && self.grown.last().is_none_or(|g| self.k > g.0);
        if on {
            self.grown.push((self.k, self.next));
        }
        on && self.grown.len() - self.told >= GROW_EVERY
    }

    // Cold path, once per publication.
    #[allow(clippy::disallowed_methods)]
    pub fn index(&mut self) -> Option<Index> {
        if self.grown.len() == self.told && !self.fresh {
            return None;
        }
        let len = self.head.file_len;
        let mut entries = Vec::with_capacity(self.grown.len() + self.head.entries.len());
        let first = self.grown.first().map(|&(r, byte)| self.layout.entry(r, byte));
        for &e in self
            .head
            .entries
            .iter()
            .filter(|e| first.is_none_or(|f| e.time_ns < f.time_ns && e.byte < f.byte))
        {
            push_ascending(&mut entries, e, len);
        }
        for &(r, byte) in &self.grown {
            push_ascending(&mut entries, self.layout.entry(r, byte), len);
        }
        for &e in &self.head.entries {
            push_ascending(&mut entries, e, len);
        }
        (self.told, self.fresh) = (self.grown.len(), false);
        let complete = self.head.complete && !self.broken;
        let head = &self.head;
        Some(Index {
            entries,
            file_len: len,
            duration: head.duration,
            complete,
            stream: head.stream,
            grouped: head.grouped,
        })
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests;
