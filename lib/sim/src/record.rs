//! What a fake sink writes down. One fixed-size POD row per observation, in arrival order,
//! shared by every sink of a run.

use crate::time::{NONE, Ns};

pub const MAX_SINKS: usize = 16;

pub const MAX_STREAMS: usize = 16;

pub const NO_STREAM: u16 = u16::MAX;

pub mod flag {
    pub const KEYFRAME: u16 = 1 << 0;
    pub const DISCONT: u16 = 1 << 1;
    pub const GAP: u16 = 1 << 2;
    pub const DELTA: u16 = 1 << 3;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RowKind {
    /// `aux` is the running time of the take, at or past the pts on a paced sink.
    Buffer = 0,
    /// Dropped as late, `aux` is the lateness. Counts as an arrival, so it is no sequence gap.
    Drop = 1,
    Format = 2,
    Tags = 3,
    Segment = 4,
    Gap = 5,
    Patch = 6,
    Eos = 7,
    FlushStart = 8,
    FlushStop = 9,
    Paused = 10,
    Resumed = 11,
    PropChanged = 12,
    Start = 13,
    Stop = 14,
    /// An action fired inside a sink call. `seq` is its index, `aux` the log's answer.
    Command = 15,
    Skip = 16,
    Before = 17,
    Decoded = 18,
    DecodeOnly = 19,
    Skipped = 20,
    RateAsked = 21,
    Retimed = 22,
    Retime = 23,
    Offset = 24,
}

pub mod class {
    use super::RowKind;

    pub const fn bit(kind: RowKind) -> u32 {
        1 << kind as u32
    }

    pub const ARRIVAL: u32 = bit(RowKind::Buffer) | bit(RowKind::Drop) | bit(RowKind::Before);
    pub const PICTURE: u32 =
        bit(RowKind::Decoded) | bit(RowKind::DecodeOnly) | bit(RowKind::Skipped);
    pub const TIMED: u32 = ARRIVAL | bit(RowKind::Gap);
    pub const FLUSH: u32 = bit(RowKind::FlushStart) | bit(RowKind::FlushStop);
    pub const STATE: u32 = bit(RowKind::Format) | bit(RowKind::Tags);
    pub const POSITIONAL: u32 =
        bit(RowKind::Segment) | bit(RowKind::Gap) | bit(RowKind::Patch) | bit(RowKind::Eos);
    pub const OUT_OF_BAND: u32 = bit(RowKind::Start)
        | bit(RowKind::Stop)
        | bit(RowKind::Paused)
        | bit(RowKind::Resumed)
        | bit(RowKind::PropChanged)
        | bit(RowKind::Command)
        | bit(RowKind::Skip)
        | bit(RowKind::Offset);
}

impl RowKind {
    pub const fn is(self, class: u32) -> bool {
        class::bit(self) & class != 0
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Buffer => "buffer",
            Self::Drop => "drop",
            Self::Format => "format",
            Self::Tags => "tags",
            Self::Segment => "segment",
            Self::Gap => "gap",
            Self::Patch => "patch",
            Self::Eos => "eos",
            Self::FlushStart => "flush-start",
            Self::FlushStop => "flush-stop",
            Self::Paused => "paused",
            Self::Resumed => "resumed",
            Self::PropChanged => "prop-changed",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Command => "command",
            Self::Skip => "skip",
            Self::Before => "before-point",
            Self::Decoded => "decoded",
            Self::DecodeOnly => "decode-only",
            Self::Skipped => "skipped",
            Self::RateAsked => "rate-asked",
            Self::Retimed => "retimed",
            Self::Retime => "retime",
            Self::Offset => "offset",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Row {
    pub pts: Ns,
    pub dts: Ns,
    pub duration: Ns,
    pub at: Ns,
    pub seq: u64,
    pub aux: u64,
    pub seek_gen: u32,
    pub size: u32,
    pub sink: u16,
    pub stream: u16,
    pub flags: u16,
    pub kind: RowKind,
    pub format: u8,
}

const _: () = assert!(core::mem::size_of::<Row>() == 64);
const _: () = assert!(core::mem::align_of::<Row>() == 8);
const _: () = assert!(!core::mem::needs_drop::<Row>());

impl Row {
    pub const fn new(kind: RowKind, sink: u16, stream: u16) -> Self {
        Self {
            pts: NONE,
            dts: NONE,
            duration: NONE,
            at: 0,
            seq: 0,
            aux: 0,
            seek_gen: 0,
            size: 0,
            sink,
            stream,
            flags: 0,
            kind,
            format: 0,
        }
    }

    pub const fn has(&self, flags: u16) -> bool {
        self.flags & flags == flags
    }
}

/// At capacity a push is refused and counted, and the sweep reports the overflow.
pub struct Recording {
    slots: Box<[Row]>,
    len: usize,
    overflow: u64,
    buffers: [u32; MAX_SINKS],
    ends: [u32; MAX_SINKS],
}

impl Recording {
    pub fn with_capacity(rows: usize) -> Self {
        let slots = vec![Row::new(RowKind::Stop, 0, NO_STREAM); rows].into_boxed_slice();
        Self { slots, len: 0, overflow: 0, buffers: [0; MAX_SINKS], ends: [0; MAX_SINKS] }
    }

    pub fn reset(&mut self) {
        self.len = 0;
        self.overflow = 0;
        self.buffers = [0; MAX_SINKS];
        self.ends = [0; MAX_SINKS];
    }

    #[inline]
    pub fn push(&mut self, row: Row) -> bool {
        let Some(slot) = self.slots.get_mut(self.len) else {
            self.overflow += 1;
            return false;
        };
        *slot = row;
        self.len += 1;
        let sink = row.sink as usize;
        if row.kind == RowKind::Buffer && sink < MAX_SINKS {
            self.buffers[sink] += 1;
        }
        if row.kind == RowKind::Eos && sink < MAX_SINKS {
            self.ends[sink] += 1;
        }
        true
    }

    pub fn rows(&self) -> &[Row] {
        &self.slots[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn overflow(&self) -> u64 {
        self.overflow
    }

    pub fn buffers_on(&self, sink: u16) -> u32 {
        self.buffers.get(sink as usize).copied().unwrap_or(0)
    }

    pub fn ends_on(&self, sink: u16) -> u32 {
        self.ends.get(sink as usize).copied().unwrap_or(0)
    }
}

pub fn checksum(rows: &[Row]) -> u64 {
    // FNV-1a over the fields as words (Fowler, Noll, Vo), each word mixed first, since FNV only
    // carries a difference upward and the high fields, sink, stream and seek_gen, would collide.
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for row in rows {
        let tail = (row.seek_gen as u64) << 32 | row.size as u64;
        let ids = (row.sink as u64) << 48
            | (row.stream as u64) << 32
            | (row.flags as u64) << 16
            | (row.format as u64) << 8
            | row.kind as u64;
        for word in [row.pts, row.dts, row.duration, row.at, row.seq, row.aux, tail, ids] {
            hash = (hash ^ crate::prng::mix(word)).wrapping_mul(PRIME);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(sink: u16, seq: u64) -> Row {
        Row { seq, pts: seq * 10, ..Row::new(RowKind::Buffer, sink, 0) }
    }

    #[test]
    fn a_full_table_counts_what_it_refuses_and_never_grows() {
        let mut rec = Recording::with_capacity(4);
        let base = rec.rows().as_ptr();
        for seq in 0..10 {
            assert_eq!(rec.push(buffer(0, seq)), seq < 4);
        }
        assert_eq!(rec.len(), 4);
        assert_eq!(rec.capacity(), 4);
        assert_eq!(rec.overflow(), 6);
        assert_eq!(rec.rows().as_ptr(), base);
        assert_eq!(rec.rows()[3].seq, 3);
    }

    #[test]
    fn reset_keeps_the_memory_and_clears_the_run() {
        let mut rec = Recording::with_capacity(8);
        let base = rec.rows().as_ptr();
        for seq in 0..12 {
            rec.push(buffer(1, seq));
        }
        rec.reset();
        assert!(rec.is_empty());
        assert_eq!(rec.overflow(), 0);
        assert_eq!(rec.buffers_on(1), 0);
        assert_eq!(rec.ends_on(1), 0);
        assert_eq!(rec.capacity(), 8);
        assert!(rec.push(buffer(1, 0)));
        assert_eq!(rec.rows().as_ptr(), base);
    }

    #[test]
    fn buffer_counts_are_per_sink_and_ignore_other_kinds() {
        let mut rec = Recording::with_capacity(16);
        rec.push(Row::new(RowKind::Start, 0, NO_STREAM));
        rec.push(Row::new(RowKind::Format, 0, 0));
        rec.push(buffer(0, 0));
        rec.push(buffer(0, 1));
        rec.push(buffer(2, 0));
        rec.push(Row::new(RowKind::Drop, 2, 0));
        rec.push(buffer(MAX_SINKS as u16, 0));
        assert_eq!(rec.buffers_on(0), 2);
        assert_eq!(rec.buffers_on(1), 0);
        assert_eq!(rec.buffers_on(2), 1);
        assert_eq!(rec.buffers_on(MAX_SINKS as u16), 0);
        assert_eq!(rec.len(), 7);
        assert_eq!((rec.ends_on(0), rec.ends_on(2)), (0, 0));
        rec.push(Row::new(RowKind::Eos, 2, 0));
        assert_eq!((rec.ends_on(0), rec.ends_on(2)), (0, 1));
    }

    #[test]
    fn kind_classes_split_state_from_positional() {
        assert!(RowKind::Format.is(class::STATE));
        assert!(RowKind::Tags.is(class::STATE));
        assert!(!RowKind::Segment.is(class::STATE));
        assert!(RowKind::Segment.is(class::POSITIONAL));
        assert!(RowKind::Eos.is(class::POSITIONAL));
        assert!(RowKind::Drop.is(class::ARRIVAL));
        assert!(RowKind::Before.is(class::ARRIVAL | class::TIMED));
        assert!(RowKind::Skip.is(class::OUT_OF_BAND) && !RowKind::Skip.is(class::FLUSH));
        assert!(RowKind::DecodeOnly.is(class::PICTURE) && !RowKind::Decoded.is(class::ARRIVAL));
        assert!(RowKind::Gap.is(class::TIMED));
        assert!(!RowKind::Gap.is(class::ARRIVAL));
        assert!(RowKind::FlushStop.is(class::FLUSH));
        assert!(RowKind::Paused.is(class::OUT_OF_BAND));
        assert!(!RowKind::Buffer.is(class::OUT_OF_BAND));
    }

    #[test]
    fn checksum_sees_order_and_every_field() {
        let a = buffer(0, 1);
        let b = buffer(0, 2);
        assert_eq!(checksum(&[a, b]), checksum(&[a, b]));
        assert_ne!(checksum(&[a, b]), checksum(&[b, a]));
        assert_ne!(checksum(&[a]), checksum(&[]));
        let variants = [
            Row { pts: 1, ..a },
            Row { dts: 1, ..a },
            Row { duration: 1, ..a },
            Row { at: 1, ..a },
            Row { seq: 9, ..a },
            Row { aux: 1, ..a },
            Row { seek_gen: 1, ..a },
            Row { size: 1, ..a },
            Row { sink: 1, ..a },
            Row { stream: 1, ..a },
            Row { flags: flag::DISCONT, ..a },
            Row { format: 1, ..a },
            Row { kind: RowKind::Drop, ..a },
        ];
        for variant in variants {
            assert_ne!(checksum(&[a]), checksum(&[variant]), "{variant:?}");
        }
    }
}
