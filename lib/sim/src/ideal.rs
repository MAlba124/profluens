//! The ideal pipeline, a reference with nothing between the edges. Stream `n` feeds sink `n`
//! directly in discrete event time, so its recordings are correct by construction.

use crate::action::{Action, ActionKind, Anchor, anchor_met};
use crate::media::{self, Cursor, EndKind, MediaSpec, ScheduleError, Step};
use crate::record::{MAX_SINKS, NO_STREAM, Recording, Row, RowKind};
use crate::sweep::SinkExpect;
use crate::time::{NONE, Ns};

#[derive(Clone, Copy)]
struct Lane {
    cursor: Cursor,
    next: Step,
    done: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    Playing,
    Paused { since: Ns },
    Stopped,
}

struct Run<'a> {
    spec: &'a MediaSpec,
    lanes: [Option<Lane>; MAX_SINKS],
    transport: Transport,
    now: Ns,
    seek_gen: u32,
    fired: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Played {
    pub now: Ns,
    pub fired: u64,
    pub stopped: bool,
    pub truncated: bool,
}

pub fn expect() -> [SinkExpect; MAX_SINKS] {
    core::array::from_fn(|sink| SinkExpect { streams: 1 << sink })
}

fn push(rec: &mut Recording, run: &Run, kind: RowKind, sink: usize, aux: u64) {
    let stream = if kind.is(crate::record::class::OUT_OF_BAND) { NO_STREAM } else { sink as u16 };
    rec.push(Row {
        at: run.now,
        seek_gen: run.seek_gen,
        aux,
        ..Row::new(kind, sink as u16, stream)
    });
}

fn push_all(rec: &mut Recording, run: &Run, kind: RowKind, aux: u64) {
    for sink in 0..MAX_SINKS {
        if run.lanes[sink].is_some() {
            push(rec, run, kind, sink, aux);
        }
    }
}

fn seek(rec: &mut Recording, run: &mut Run, to: Ns) {
    run.seek_gen += 1;
    for sink in 0..MAX_SINKS {
        let Some(mut lane) = run.lanes[sink] else {
            continue;
        };
        push(rec, run, RowKind::FlushStart, sink, 0);
        push(rec, run, RowKind::FlushStop, sink, 0);
        // The format is link state and survives the flush. The segment does not.
        push(rec, run, RowKind::Segment, sink, to);
        media::seek(run.spec, &mut lane.cursor, to, run.now);
        lane.next = media::step(run.spec, &mut lane.cursor);
        lane.done = false;
        run.lanes[sink] = Some(lane);
    }
    if let Transport::Paused { .. } = run.transport {
        run.transport = Transport::Paused { since: run.now };
    }
}

fn resume(rec: &mut Recording, run: &mut Run, since: Ns) {
    let by = run.now - since;
    for lane in run.lanes.iter_mut().flatten() {
        media::shift(&mut lane.cursor, by);
        lane.next = match lane.next {
            Step::Slot(slot) => {
                Step::Slot(media::Slot { release: slot.release.saturating_add(by), ..slot })
            }
            Step::End { kind, at } => Step::End { kind, at: at.saturating_add(by) },
        };
    }
    run.transport = Transport::Playing;
    push_all(rec, run, RowKind::Resumed, 0);
}

fn fire(rec: &mut Recording, run: &mut Run, action: &Action) {
    match (action.kind, run.transport) {
        (ActionKind::Seek { to }, _) => seek(rec, run, to),
        (ActionKind::Pause, Transport::Playing) => {
            run.transport = Transport::Paused { since: run.now };
            push_all(rec, run, RowKind::Paused, 0);
        }
        (ActionKind::Resume, Transport::Paused { since }) => resume(rec, run, since),
        (ActionKind::Stop, _) => {
            run.transport = Transport::Stopped;
            push_all(rec, run, RowKind::Stop, 0);
        }
        _ => {}
    }
}

fn due(lane: &Lane) -> Ns {
    match lane.next {
        _ if lane.done => NONE,
        Step::Slot(slot) => slot.release,
        Step::End { at, .. } => at,
    }
}

fn deliver(rec: &mut Recording, run: &mut Run, sink: usize) {
    let Some(mut lane) = run.lanes[sink] else {
        return;
    };
    match lane.next {
        Step::Slot(slot) => {
            rec.push(Row {
                pts: slot.pts,
                dts: slot.dts,
                duration: slot.duration,
                at: run.now,
                seq: slot.seq,
                seek_gen: run.seek_gen,
                size: slot.size,
                flags: slot.flags,
                ..Row::new(RowKind::Buffer, sink as u16, sink as u16)
            });
            lane.next = media::step(run.spec, &mut lane.cursor);
        }
        Step::End { kind, .. } => {
            if kind != EndKind::Error {
                push(rec, run, RowKind::Eos, sink, 0);
            }
            lane.done = true;
        }
    }
    run.lanes[sink] = Some(lane);
}

fn ready_action(run: &Run, actions: &[Action], rec: &Recording) -> Option<usize> {
    (0..actions.len().min(64))
        .find(|i| run.fired & (1 << i) == 0 && anchor_met(actions[*i].anchor, run.now, rec))
}

fn next_anchor(run: &Run, actions: &[Action]) -> Ns {
    let mut earliest = NONE;
    for (i, action) in actions.iter().enumerate().take(64) {
        if let (Anchor::At(t), true) = (action.anchor, run.fired & (1 << i) == 0) {
            earliest = earliest.min(t);
        }
    }
    earliest
}

fn next_delivery(run: &Run) -> (usize, Ns) {
    let mut best = (0, NONE);
    if run.transport != Transport::Playing {
        return best;
    }
    for (sink, lane) in run.lanes.iter().enumerate() {
        let at = lane.as_ref().map_or(NONE, due);
        if at < best.1 {
            best = (sink, at);
        }
    }
    best
}

pub fn play(
    spec: &MediaSpec,
    seed: u64,
    actions: &[Action],
    rec: &mut Recording,
) -> Result<Played, ScheduleError> {
    rec.reset();
    let mut run = Run {
        spec,
        lanes: [None; MAX_SINKS],
        transport: Transport::Playing,
        now: 0,
        seek_gen: 0,
        fired: 0,
    };
    for sink in 0..spec.streams.len().min(MAX_SINKS) {
        let mut cursor = media::cursor(spec, sink as u16, seed, 0)?;
        let next = media::step(spec, &mut cursor);
        run.lanes[sink] = Some(Lane { cursor, next, done: false });
    }
    push_all(rec, &run, RowKind::Start, 0);
    push_all(rec, &run, RowKind::Format, 0);
    push_all(rec, &run, RowKind::Segment, 0);

    // Every pass fires, delivers or ends, and a full recording ends it, so capacity bounds the
    // loop.
    while run.transport != Transport::Stopped && rec.overflow() == 0 {
        if let Some(i) = ready_action(&run, actions, rec) {
            run.fired |= 1 << i;
            fire(rec, &mut run, &actions[i]);
            continue;
        }
        let (sink, release) = next_delivery(&run);
        let anchor = next_anchor(&run, actions);
        if anchor == NONE && release == NONE {
            break;
        }
        // On a tie the action goes first, as a command queue is read at the top of a pass.
        if anchor <= release {
            run.now = run.now.max(anchor);
            continue;
        }
        run.now = run.now.max(release);
        deliver(rec, &mut run, sink);
    }
    let stopped = run.transport == Transport::Stopped;
    if !stopped {
        push_all(rec, &run, RowKind::Stop, 0);
    }
    Ok(Played { now: run.now, fired: run.fired, stopped, truncated: rec.overflow() > 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Fault, Pacing, StreamKind, StreamSpec};
    use crate::record::{checksum, flag};
    use crate::sweep::{self, Expect};
    use crate::time::{MS, Rational, SEC};

    fn reference(pacing: Pacing) -> MediaSpec {
        let mut spec = MediaSpec::with_capacity(0, 4);
        let video = StreamKind::Video { fps: Rational::new(25, 1), keyframe_interval: 25 };
        spec.add_stream(StreamSpec { kind: video, duration: 10 * SEC, size: 4096, pacing })
            .unwrap();
        let audio = StreamKind::Audio { packet: 20 * MS };
        spec.add_stream(StreamSpec { kind: audio, duration: 10 * SEC, size: 256, pacing }).unwrap();
        spec
    }

    fn at(t: Ns, kind: ActionKind) -> Action {
        Action { anchor: Anchor::At(t), kind }
    }

    fn played(spec: &MediaSpec, seed: u64, actions: &[Action]) -> (Recording, Played) {
        let mut rec = Recording::with_capacity(8_192);
        let done = play(spec, seed, actions, &mut rec).unwrap();
        let sinks = expect();
        let expect = Expect { sinks: &sinks[..spec.streams.len()], gaps: &[] };
        if let Err(error) = sweep::check(&rec, &expect) {
            panic!("{error}");
        }
        (rec, done)
    }

    fn count(rec: &Recording, kind: RowKind, sink: u16) -> usize {
        rec.rows().iter().filter(|row| row.kind == kind && row.sink == sink).count()
    }

    #[test]
    fn a_plain_run_delivers_everything_once_in_time_order() {
        let (rec, done) = played(&reference(Pacing::RealTime), 1, &[]);
        assert_eq!(count(&rec, RowKind::Buffer, 0), 250);
        assert_eq!(count(&rec, RowKind::Buffer, 1), 500);
        for sink in 0..2 {
            for kind in
                [RowKind::Start, RowKind::Format, RowKind::Segment, RowKind::Eos, RowKind::Stop]
            {
                assert_eq!(count(&rec, kind, sink), 1, "{kind:?} on sink {sink}");
            }
        }
        assert!(rec.rows().windows(2).all(|pair| pair[0].at <= pair[1].at));
        assert_eq!(done, Played { now: 10 * SEC, fired: 0, stopped: false, truncated: false });
    }

    #[test]
    fn one_seed_gives_one_run_and_jitter_makes_seeds_differ() {
        let spec = reference(Pacing::Jitter { base: 10 * MS, bound: 25 * MS });
        let actions = [at(2 * SEC, ActionKind::Seek { to: 7 * SEC })];
        let (a, _) = played(&spec, 11, &actions);
        let (b, _) = played(&spec, 11, &actions);
        let (c, _) = played(&spec, 12, &actions);
        assert_eq!(checksum(a.rows()), checksum(b.rows()));
        assert_ne!(checksum(a.rows()), checksum(c.rows()));
    }

    #[test]
    fn a_seek_flushes_bumps_the_generation_and_resumes_at_a_keyframe() {
        let actions = [at(2 * SEC, ActionKind::Seek { to: 4_500 * MS })];
        let (rec, done) = played(&reference(Pacing::RealTime), 1, &actions);
        assert_eq!(done.fired, 1);
        let rows = rec.rows();
        let flush =
            rows.iter().position(|row| row.kind == RowKind::FlushStart && row.sink == 0).unwrap();
        assert_eq!((rows[flush].at, rows[flush].seek_gen), (2 * SEC, 1));
        assert!(rows[..flush].iter().all(|row| row.seek_gen == 0));
        assert!(rows[flush..].iter().all(|row| row.seek_gen == 1));
        let first =
            rows[flush..].iter().find(|row| row.kind == RowKind::Buffer && row.sink == 0).unwrap();
        assert_eq!((first.seq, first.pts), (100, 4 * SEC));
        assert_eq!(first.flags, flag::KEYFRAME | flag::DISCONT);
        assert_eq!(count(&rec, RowKind::Buffer, 0), 50 + 150);
        assert_eq!(count(&rec, RowKind::Buffer, 1), 100 + 275);
        assert_eq!(count(&rec, RowKind::Segment, 0), 2);
        assert_eq!(count(&rec, RowKind::Format, 0), 1);
    }

    #[test]
    fn a_seek_after_the_end_replays_and_ends_again() {
        let actions = [at(20 * SEC, ActionKind::Seek { to: 9 * SEC })];
        let (rec, done) = played(&reference(Pacing::RealTime), 1, &actions);
        assert_eq!(count(&rec, RowKind::Eos, 0), 2);
        assert_eq!(count(&rec, RowKind::Buffer, 0), 250 + 25);
        assert_eq!(done.now, 21 * SEC);
    }

    #[test]
    fn a_pause_holds_delivery_and_resume_shifts_it() {
        let actions = [at(SEC, ActionKind::Pause), at(4 * SEC, ActionKind::Resume)];
        let (rec, done) = played(&reference(Pacing::RealTime), 1, &actions);
        let rows = rec.rows();
        assert!(
            rows.iter()
                .all(|row| row.kind != RowKind::Buffer || row.at <= SEC || row.at >= 4 * SEC)
        );
        assert_eq!(count(&rec, RowKind::Paused, 1), 1);
        assert_eq!(count(&rec, RowKind::Resumed, 1), 1);
        assert_eq!(count(&rec, RowKind::Buffer, 0), 250);
        assert_eq!(done.now, 13 * SEC);
        let late =
            rows.iter().find(|row| row.kind == RowKind::Buffer && row.at >= 4 * SEC).unwrap();
        assert_eq!(late.at - late.pts, 3 * SEC);
    }

    #[test]
    fn a_pause_with_nothing_left_to_wake_it_ends_the_run() {
        let unreachable = Action {
            anchor: Anchor::AfterBuffers { sink: 0, count: 9_999 },
            kind: ActionKind::Resume,
        };
        let (rec, done) =
            played(&reference(Pacing::RealTime), 1, &[at(SEC, ActionKind::Pause), unreachable]);
        assert_eq!(done, Played { now: SEC, fired: 1, stopped: false, truncated: false });
        assert_eq!(count(&rec, RowKind::Eos, 0), 0);
        assert_eq!(count(&rec, RowKind::Stop, 0), 1);
    }

    #[test]
    fn a_stop_ends_the_run_and_later_actions_never_fire() {
        let actions = [at(3 * SEC, ActionKind::Stop), at(5 * SEC, ActionKind::Seek { to: 0 })];
        let (rec, done) = played(&reference(Pacing::RealTime), 1, &actions);
        assert_eq!(done, Played { now: 3 * SEC, fired: 1, stopped: true, truncated: false });
        assert_eq!(count(&rec, RowKind::Stop, 0), 1);
        assert_eq!(count(&rec, RowKind::FlushStart, 0), 0);
    }

    #[test]
    fn a_buffer_anchor_fires_between_deliveries() {
        let after = Action {
            anchor: Anchor::AfterBuffers { sink: 1, count: 10 },
            kind: ActionKind::Seek { to: 0 },
        };
        let (rec, _) = played(&reference(Pacing::AsFastAsPossible), 1, &[after]);
        let rows = rec.rows();
        let flush =
            rows.iter().position(|row| row.kind == RowKind::FlushStart && row.sink == 1).unwrap();
        let before =
            rows[..flush].iter().filter(|row| row.kind == RowKind::Buffer && row.sink == 1).count();
        assert_eq!(before, 10);
    }

    #[test]
    fn declared_faults_shape_the_recording() {
        let mut spec = reference(Pacing::RealTime);
        spec.add_fault(0, Fault::ErrorAt { buffer: 30 }).unwrap();
        spec.add_fault(1, Fault::EndEarlyAt { buffer: 40 }).unwrap();
        let (rec, _) = played(&spec, 1, &[]);
        assert_eq!((count(&rec, RowKind::Buffer, 0), count(&rec, RowKind::Eos, 0)), (30, 0));
        assert_eq!((count(&rec, RowKind::Buffer, 1), count(&rec, RowKind::Eos, 1)), (40, 1));
    }

    #[test]
    fn a_full_recording_cuts_the_run_short() {
        let mut rec = Recording::with_capacity(100);
        let done = play(&reference(Pacing::RealTime), 1, &[], &mut rec).unwrap();
        assert!(done.truncated);
        assert_eq!(rec.len(), 100);
        let sinks = expect();
        let verdict = sweep::check(&rec, &Expect { sinks: &sinks[..2], gaps: &[] });
        assert!(matches!(verdict, Err(sweep::SweepError::Overflow { .. })));
    }

    #[test]
    fn a_bad_spec_is_an_error_not_a_run() {
        let mut spec = reference(Pacing::RealTime);
        spec.streams[1].kind = StreamKind::Audio { packet: 0 };
        let mut rec = Recording::with_capacity(16);
        assert_eq!(play(&spec, 1, &[], &mut rec), Err(ScheduleError::ZeroPacket { stream: 1 }));
    }
}
