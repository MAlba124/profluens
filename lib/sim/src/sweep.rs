//! The oracles, as a sweep over a finished recording. Each rule returns the first row breaking
//! it as a typed value, without formatting or allocating.

use core::fmt;

use crate::record::{MAX_SINKS, MAX_STREAMS, Recording, Row, RowKind, class, flag};
use crate::time::{NONE, Ns};

pub const NO_ROW: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Rule {
    FormatBeforeBuffer = 0,
    SegmentBeforeBuffer = 1,
    TimestampsMonotonic = 2,
    NothingAfterEos = 3,
    EosOnce = 4,
    NoStaleGeneration = 5,
    DiscontAfterFlush = 6,
    NoDuplicateSeq = 7,
    NoMisrouting = 8,
    NoUndeclaredGap = 9,
}

impl Rule {
    pub const COUNT: usize = 10;

    pub const ALL: [Rule; Rule::COUNT] = [
        Rule::FormatBeforeBuffer,
        Rule::SegmentBeforeBuffer,
        Rule::TimestampsMonotonic,
        Rule::NothingAfterEos,
        Rule::EosOnce,
        Rule::NoStaleGeneration,
        Rule::DiscontAfterFlush,
        Rule::NoDuplicateSeq,
        Rule::NoMisrouting,
        Rule::NoUndeclaredGap,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::FormatBeforeBuffer => "format_before_buffer",
            Self::SegmentBeforeBuffer => "segment_before_buffer",
            Self::TimestampsMonotonic => "timestamps_monotonic",
            Self::NothingAfterEos => "nothing_after_eos",
            Self::EosOnce => "eos_once",
            Self::NoStaleGeneration => "no_stale_generation",
            Self::DiscontAfterFlush => "discont_after_flush",
            Self::NoDuplicateSeq => "no_duplicate_seq",
            Self::NoMisrouting => "no_misrouting",
            Self::NoUndeclaredGap => "no_undeclared_gap",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SinkExpect {
    pub streams: u16,
}

/// Sequence numbers a scenario allows to be missing on one stream at one sink, both ends included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GapWindow {
    pub sink: u16,
    pub stream: u16,
    pub first: u64,
    pub last: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Expect<'a> {
    pub sinks: &'a [SinkExpect],
    pub gaps: &'a [GapWindow],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    NoFormat,
    NoSegment { since: u32 },
    Backwards { prev: u32, prev_ts: Ns, ts: Ns },
    AfterEos { eos: u32 },
    StaleGeneration { flush: u32, flush_gen: u32 },
    NotDiscont { flush: u32 },
    Repeated { prev: u32, prev_seq: u64 },
    UnknownSink { sinks: u16 },
    UnexpectedStream { expected: u16 },
    Gap { prev: u32, prev_seq: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Violation {
    pub rule: Rule,
    pub index: u32,
    pub row: Row,
    pub cause: Cause,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepError {
    Overflow { dropped: u64, capacity: u32 },
    Violation(Violation),
}

fn violation(rule: Rule, index: usize, row: &Row, cause: Cause) -> Violation {
    Violation { rule, index: index as u32, row: *row, cause }
}

#[inline]
fn tracked(row: &Row) -> Option<usize> {
    let sink = row.sink as usize;
    (sink < MAX_SINKS).then_some(sink)
}

#[inline]
fn resets(kind: RowKind) -> bool {
    kind.is(class::FLUSH | class::bit(RowKind::Start))
}

pub fn format_before_buffer(rows: &[Row]) -> Result<(), Violation> {
    let mut have = [false; MAX_SINKS];
    for (i, row) in rows.iter().enumerate() {
        let Some(sink) = tracked(row) else { continue };
        if row.kind == RowKind::Format {
            have[sink] = true;
        } else if row.kind.is(class::TIMED) && !have[sink] {
            return Err(violation(Rule::FormatBeforeBuffer, i, row, Cause::NoFormat));
        }
    }
    Ok(())
}

pub fn segment_before_buffer(rows: &[Row]) -> Result<(), Violation> {
    let mut have = [false; MAX_SINKS];
    let mut since = [NO_ROW; MAX_SINKS];
    for (i, row) in rows.iter().enumerate() {
        let Some(sink) = tracked(row) else { continue };
        if row.kind == RowKind::Segment {
            have[sink] = true;
        } else if resets(row.kind) {
            have[sink] = false;
            since[sink] = i as u32;
        } else if row.kind.is(class::TIMED) && !have[sink] {
            let cause = Cause::NoSegment { since: since[sink] };
            return Err(violation(Rule::SegmentBeforeBuffer, i, row, cause));
        }
    }
    Ok(())
}

/// Dts when the buffer has one, since encoded video is in decode order, else pts.
#[inline]
fn order_ts(row: &Row) -> Ns {
    if row.dts != NONE { row.dts } else { row.pts }
}

pub fn timestamps_monotonic(rows: &[Row]) -> Result<(), Violation> {
    let mut last = [(NO_ROW, 0 as Ns); MAX_SINKS];
    for (i, row) in rows.iter().enumerate() {
        let Some(sink) = tracked(row) else { continue };
        if row.kind == RowKind::Segment || resets(row.kind) {
            last[sink].0 = NO_ROW;
            continue;
        }
        if !row.kind.is(class::ARRIVAL) {
            continue;
        }
        let ts = order_ts(row);
        if ts == NONE {
            continue;
        }
        let (prev, prev_ts) = last[sink];
        if prev != NO_ROW && ts < prev_ts {
            let cause = Cause::Backwards { prev, prev_ts, ts };
            return Err(violation(Rule::TimestampsMonotonic, i, row, cause));
        }
        last[sink] = (i as u32, ts);
    }
    Ok(())
}

const AFTER_EOS: u32 =
    class::TIMED | class::STATE | class::bit(RowKind::Segment) | class::bit(RowKind::Patch);

pub fn nothing_after_eos(rows: &[Row]) -> Result<(), Violation> {
    let mut eos = [NO_ROW; MAX_SINKS];
    for (i, row) in rows.iter().enumerate() {
        let Some(sink) = tracked(row) else { continue };
        if row.kind == RowKind::Eos {
            eos[sink] = eos[sink].min(i as u32);
        } else if resets(row.kind) {
            eos[sink] = NO_ROW;
        } else if row.kind.is(AFTER_EOS) && eos[sink] != NO_ROW {
            let cause = Cause::AfterEos { eos: eos[sink] };
            return Err(violation(Rule::NothingAfterEos, i, row, cause));
        }
    }
    Ok(())
}

pub fn eos_once(rows: &[Row]) -> Result<(), Violation> {
    let mut eos = [NO_ROW; MAX_SINKS];
    for (i, row) in rows.iter().enumerate() {
        let Some(sink) = tracked(row) else { continue };
        if resets(row.kind) {
            eos[sink] = NO_ROW;
        } else if row.kind == RowKind::Eos {
            if eos[sink] != NO_ROW {
                let cause = Cause::AfterEos { eos: eos[sink] };
                return Err(violation(Rule::EosOnce, i, row, cause));
            }
            eos[sink] = i as u32;
        }
    }
    Ok(())
}

pub fn no_stale_generation(rows: &[Row]) -> Result<(), Violation> {
    let mut latest = [(NO_ROW, 0u32); MAX_SINKS];
    for (i, row) in rows.iter().enumerate() {
        let Some(sink) = tracked(row) else { continue };
        if row.kind == RowKind::Start {
            latest[sink].0 = NO_ROW;
        }
        if row.kind.is(class::OUT_OF_BAND) {
            continue;
        }
        let (flush, flush_gen) = latest[sink];
        if flush != NO_ROW && row.seek_gen < flush_gen {
            let cause = Cause::StaleGeneration { flush, flush_gen };
            return Err(violation(Rule::NoStaleGeneration, i, row, cause));
        }
        if row.kind.is(class::FLUSH) {
            latest[sink] = (i as u32, row.seek_gen);
        }
    }
    Ok(())
}

/// Per sink, the flush and a bit for each stream yet to arrive since it. Every stream's first
/// buffer after a flush is a discontinuity, not only the first on the sink.
pub fn discont_after_flush(rows: &[Row]) -> Result<(), Violation> {
    let mut pending = [(NO_ROW, 0u32); MAX_SINKS];
    for (i, row) in rows.iter().enumerate() {
        let Some(sink) = tracked(row) else { continue };
        if row.kind.is(class::FLUSH) {
            pending[sink] = (i as u32, u32::MAX);
        } else if row.kind == RowKind::Start {
            pending[sink] = (NO_ROW, 0);
        } else if row.kind.is(class::ARRIVAL) && pending[sink].0 != NO_ROW {
            // A streamless row takes the last bit, the sweep reports it on its own.
            let bit = 1u32 << (row.stream as usize).min(31);
            if pending[sink].1 & bit != 0 {
                if !row.has(flag::DISCONT) {
                    let cause = Cause::NotDiscont { flush: pending[sink].0 };
                    return Err(violation(Rule::DiscontAfterFlush, i, row, cause));
                }
                pending[sink].1 &= !bit;
            }
        }
    }
    Ok(())
}

/// A flush or a start begins a new run, so a seek that replays or skips buffers pairs nothing.
#[inline]
fn each_successor(
    rows: &[Row],
    mut check: impl FnMut(usize, &Row, u32, u64) -> Result<(), Violation>,
) -> Result<(), Violation> {
    let mut last = [(NO_ROW, 0u64); MAX_SINKS * MAX_STREAMS];
    for (i, row) in rows.iter().enumerate() {
        let Some(sink) = tracked(row) else { continue };
        let of_sink = &mut last[sink * MAX_STREAMS..][..MAX_STREAMS];
        if resets(row.kind) {
            of_sink.fill((NO_ROW, 0));
            continue;
        }
        if !row.kind.is(class::ARRIVAL) {
            continue;
        }
        let Some(slot) = of_sink.get_mut(row.stream as usize) else {
            continue;
        };
        if slot.0 != NO_ROW {
            check(i, row, slot.0, slot.1)?;
        }
        *slot = (i as u32, row.seq);
    }
    Ok(())
}

pub fn no_duplicate_seq(rows: &[Row]) -> Result<(), Violation> {
    each_successor(rows, |i, row, prev, prev_seq| {
        if row.seq > prev_seq {
            return Ok(());
        }
        Err(violation(Rule::NoDuplicateSeq, i, row, Cause::Repeated { prev, prev_seq }))
    })
}

pub fn no_misrouting(rows: &[Row], expect: &Expect) -> Result<(), Violation> {
    let sinks = expect.sinks.len().min(MAX_SINKS);
    for (i, row) in rows.iter().enumerate() {
        let sink = row.sink as usize;
        if sink >= sinks {
            let cause = Cause::UnknownSink { sinks: sinks as u16 };
            return Err(violation(Rule::NoMisrouting, i, row, cause));
        }
        if !row.kind.is(class::ARRIVAL) {
            continue;
        }
        let expected = expect.sinks[sink].streams;
        let known = (row.stream as usize) < MAX_STREAMS;
        if !known || expected & (1 << row.stream) == 0 {
            let cause = Cause::UnexpectedStream { expected };
            return Err(violation(Rule::NoMisrouting, i, row, cause));
        }
    }
    Ok(())
}

fn gap_declared(expect: &Expect, row: &Row, first: u64, last: u64) -> bool {
    expect.gaps.iter().any(|window| {
        window.sink == row.sink
            && window.stream == row.stream
            && window.first <= first
            && last <= window.last
    })
}

pub fn no_undeclared_gap(rows: &[Row], expect: &Expect) -> Result<(), Violation> {
    each_successor(rows, |i, row, prev, prev_seq| {
        // At or below the previous number is the duplicate rule's business.
        if row.seq <= prev_seq || row.seq - prev_seq == 1 {
            return Ok(());
        }
        if gap_declared(expect, row, prev_seq + 1, row.seq - 1) {
            return Ok(());
        }
        Err(violation(Rule::NoUndeclaredGap, i, row, Cause::Gap { prev, prev_seq }))
    })
}

pub fn run(rule: Rule, rows: &[Row], expect: &Expect) -> Result<(), Violation> {
    match rule {
        Rule::FormatBeforeBuffer => format_before_buffer(rows),
        Rule::SegmentBeforeBuffer => segment_before_buffer(rows),
        Rule::TimestampsMonotonic => timestamps_monotonic(rows),
        Rule::NothingAfterEos => nothing_after_eos(rows),
        Rule::EosOnce => eos_once(rows),
        Rule::NoStaleGeneration => no_stale_generation(rows),
        Rule::DiscontAfterFlush => discont_after_flush(rows),
        Rule::NoDuplicateSeq => no_duplicate_seq(rows),
        Rule::NoMisrouting => no_misrouting(rows, expect),
        Rule::NoUndeclaredGap => no_undeclared_gap(rows, expect),
    }
}

pub fn each(rows: &[Row], expect: &Expect) -> [Option<Violation>; Rule::COUNT] {
    Rule::ALL.map(|rule| run(rule, rows, expect).err())
}

/// Rules tie in [`Rule::ALL`] order.
pub fn all(rows: &[Row], expect: &Expect) -> Result<(), Violation> {
    let mut first: Option<Violation> = None;
    for rule in Rule::ALL {
        let Err(found) = run(rule, rows, expect) else {
            continue;
        };
        if first.is_none_or(|kept| found.index < kept.index) {
            first = Some(found);
        }
    }
    first.map_or(Ok(()), Err)
}

pub fn check(recording: &Recording, expect: &Expect) -> Result<(), SweepError> {
    if recording.overflow() > 0 {
        let capacity = recording.capacity() as u32;
        return Err(SweepError::Overflow { dropped: recording.overflow(), capacity });
    }
    all(recording.rows(), expect).map_err(SweepError::Violation)
}

struct Ts(Ns);

impl fmt::Display for Ts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == NONE {
            return f.write_str("none");
        }
        write!(f, "{}ns", self.0)
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let row = &self.row;
        write!(
            f,
            "{} at row {} ({} sink {} stream {} seq {} pts {} dts {} gen {} flags {:#x} at {}), ",
            self.rule,
            self.index,
            row.kind.name(),
            row.sink,
            row.stream,
            row.seq,
            Ts(row.pts),
            Ts(row.dts),
            row.seek_gen,
            row.flags,
            Ts(row.at),
        )?;
        match self.cause {
            Cause::NoFormat => f.write_str("no format event reached the sink before it"),
            Cause::NoSegment { since: NO_ROW } => {
                f.write_str("no segment event reached the sink before it")
            }
            Cause::NoSegment { since } => {
                write!(f, "no segment event since the flush or start at row {since}")
            }
            Cause::Backwards { prev, prev_ts, ts } => {
                write!(
                    f,
                    "timestamp {} is behind {} at row {prev} in the same segment",
                    Ts(ts),
                    Ts(prev_ts)
                )
            }
            Cause::AfterEos { eos } => {
                write!(f, "the sink ended at row {eos} and nothing reopened it")
            }
            Cause::StaleGeneration { flush, flush_gen } => {
                write!(f, "the flush at row {flush} started generation {flush_gen}")
            }
            Cause::NotDiscont { flush } => {
                write!(
                    f,
                    "first buffer after the flush at row {flush} and not flagged discontinuous"
                )
            }
            Cause::Repeated { prev, prev_seq } => {
                write!(f, "the stream already delivered seq {prev_seq} at row {prev}")
            }
            Cause::UnknownSink { sinks } => write!(f, "the scenario declares {sinks} sinks"),
            Cause::UnexpectedStream { expected } => {
                write!(f, "the sink expects only streams in mask {expected:#06x}")
            }
            Cause::Gap { prev, prev_seq } => {
                write!(
                    f,
                    "the stream's last buffer was seq {prev_seq} at row {prev} and no gap is declared"
                )
            }
        }
    }
}

impl fmt::Display for SweepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow { dropped, capacity } => {
                write!(f, "recording overflowed, {dropped} rows dropped past {capacity}")
            }
            Self::Violation(violation) => violation.fmt(f),
        }
    }
}

impl std::error::Error for Violation {}
impl std::error::Error for SweepError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_log_breaks_no_rule() {
        let expect = Expect { sinks: &[], gaps: &[] };
        assert_eq!(all(&[], &expect), Ok(()));
        assert!(each(&[], &expect).iter().all(Option::is_none));
    }

    #[test]
    fn rule_names_and_indices_line_up() {
        for (i, rule) in Rule::ALL.iter().enumerate() {
            assert_eq!(*rule as usize, i);
            assert!(!rule.name().is_empty());
        }
    }

    #[test]
    fn an_overflowed_recording_is_refused_not_swept() {
        let mut rec = Recording::with_capacity(1);
        let expect = Expect { sinks: &[SinkExpect { streams: 1 }], gaps: &[] };
        rec.push(Row::new(RowKind::Start, 0, 0));
        assert_eq!(check(&rec, &expect), Ok(()));
        rec.push(Row::new(RowKind::Stop, 0, 0));
        assert_eq!(check(&rec, &expect), Err(SweepError::Overflow { dropped: 1, capacity: 1 }));
    }
}
