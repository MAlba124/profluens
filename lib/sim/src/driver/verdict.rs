//! What a failed oracle returns. Plain data, compared by kind when shrinking, formatted only
//! when printed.

use core::fmt;

use super::model::Answer;
use super::offers::Fixed;
use crate::record::Row;
use crate::sweep::{Rule, Violation};
use crate::time::{NONE, Ns};

pub const NO_SINK: u16 = u16::MAX;
pub const NO_ROW: u32 = u32::MAX;

/// Which oracle failed. The shrinker keeps this fixed while it cuts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    BadScenario,
    /// The run panicked, in core or a fake. The farm catches it and names the seed.
    Panicked,
    BuildFailed,
    LinkRefused,
    LinkMade,
    LinkFormat,
    RefusalUnnamed,
    Rejection,
    Unoffered,
    UnexpectedError,
    NetBytes,
    /// An op the source had cancelled completed with bytes, so data crossed a flush.
    NetStale,
    NoQuiescence,
    Stall,
    DeadlineBehind,
    PassAfterEnd,
    LeftEarly,
    FinishTakenBack,
    FinishedEarly,
    RowAfterEnd,
    EndUnclear,
    PoolOverCap,
    PoolLeak,
    LinkOverLimit,
    LinkOverCap,
    /// A retimer switched to a rate later than the limit, its lag and a few buffers allow.
    ControlLate,
    HeldAtEnd,
    CommandNotApplied,
    Refusal,
    LogHeld,
    Bus,
    RecordingOverflow,
    Rule(Rule),
    Lifecycle,
    Transport,
    FlushOrder,
    FlushTarget,
    Generation,
    Format,
    Content,
    Repeated,
    Missing,
    Beyond,
    Incomplete,
    NotEnded,
    Paced,
    DecodedTwice,
    Undecoded,
    /// A decoder kept back a picture at or after its point, skipped a referenced one, or decoded
    /// without output an unreferenced one.
    Withheld,
    BeforePoint,
    PastPoint,
    /// After a rule-chosen skip, a decoder kept back a picture a seek to the target keyframe would
    /// not have decoded.
    Overdecoded,
    SkipUnsent,
    MissingReference,
    OffTimeline,
    Position,
    OffsetUnsent,
    RateUnsent,
    Retimed,
    Unretimed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waits {
    Unrun,
    Progressed,
    Idle { until: Ns },
    BlockedRing { pad: u32 },
    BlockedPool { element: u32 },
    BlockedIo,
    Exited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detail {
    None,
    Violation(Violation),
    Row {
        row: Row,
        want: u64,
        got: u64,
    },
    Numbers {
        want: u64,
        got: u64,
    },
    Group {
        group: u16,
        waits: Waits,
        held: u32,
    },
    Action {
        index: u8,
        answer: Answer,
    },
    /// `tap_full` is a sink that made more rows in one pass than its tap holds.
    Element {
        element: u32,
        tap_full: bool,
    },
    Link {
        link: u8,
        want: Fixed,
        got: Fixed,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finding {
    pub kind: Kind,
    pub sink: u16,
    pub row: u32,
    pub detail: Detail,
}

impl Finding {
    pub const fn run(kind: Kind, detail: Detail) -> Finding {
        Finding { kind, sink: NO_SINK, row: NO_ROW, detail }
    }

    pub const fn numbers(kind: Kind, want: u64, got: u64) -> Finding {
        Finding::run(kind, Detail::Numbers { want, got })
    }

    pub const fn at_row(kind: Kind, index: usize, row: &Row, want: u64, got: u64) -> Finding {
        let detail = Detail::Row { row: *row, want, got };
        Finding { kind, sink: row.sink, row: index as u32, detail }
    }

    pub const fn violation(found: Violation) -> Finding {
        let detail = Detail::Violation(found);
        Finding { kind: Kind::Rule(found.rule), sink: found.row.sink, row: found.index, detail }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub seed: u64,
    /// The step the run was in when the oracle failed. For a row of the
    /// recording, the step that recorded it.
    pub step: u32,
    pub kind: Kind,
    pub sink: u16,
    pub row: u32,
    pub detail: Detail,
}

impl Verdict {
    pub const fn new(seed: u64, step: u32, found: Finding) -> Verdict {
        let Finding { kind, sink, row, detail } = found;
        Verdict { seed, step, kind, sink, row, detail }
    }
}

struct Ts(Ns);

impl fmt::Display for Ts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            NONE => f.write_str("none"),
            ns => write!(f, "{ns}ns"),
        }
    }
}

impl fmt::Display for Waits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Waits::Unrun => f.write_str("has not run"),
            Waits::Progressed => f.write_str("progressed"),
            Waits::Idle { until: NONE } => f.write_str("idle until a signal"),
            Waits::Idle { until } => write!(f, "idle until running time {}", Ts(*until)),
            Waits::BlockedRing { pad } => write!(f, "blocked on the ring of src pad {pad}"),
            Waits::BlockedPool { element } => write!(f, "blocked on the pool of element {element}"),
            Waits::BlockedIo => f.write_str("blocked on io"),
            Waits::Exited => f.write_str("exited"),
        }
    }
}

impl fmt::Display for Detail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Detail::None => Ok(()),
            Detail::Violation(found) => write!(f, ", {found}"),
            Detail::Row { row, want, got } => write!(
                f,
                ", {} stream {} seq {} pts {} dts {} gen {} format {} flags {:#x} aux {} at {}, \
                 wanted {want} and got {got}",
                row.kind.name(),
                row.stream,
                row.seq,
                Ts(row.pts),
                Ts(row.dts),
                row.seek_gen,
                row.format,
                row.flags,
                row.aux,
                Ts(row.at),
            ),
            Detail::Numbers { want, got } => write!(f, ", wanted {want} and got {got}"),
            Detail::Group { group, waits, held } => {
                write!(f, ", group {group} {waits} and holds {held}")
            }
            Detail::Action { index, answer } => write!(f, ", action {index} was {}", answer.name()),
            Detail::Element { element, tap_full: false } => write!(f, ", element {element} failed"),
            Detail::Element { element, tap_full: true } => {
                write!(f, ", the tap of element {element} is full")
            }
            Detail::Link { link, want, got } => {
                write!(f, ", link {link}, the offers give {want} and core has {got}")
            }
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "seed {} step {} {:?}", self.seed, self.step, self.kind)?;
        if self.sink != NO_SINK {
            write!(f, " sink {}", self.sink)?;
        }
        if self.row != NO_ROW {
            write!(f, " row {}", self.row)?;
        }
        self.detail.fmt(f)
    }
}

impl std::error::Error for Verdict {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RowKind;

    #[test]
    fn a_verdict_names_seed_step_sink_and_row() {
        let row = Row { seq: 4, ..Row::new(RowKind::Buffer, 1, 0) };
        let found = Finding::at_row(Kind::Missing, 17, &row, 3, 4);
        let verdict = Verdict::new(99, 250, found);
        assert_eq!((verdict.seed, verdict.step, verdict.sink, verdict.row), (99, 250, 1, 17));
        let text = format!("{verdict}");
        for part in ["seed 99", "step 250", "Missing", "sink 1", "row 17", "wanted 3 and got 4"] {
            assert!(text.contains(part), "{text}");
        }
    }

    #[test]
    fn a_link_verdict_names_the_link_and_both_formats() {
        let (want, got) = (Fixed { family: 0, id: 21 }, Fixed { family: 1, id: 0 });
        let detail = Detail::Link { link: 2, want, got };
        let text = format!("{}", Verdict::new(8, 0, Finding::run(Kind::LinkFormat, detail)));
        assert_eq!(
            text,
            "seed 8 step 0 LinkFormat, link 2, the offers give sim/media id 21 and core has \
             sim/other with the id open"
        );
    }

    #[test]
    fn a_run_level_verdict_names_no_sink_and_no_row() {
        let detail = Detail::Group { group: 2, waits: Waits::BlockedRing { pad: 1 }, held: 3 };
        let text = format!("{}", Verdict::new(5, 9, Finding::run(Kind::Stall, detail)));
        assert_eq!(
            text,
            "seed 5 step 9 Stall, group 2 blocked on the ring of src pad 1 and holds 3"
        );
    }
}
