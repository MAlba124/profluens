//! Every illegal log must be rejected by exactly the rule it names, every legal one by none.

use pf_sim::record::{NO_STREAM, Row, RowKind, flag};
use pf_sim::sweep::{self, Cause, Expect, GapWindow, NO_ROW, Rule, SinkExpect, Violation};
use pf_sim::time::{MS, NONE};

const SINKS: [SinkExpect; 2] = [SinkExpect { streams: 0b001 }, SinkExpect { streams: 0b110 }];
const EXPECT: Expect = Expect { sinks: &SINKS, gaps: &[] };

fn ev(kind: RowKind) -> Row {
    let stream = if kind.is(pf_sim::record::class::OUT_OF_BAND) { NO_STREAM } else { 0 };
    Row::new(kind, 0, stream)
}

fn buf(seq: u64, pts_ms: u64) -> Row {
    Row { seq, pts: pts_ms * MS, duration: 40 * MS, size: 100, ..Row::new(RowKind::Buffer, 0, 0) }
}

fn discont(row: Row) -> Row {
    Row { flags: row.flags | flag::DISCONT, ..row }
}

fn generation(seek_gen: u32, row: Row) -> Row {
    Row { seek_gen, ..row }
}

fn on_sink_1(stream: u16, row: Row) -> Row {
    let stream = if row.stream == NO_STREAM { NO_STREAM } else { stream };
    Row { sink: 1, stream, ..row }
}

fn start() -> Row {
    ev(RowKind::Start)
}
fn format() -> Row {
    ev(RowKind::Format)
}
fn segment() -> Row {
    ev(RowKind::Segment)
}
fn eos() -> Row {
    ev(RowKind::Eos)
}
fn flush() -> [Row; 2] {
    [ev(RowKind::FlushStart), ev(RowKind::FlushStop)]
}

fn opened() -> Vec<Row> {
    vec![start(), format(), segment(), discont(buf(0, 0))]
}

fn with(mut base: Vec<Row>, more: &[Row]) -> Vec<Row> {
    base.extend_from_slice(more);
    base
}

fn fired(rows: &[Row], expect: &Expect) -> Vec<Rule> {
    sweep::each(rows, expect).iter().flatten().map(|violation| violation.rule).collect()
}

fn reject(name: &str, rule: Rule, rows: &[Row], expect: &Expect) -> Violation {
    assert_eq!(fired(rows, expect), [rule], "{name}");
    let violation = sweep::all(rows, expect).expect_err(name);
    assert_eq!(violation.rule, rule, "{name}");
    assert_eq!(violation.row, rows[violation.index as usize], "{name}");
    assert_eq!(sweep::run(rule, rows, expect), Err(violation), "{name}");
    violation
}

fn accept(name: &str, rows: &[Row], expect: &Expect) {
    if let Err(violation) = sweep::all(rows, expect) {
        panic!("{name} is legal but was rejected, {violation}");
    }
    assert!(fired(rows, expect).is_empty(), "{name}");
}

#[test]
fn format_before_buffer() {
    let rule = Rule::FormatBeforeBuffer;
    let v = reject(
        "buffer before any format",
        rule,
        &[start(), segment(), discont(buf(0, 0))],
        &EXPECT,
    );
    assert_eq!((v.index, v.cause), (2, Cause::NoFormat));
    reject(
        "gap before any format",
        rule,
        &[start(), segment(), ev(RowKind::Gap), format()],
        &EXPECT,
    );
    reject(
        "format after the first buffer",
        rule,
        &[start(), segment(), buf(0, 0), format()],
        &EXPECT,
    );
    reject("a drop counts as an arrival", rule, &[start(), segment(), ev(RowKind::Drop)], &EXPECT);
    let other_sink = with(opened(), &[on_sink_1(1, segment()), on_sink_1(1, buf(0, 0))]);
    let v = reject("the format of sink 0 does not cover sink 1", rule, &other_sink, &EXPECT);
    assert_eq!(v.index, 5);

    accept("opening", &opened(), &EXPECT);
    accept("events only", &[start(), segment(), eos()], &EXPECT);
    let [fs, fe] = flush();
    accept(
        "format survives a flush",
        &with(opened(), &[fs, fe, segment(), discont(buf(9, 360))]),
        &EXPECT,
    );
    let restarted = with(opened(), &[ev(RowKind::Stop), start(), segment(), buf(0, 0)]);
    accept("format survives a restart", &restarted, &EXPECT);
}

#[test]
fn segment_before_buffer() {
    let rule = Rule::SegmentBeforeBuffer;
    let [fs, fe] = flush();
    let v = reject("buffer with no segment", rule, &[start(), format(), buf(0, 0)], &EXPECT);
    assert_eq!((v.index, v.cause), (2, Cause::NoSegment { since: 0 }));
    let v = reject("no lifecycle rows and no segment", rule, &[format(), buf(0, 0)], &EXPECT);
    assert_eq!(v.cause, Cause::NoSegment { since: NO_ROW });
    reject("gap with no segment", rule, &[start(), format(), ev(RowKind::Gap)], &EXPECT);
    let stale = with(opened(), &[fs, fe, discont(buf(5, 200))]);
    let v = reject("buffer after a flush with no fresh segment", rule, &stale, &EXPECT);
    assert_eq!((v.index, v.cause), (6, Cause::NoSegment { since: 5 }));
    reject(
        "flush start alone drops the segment",
        rule,
        &with(opened(), &[fs, discont(buf(5, 200))]),
        &EXPECT,
    );
    let restarted = with(opened(), &[ev(RowKind::Stop), start(), buf(0, 0)]);
    reject("buffer after a restart with no segment", rule, &restarted, &EXPECT);
    let late = with(opened(), &[fs, fe, discont(buf(5, 200)), segment()]);
    reject("segment after the buffer it should precede", rule, &late, &EXPECT);

    accept(
        "fresh segment after a flush",
        &with(opened(), &[fs, fe, segment(), discont(buf(5, 200))]),
        &EXPECT,
    );
    accept(
        "two flushes, one segment",
        &with(opened(), &[fs, fe, fs, fe, segment(), discont(buf(5, 200))]),
        &EXPECT,
    );
    accept("flush with nothing after", &with(opened(), &[fs, fe]), &EXPECT);
}

#[test]
fn timestamps_monotonic() {
    let rule = Rule::TimestampsMonotonic;
    let [fs, fe] = flush();
    let back = with(opened(), &[buf(1, 100), buf(2, 40)]);
    let v = reject("pts goes back inside a segment", rule, &back, &EXPECT);
    assert_eq!(
        (v.index, v.cause),
        (5, Cause::Backwards { prev: 4, prev_ts: 100 * MS, ts: 40 * MS })
    );
    let dts_back = [Row { dts: 80 * MS, ..buf(1, 80) }, Row { dts: 79 * MS, ..buf(2, 120) }];
    reject("dts goes back while pts goes forward", rule, &with(opened(), &dts_back), &EXPECT);
    let dropped = Row { kind: RowKind::Drop, ..buf(2, 40) };
    reject(
        "a dropped buffer is still ordered",
        rule,
        &with(opened(), &[buf(1, 100), dropped]),
        &EXPECT,
    );
    let untimed = |seq| Row { pts: NONE, ..buf(seq, 0) };
    let around = with(opened(), &[buf(1, 100), untimed(2), buf(3, 99)]);
    reject("an untimed buffer does not reset the order", rule, &around, &EXPECT);

    accept("equal timestamps", &with(opened(), &[buf(1, 40), buf(2, 40)]), &EXPECT);
    accept(
        "a new segment restarts the timeline",
        &with(opened(), &[buf(1, 100), segment(), buf(2, 0)]),
        &EXPECT,
    );
    let seek_back = with(opened(), &[buf(1, 100), fs, fe, segment(), discont(buf(0, 0))]);
    accept("a seek goes back", &seek_back, &EXPECT);
    // Decode order. Dts rises while pts reorders, as with B-frames.
    let reordered = [
        Row { dts: 40 * MS, ..buf(1, 160) },
        Row { dts: 80 * MS, ..buf(2, 80) },
        Row { dts: 120 * MS, ..buf(3, 120) },
    ];
    accept("pts reorders under a rising dts", &with(opened(), &reordered), &EXPECT);
    accept("untimed buffers", &with(opened(), &[untimed(1), untimed(2)]), &EXPECT);
    let two_sinks = with(
        opened(),
        &[
            on_sink_1(1, format()),
            on_sink_1(1, segment()),
            on_sink_1(1, buf(0, 500)),
            buf(1, 40),
            on_sink_1(1, buf(1, 540)),
        ],
    );
    accept("each sink has its own timeline", &two_sinks, &EXPECT);
}

#[test]
fn nothing_after_eos() {
    let rule = Rule::NothingAfterEos;
    let [fs, fe] = flush();
    let ended = with(opened(), &[eos()]);
    let v = reject("buffer after eos", rule, &with(ended.clone(), &[buf(1, 40)]), &EXPECT);
    assert_eq!((v.index, v.cause), (5, Cause::AfterEos { eos: 4 }));
    for kind in [RowKind::Gap, RowKind::Segment, RowKind::Format, RowKind::Tags, RowKind::Patch] {
        reject(kind.name(), rule, &with(ended.clone(), &[ev(kind)]), &EXPECT);
    }
    let dropped = Row { kind: RowKind::Drop, ..buf(1, 40) };
    reject("dropped buffer after eos", rule, &with(ended.clone(), &[dropped]), &EXPECT);
    let newer = generation(1, buf(1, 40));
    reject("newer generation but no flush", rule, &with(ended.clone(), &[newer]), &EXPECT);

    accept("played to the end", &ended, &EXPECT);
    accept(
        "a flush reopens the sink",
        &with(ended.clone(), &[fs, fe, segment(), discont(buf(3, 120)), eos()]),
        &EXPECT,
    );
    let restart = with(ended.clone(), &[ev(RowKind::Stop), start(), segment(), buf(0, 0), eos()]);
    accept("a restart reopens the sink", &restart, &EXPECT);
    let calls = [RowKind::Paused, RowKind::Resumed, RowKind::PropChanged, RowKind::Stop].map(ev);
    accept("calls and transport changes after eos", &with(ended.clone(), &calls), &EXPECT);
    let other = [on_sink_1(1, format()), on_sink_1(1, segment()), on_sink_1(1, buf(0, 0))];
    accept("another sink keeps playing", &with(ended, &other), &EXPECT);
}

#[test]
fn eos_once() {
    let rule = Rule::EosOnce;
    let [fs, fe] = flush();
    let v = reject("eos twice", rule, &with(opened(), &[eos(), eos()]), &EXPECT);
    assert_eq!((v.index, v.cause), (5, Cause::AfterEos { eos: 4 }));
    reject(
        "eos twice around a pause",
        rule,
        &with(opened(), &[eos(), ev(RowKind::Paused), eos()]),
        &EXPECT,
    );
    reject(
        "eos twice with a newer generation",
        rule,
        &with(opened(), &[eos(), generation(1, eos())]),
        &EXPECT,
    );

    accept("one eos per generation", &with(opened(), &[eos(), fs, fe, segment(), eos()]), &EXPECT);
    accept("one eos per sink", &with(opened(), &[eos(), on_sink_1(1, eos())]), &EXPECT);
    accept(
        "one eos per run",
        &with(opened(), &[eos(), ev(RowKind::Stop), start(), eos()]),
        &EXPECT,
    );
}

#[test]
fn no_stale_generation() {
    let rule = Rule::NoStaleGeneration;
    let [fs, fe] = flush().map(|row| generation(1, row));
    let seeked = with(opened(), &[fs, fe]);
    let old_buffer = [generation(1, segment()), discont(buf(1, 40))];
    let v = reject(
        "buffer of the old generation after a flush",
        rule,
        &with(seeked.clone(), &old_buffer),
        &EXPECT,
    );
    assert_eq!((v.index, v.cause), (7, Cause::StaleGeneration { flush: 5, flush_gen: 1 }));
    let old_segment = [segment(), generation(1, discont(buf(1, 40)))];
    reject(
        "segment of the old generation after a flush",
        rule,
        &with(seeked.clone(), &old_segment),
        &EXPECT,
    );
    reject(
        "eos of the old generation after a flush",
        rule,
        &with(seeked.clone(), &[eos()]),
        &EXPECT,
    );
    reject(
        "flush stop older than its flush start",
        rule,
        &with(opened(), &[fs, ev(RowKind::FlushStop)]),
        &EXPECT,
    );
    reject("a flush older than the one before", rule, &with(seeked.clone(), &flush()), &EXPECT);

    let fresh =
        [generation(1, segment()), generation(1, discont(buf(7, 280))), generation(1, eos())];
    accept(
        "everything after the flush is of its generation",
        &with(seeked.clone(), &fresh),
        &EXPECT,
    );
    let ahead = [generation(2, segment()), generation(2, discont(buf(7, 280)))];
    accept("a generation ahead of the flush", &with(seeked.clone(), &ahead), &EXPECT);
    let calls = [RowKind::Paused, RowKind::Resumed, RowKind::PropChanged, RowKind::Stop].map(ev);
    accept("out of band rows keep whatever generation", &with(seeked.clone(), &calls), &EXPECT);
    let other = [on_sink_1(1, format()), on_sink_1(1, segment()), on_sink_1(1, buf(0, 0))];
    accept("a sink that has not seen the flush yet", &with(seeked.clone(), &other), &EXPECT);
    let restart = [ev(RowKind::Stop), start(), segment(), buf(0, 0)];
    accept("a restart forgets the flush", &with(seeked, &restart), &EXPECT);
}

#[test]
fn discont_after_flush() {
    let rule = Rule::DiscontAfterFlush;
    let [fs, fe] = flush();
    let seeked = with(opened(), &[fs, fe, segment()]);
    let v = reject(
        "first buffer after a flush is continuous",
        rule,
        &with(seeked.clone(), &[buf(5, 200)]),
        &EXPECT,
    );
    assert_eq!((v.index, v.cause), (7, Cause::NotDiscont { flush: 5 }));
    let dropped = Row { kind: RowKind::Drop, ..buf(5, 200) };
    reject(
        "first arrival after a flush is a continuous drop",
        rule,
        &with(seeked.clone(), &[dropped]),
        &EXPECT,
    );
    let twice = [discont(buf(5, 200)), fs, fe, segment(), buf(9, 360)];
    reject("the second of two flushes", rule, &with(seeked.clone(), &twice), &EXPECT);
    // Sink 1 carries streams 1 and 2, each one's first buffer after the flush is a discontinuity.
    let two_streams = |second: Row| {
        let s1 = |row| on_sink_1(1, row);
        vec![
            s1(start()),
            s1(format()),
            s1(segment()),
            s1(buf(0, 0)),
            on_sink_1(2, buf(0, 0)),
            s1(fs),
            s1(fe),
            s1(segment()),
            s1(discont(buf(5, 200))),
            on_sink_1(2, second),
        ]
    };
    let v = reject(
        "the second stream of a sink is continuous after the flush",
        rule,
        &two_streams(buf(5, 200)),
        &EXPECT,
    );
    assert_eq!((v.index, v.cause), (9, Cause::NotDiscont { flush: 6 }));
    accept(
        "both streams discontinuous after the flush",
        &two_streams(discont(buf(5, 200))),
        &EXPECT,
    );

    accept(
        "discontinuous after a flush",
        &with(seeked.clone(), &[discont(buf(5, 200)), buf(6, 240)]),
        &EXPECT,
    );
    accept(
        "a discontinuous drop takes the flag",
        &with(seeked.clone(), &[discont(dropped), buf(6, 240)]),
        &EXPECT,
    );
    let keyframe = Row { flags: flag::KEYFRAME | flag::DISCONT, ..buf(5, 200) };
    accept("other flags beside it", &with(seeked.clone(), &[keyframe]), &EXPECT);
    let other = [on_sink_1(1, format()), on_sink_1(1, segment()), on_sink_1(1, buf(0, 0))];
    accept("a flush on one sink does not bind another", &with(seeked, &other), &EXPECT);
}

#[test]
fn no_duplicate_seq() {
    let rule = Rule::NoDuplicateSeq;
    let [fs, fe] = flush();
    let v =
        reject("the same buffer twice", rule, &with(opened(), &[buf(1, 40), buf(1, 40)]), &EXPECT);
    assert_eq!((v.index, v.cause), (5, Cause::Repeated { prev: 4, prev_seq: 1 }));
    reject(
        "a replay without a seek",
        rule,
        &with(opened(), &[buf(1, 40), buf(2, 80), buf(1, 120)]),
        &EXPECT,
    );
    let dropped = Row { kind: RowKind::Drop, ..buf(1, 40) };
    reject(
        "a buffer dropped and then delivered",
        rule,
        &with(opened(), &[dropped, buf(1, 40)]),
        &EXPECT,
    );
    let interleaved = [
        on_sink_1(1, format()),
        on_sink_1(1, segment()),
        on_sink_1(1, buf(0, 0)),
        on_sink_1(2, buf(0, 0)),
        on_sink_1(1, buf(0, 40)),
    ];
    let v = reject("a duplicate behind another stream", rule, &interleaved, &EXPECT);
    assert_eq!(v.cause, Cause::Repeated { prev: 2, prev_seq: 0 });

    accept(
        "a seek back replays",
        &with(opened(), &[buf(1, 40), fs, fe, segment(), discont(buf(0, 0)), buf(1, 40)]),
        &EXPECT,
    );
    accept(
        "a restart replays",
        &with(opened(), &[ev(RowKind::Stop), start(), segment(), buf(0, 0)]),
        &EXPECT,
    );
    let two_streams = [
        on_sink_1(1, format()),
        on_sink_1(1, segment()),
        on_sink_1(1, buf(0, 0)),
        on_sink_1(2, buf(0, 0)),
        on_sink_1(1, buf(1, 40)),
        on_sink_1(2, buf(1, 40)),
    ];
    accept("two streams number independently", &two_streams, &EXPECT);
    accept("two sinks number independently", &with(opened(), &two_streams), &EXPECT);
}

#[test]
fn no_misrouting() {
    let rule = Rule::NoMisrouting;
    let wrong_stream = Row { stream: 1, ..buf(0, 40) };
    let v = reject(
        "a buffer of stream 1 at the sink of stream 0",
        rule,
        &with(opened(), &[wrong_stream]),
        &EXPECT,
    );
    assert_eq!((v.index, v.cause), (4, Cause::UnexpectedStream { expected: 0b001 }));
    let crossed = [on_sink_1(1, format()), on_sink_1(1, segment()), on_sink_1(0, buf(0, 0))];
    reject("a buffer of stream 0 at the sink of streams 1 and 2", rule, &crossed, &EXPECT);
    let far = Row { stream: 40, ..buf(0, 40) };
    reject("a stream id no mask can name", rule, &with(opened(), &[far]), &EXPECT);
    let dropped = Row { kind: RowKind::Drop, stream: 2, ..buf(0, 40) };
    reject("a dropped buffer is still routed", rule, &with(opened(), &[dropped]), &EXPECT);
    let unknown = Row { sink: 2, ..start() };
    let v = reject("a row from an undeclared sink", rule, &with(opened(), &[unknown]), &EXPECT);
    assert_eq!(v.cause, Cause::UnknownSink { sinks: 2 });

    let both = [
        on_sink_1(1, format()),
        on_sink_1(1, segment()),
        on_sink_1(1, buf(0, 0)),
        on_sink_1(2, buf(0, 0)),
    ];
    accept("every stream at its sink", &with(opened(), &both), &EXPECT);
    accept("a stop names no stream", &with(opened(), &[ev(RowKind::Stop)]), &EXPECT);
}

#[test]
fn no_undeclared_gap() {
    let rule = Rule::NoUndeclaredGap;
    let [fs, fe] = flush();
    let holed = with(opened(), &[buf(1, 40), buf(4, 160)]);
    let v = reject("two buffers missing", rule, &holed, &EXPECT);
    assert_eq!((v.index, v.cause), (5, Cause::Gap { prev: 4, prev_seq: 1 }));

    let window = |sink, stream, first, last| [GapWindow { sink, stream, first, last }];
    let partly = window(0, 0, 2, 2);
    reject("a window that covers half the gap", rule, &holed, &Expect { gaps: &partly, ..EXPECT });
    let elsewhere = window(1, 0, 0, u64::MAX);
    reject("a window for another sink", rule, &holed, &Expect { gaps: &elsewhere, ..EXPECT });
    let other_stream = window(0, 1, 0, u64::MAX);
    reject("a window for another stream", rule, &holed, &Expect { gaps: &other_stream, ..EXPECT });
    let after_drop = with(opened(), &[Row { kind: RowKind::Drop, ..buf(1, 40) }, buf(3, 120)]);
    reject("a reported drop covers only itself", rule, &after_drop, &EXPECT);

    let exact = window(0, 0, 2, 3);
    accept("a declared fault", &holed, &Expect { gaps: &exact, ..EXPECT });
    let blanket = window(0, 0, 0, u64::MAX);
    accept("a stream the scenario deselects", &holed, &Expect { gaps: &blanket, ..EXPECT });
    let drops = [
        buf(1, 40),
        Row { kind: RowKind::Drop, ..buf(2, 80) },
        Row { kind: RowKind::Drop, ..buf(3, 120) },
        buf(4, 160),
    ];
    accept("drops the sink reported", &with(opened(), &drops), &EXPECT);
    accept(
        "a seek forward",
        &with(opened(), &[buf(1, 40), fs, fe, segment(), discont(buf(90, 3_600))]),
        &EXPECT,
    );
    accept(
        "a run that starts mid stream",
        &[start(), format(), segment(), discont(buf(50, 2_000)), buf(51, 2_040)],
        &EXPECT,
    );
    accept(
        "a restart",
        &with(opened(), &[ev(RowKind::Stop), start(), segment(), buf(30, 1_200)]),
        &EXPECT,
    );
}

#[test]
fn legal_logs_pass_every_rule() {
    let [fs, fe] = flush().map(|row| generation(1, row));
    let g1 = |row| generation(1, row);
    accept("nothing observed", &[], &EXPECT);
    accept("started and stopped", &[start(), ev(RowKind::Stop)], &EXPECT);
    let played = with(opened(), &[buf(1, 40), buf(2, 80), eos(), ev(RowKind::Stop)]);
    accept("played to the end", &played, &EXPECT);
    let sparse = [
        start(),
        format(),
        ev(RowKind::Tags),
        segment(),
        ev(RowKind::Gap),
        discont(buf(0, 5_000)),
        eos(),
    ];
    accept("a sparse stream that opens with a gap", &sparse, &EXPECT);
    let seek = [
        buf(1, 40),
        ev(RowKind::Paused),
        fs,
        fe,
        g1(segment()),
        ev(RowKind::Resumed),
        g1(discont(buf(100, 4_000))),
        g1(buf(101, 4_040)),
        g1(eos()),
        g1(ev(RowKind::Stop)),
    ];
    accept("pause, seek, resume, end", &with(opened(), &seek), &EXPECT);
    let reformat =
        [buf(1, 40), format(), buf(2, 80), ev(RowKind::PropChanged), ev(RowKind::Patch), eos()];
    accept("a format change mid stream", &with(opened(), &reformat), &EXPECT);
    let replayed = [
        buf(1, 40),
        fs,
        g1(format()),
        g1(ev(RowKind::Tags)),
        fe,
        g1(segment()),
        g1(discont(buf(0, 0))),
    ];
    accept("state events replayed inside a flush", &with(opened(), &replayed), &EXPECT);
}

#[test]
fn all_reports_the_earliest_row_and_display_explains_it() {
    let rows = [start(), ev(RowKind::Tags), buf(0, 0), buf(0, 0)];
    let rules = fired(&rows, &EXPECT);
    assert_eq!(rules, [Rule::FormatBeforeBuffer, Rule::SegmentBeforeBuffer, Rule::NoDuplicateSeq]);
    let first = sweep::all(&rows, &EXPECT).unwrap_err();
    // Two rules meet at row 2. The first in rule order is reported.
    assert_eq!((first.rule, first.index), (Rule::FormatBeforeBuffer, 2));

    let late = with(opened(), &[buf(1, 100), buf(2, 40), eos(), eos()]);
    let first = sweep::all(&late, &EXPECT).unwrap_err();
    assert_eq!((first.rule, first.index), (Rule::TimestampsMonotonic, 5));
    let text = format!("{first}");
    assert!(
        text.starts_with("timestamps_monotonic at row 5 (buffer sink 0 stream 0 seq 2 "),
        "{text}"
    );
    assert!(text.contains("40000000ns is behind 100000000ns at row 4"), "{text}");
    assert!(text.contains("dts none"), "{text}");
}

#[test]
fn rows_from_untracked_sinks_reach_only_the_routing_rule() {
    let wild = Row { sink: 40, ..buf(0, 0) };
    let rows = with(opened(), &[wild, wild, Row { sink: 40, ..eos() }, wild]);
    assert_eq!(fired(&rows, &EXPECT), [Rule::NoMisrouting]);
}
