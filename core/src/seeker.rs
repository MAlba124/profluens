//! Turns a seek in time into what each source is told, planned at append from the published
//! indexes.

use crate::command::{ALL, Target};
use crate::counters::{Row, View};
use crate::id::{ElementId, SrcPad};
use crate::pipeline::{Addressed, Index, Reach, SeekMode, SeekPlan, Seekable, Serves};
use crate::time::Timestamp;

pub(crate) struct Planned {
    pub(crate) plan: SeekPlan,
    pub(crate) targets: Vec<Target>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Route {
    source: ElementId,
    serves: Option<Serves>,
    /// `None` where no element on the way publishes an index or is reading one.
    publisher: Option<ElementId>,
}

fn live(view: &View, el: ElementId) -> Option<&Row> {
    view.row(el)
}

/// How `source` is addressed. Silent is a file once the run is on, unknown while it does IO.
fn serves_of(row: &Row) -> Option<Serves> {
    row.counters.serves().or((!row.desc.io).then_some(Serves::FILE))
}

fn sinks(view: &View) -> impl Iterator<Item = ElementId> + '_ {
    let feeds = |el: ElementId| view.edges.iter().any(|e| e.0 == el);
    let mut fed = view.edges.iter().map(|e| e.2).filter(move |el| !feeds(*el)).collect::<Vec<_>>();
    fed.sort_unstable_by_key(|el| el.0);
    fed.dedup();
    fed.into_iter()
}

fn walk(view: &View, sink: ElementId, chain: &mut Vec<ElementId>, routes: &mut Vec<Route>) {
    chain.push(sink);
    let ins = view.edges.iter().filter(|e| e.2 == sink).map(|e| e.0);
    let mut fed = false;
    for up in ins {
        fed = true;
        // A DAG, the depth is bounded by the elements there are.
        if chain.len() <= view.slots.len() && !chain.contains(&up) {
            walk(view, up, chain, routes);
        }
    }
    if !fed {
        routes.push(route(view, chain));
    }
    chain.pop();
}

fn route(view: &View, chain: &[ElementId]) -> Route {
    let source = chain[chain.len() - 1];
    let serves = live(view, source).and_then(serves_of);
    let indexes = |el: &&ElementId| {
        live(view, **el)
            .is_some_and(|row| row.counters.index.get().is_some() || row.counters.indexing())
    };
    let publisher = chain.iter().rev().find(indexes).copied();
    Route { source, serves, publisher }
}

// Off the media path, on an app thread or once per publication while a seek is held.
#[allow(clippy::disallowed_methods)]
fn routes(view: &View) -> Vec<Route> {
    let (mut chain, mut routes) = (Vec::new(), Vec::new());
    for sink in sinks(view) {
        walk(view, sink, &mut chain, &mut routes);
    }
    let mut unique: Vec<Route> = Vec::with_capacity(routes.len());
    for r in routes {
        if !unique.contains(&r) {
            unique.push(r);
        }
    }
    unique
}

/// Whether a skip leaves every sink's streams together, every element feeding a sink having stamped
/// its output.
pub(crate) fn can_skip(view: &View) -> bool {
    sinks(view).all(|sink| {
        let mut feeders = view.edges.iter().filter(|e| e.2 == sink).map(|e| e.0);
        feeders
            .all(|el| live(view, el).is_some_and(|row| row.counters.snapshot().pts_out.is_some()))
    })
}

fn reader_pad(view: &View, publisher: ElementId, index: &Index) -> Option<SrcPad> {
    match index.stream {
        None => Some(SrcPad(0)),
        Some(stream) => {
            let pads = view.stream_pads.get(view.slot(publisher)?)?;
            pads.iter().find(|(s, _)| *s == stream).map(|(_, pad)| *pad)
        }
    }
}

pub(crate) fn seekable(view: &View) -> Seekable {
    let routes = routes(view);
    if routes.is_empty() {
        return Seekable::No;
    }
    let mut best = Seekable::No;
    for r in &routes {
        // A path nothing indexes is unseekable however its source answers, as plan has it.
        let Some(serves) = r.serves else {
            match r.publisher {
                Some(_) => return Seekable::Unknown,
                None => continue,
            }
        };
        let found = match (serves.by, r.publisher) {
            (Addressed::Time, _) => Seekable::Time,
            (_, None) => Seekable::No,
            (Addressed::Byte, Some(_)) => Seekable::Byte,
            (Addressed::No, Some(_)) => Seekable::Forward,
        };
        best = match (best, found) {
            (Seekable::Time, _) | (_, Seekable::Time) => Seekable::Time,
            (Seekable::Byte, _) | (_, Seekable::Byte) => Seekable::Byte,
            (Seekable::Forward, _) | (_, Seekable::Forward) => Seekable::Forward,
            _ => Seekable::No,
        };
    }
    best
}

fn aim(targets: &mut Vec<Target>, (el, byte, at): Target) {
    match targets.iter_mut().find(|t| t.0 == el) {
        Some(t) if byte < t.1 => (t.1, t.2) = (byte, at),
        Some(_) => {}
        None => targets.push((el, byte, at)),
    }
}

/// Plan a seek to `to` from `position`. `settled` is read by the caller before any counter.
// Off the media path, a few targets a seek.
#[allow(clippy::disallowed_methods)]
pub(crate) fn plan(
    view: &View,
    settled: bool,
    position: Timestamp,
    to: Timestamp,
    mode: SeekMode,
) -> Planned {
    let unseekable = Planned { plan: SeekPlan::Unseekable, targets: Vec::new() };
    let deferred = || Planned { plan: SeekPlan::Deferred, targets: Vec::new() };
    let routes = routes(view);
    let skips = settled && can_skip(view);
    let (mut timed, mut seeks, mut all_skip, mut any_byte) = (Vec::new(), Vec::new(), true, false);
    let mut stuck = false;
    for r in &routes {
        // A path nothing indexes is unseekable however its source is addressed.
        if r.publisher.is_none() && r.serves.is_some_and(|s| s.by != Addressed::Time) {
            continue;
        }
        let Some(serves) = r.serves else {
            match r.publisher {
                Some(_) => return deferred(),
                None => continue,
            }
        };
        if serves.by == Addressed::Time {
            timed.push(r.source);
            continue;
        }
        let Some(publisher) = r.publisher else {
            continue;
        };
        let Some(row) = live(view, publisher) else {
            continue;
        };
        let Some(index) = row.counters.index.get().filter(|_| !row.counters.indexing()) else {
            return deferred();
        };
        let read = match (skips, reader_pad(view, publisher, &index)) {
            (true, Some(pad)) => row.counters.snapshot().pad_pts(pad),
            _ => None,
        };
        let open = serves.remote || serves.by == Addressed::No || !index.grouped;
        let reach = if open { Reach::Read } else { Reach::Group };
        let plan = index.plan(to, mode, position, read, reach);
        match plan {
            SeekPlan::Skip { .. } => {}
            SeekPlan::Seek { .. } if serves.by == Addressed::No => {
                stuck = true;
                continue;
            }
            SeekPlan::Seek { .. } => all_skip = false,
            SeekPlan::Unseekable | SeekPlan::Deferred => continue,
        }
        any_byte = true;
        seeks.push((*r, serves, index, plan, reach));
    }
    if !any_byte && timed.is_empty() {
        return unseekable;
    }
    if any_byte && all_skip && timed.is_empty() && !stuck {
        return Planned { plan: SeekPlan::Skip { to_time: to }, targets: Vec::new() };
    }
    // A seek flushes every path, and one source cannot go back.
    if stuck {
        return unseekable;
    }
    let one_source = routes.iter().all(|r| r.source == routes[0].source);
    let single = seeks.len() == 1 && timed.is_empty() && one_source;
    if seeks.is_empty() {
        let plan = SeekPlan::Seek { to_byte: 0, key: to, to_time: to };
        return Planned { plan, targets: vec![(ALL, 0, to)] };
    }
    let (mut targets, mut decided, mut aligned) = (Vec::new(), None::<SeekPlan>, true);
    for (r, serves, index, plan, reach) in &seeks {
        // A path that could skip restarts at its own keyframe, a seek it costs nothing.
        let plan = match plan {
            SeekPlan::Skip { .. } => index.plan(to, mode, position, None, *reach),
            plan => *plan,
        };
        let SeekPlan::Seek { to_byte, key, .. } = plan else {
            continue;
        };
        // A source that cannot restart is flushed by no seek.
        if serves.by == Addressed::No {
            return unseekable;
        }
        let (byte, at) = index.restart(plan).unwrap_or((to_byte, key));
        aligned &= (byte, at) == (to_byte, key);
        aim(&mut targets, (r.source, byte, at));
        if let Some(publisher) = r.publisher.filter(|p| *p != r.source) {
            aim(&mut targets, (publisher, byte, key));
        }
        let later = |d: &SeekPlan| matches!(d, SeekPlan::Seek { key: k, .. } if *k >= key);
        if !decided.as_ref().is_some_and(later) {
            decided = Some(plan);
        }
    }
    for source in timed {
        aim(&mut targets, (source, 0, to));
    }
    let Some(plan) = decided else {
        return unseekable;
    };
    // One source restarting at its keyframe is told through the slot of every element.
    if let (true, true, SeekPlan::Seek { to_byte, key, .. }) = (aligned, single, plan) {
        targets = vec![(ALL, to_byte, key)];
    }
    Planned { plan, targets }
}

pub(crate) fn duration(view: &View) -> Option<Timestamp> {
    let rows = || view.rows.iter().filter(|row| !row.gone);
    let stated = rows().filter_map(|row| row.counters.duration()).max();
    stated.or_else(|| {
        rows()
            .filter_map(|row| row.counters.index.get())
            .filter_map(|i| i.duration.nanos())
            .max()
            .map(Timestamp)
    })
}

#[cfg(test)]
// Tests build their graphs and indexes on the heap.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::sync::Arc;

    use crate::batch::Inputs;
    use crate::clock::MockClock;
    use crate::command::{ALL, Target};
    use crate::ctx::Ctx;
    use crate::element::{
        Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
    };
    use crate::error::Error;
    use crate::format::OfferDesc;
    use crate::id::{ElementId, SinkPad, SrcPad};
    use crate::pipeline::{
        Addressed, Index, IndexEntry, Pipeline, SeekMode, SeekPlan, Seekable, Serves,
    };
    use crate::time::Timestamp;

    const MS: fn(u64) -> Timestamp = Timestamp::from_millis;

    static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
    static ONE: [PadDesc; 1] = [PadDesc { name: "pad", offers: &BYTES }];
    static TWO: [PadDesc; 2] =
        [PadDesc { name: "a", offers: &BYTES }, PadDesc { name: "b", offers: &BYTES }];

    const fn desc(
        name: &'static str,
        sinks: &'static [PadDesc],
        srcs: &'static [PadDesc],
        io: bool,
    ) -> ElementDesc {
        let inputs = if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Any };
        let (props, sched, latency) = (&[], SchedHint::Active, LatencyDesc::NONE);
        ElementDesc {
            name,
            sinks,
            srcs,
            props,
            sched,
            inputs,
            latency,
            io,
            make_default: None,
            couplings: &[],
        }
    }

    static FILE: ElementDesc = desc("file", &[], &ONE, false);
    static NET: ElementDesc = desc("net", &[], &ONE, true);
    static DEMUX: ElementDesc = desc("demux", &ONE, &TWO, false);
    static PASS: ElementDesc = desc("pass", &ONE, &ONE, false);
    static SINK: ElementDesc = desc("sink", &ONE, &[], false);
    static MIX: ElementDesc = desc("mix", &TWO, &[], false);

    struct El(&'static ElementDesc);

    impl Element for El {
        fn desc(&self) -> &'static ElementDesc {
            self.0
        }

        fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
            Ok(Flow::Eos)
        }
    }

    fn link(p: &mut Pipeline, (a, pa): (ElementId, u32), (b, pb): (ElementId, u32)) {
        p.link((a, SrcPad(pa)), (b, SinkPad(pb))).expect("links");
    }

    fn chain(p: &mut Pipeline, source: &'static ElementDesc) -> [ElementId; 3] {
        let (s, d, k) = (p.add(El(source)), p.add(El(&DEMUX)), p.add(El(&SINK)));
        link(p, (s, 0), (d, 0));
        link(p, (d, 0), (k, 0));
        [s, d, k]
    }

    fn gops(grouped: bool) -> Index {
        let entries =
            (0..16u64).map(|n| IndexEntry::key(n * 2_000_000_000, 1_000 + n * 1_000)).collect();
        Index {
            entries,
            file_len: Some(17_000),
            duration: MS(32_000),
            complete: true,
            grouped,
            ..Index::default()
        }
    }

    fn serve(p: &Pipeline, el: ElementId, by: Addressed, remote: bool) {
        p.counters_of(el).set_serves(Serves { by, remote });
    }

    fn out(p: &Pipeline, el: ElementId, pad: usize, pts: Timestamp) {
        p.counters_of(el).record_pts_out(pad, pts);
    }

    fn planned(p: &Pipeline, position: Timestamp, to: Timestamp) -> (SeekPlan, Vec<Target>) {
        let planned = p.control().planned(position, to, SeekMode::Accurate);
        (planned.plan, planned.targets)
    }

    fn seek(to_byte: u64, key: u64, to: u64) -> SeekPlan {
        SeekPlan::Seek { to_byte, key: MS(key), to_time: MS(to) }
    }

    #[test]
    fn a_file_seeks_at_its_keyframe_and_skips_inside_the_group_being_presented() {
        let mut p = Pipeline::new();
        let [_, demux, _] = chain(&mut p, &FILE);
        p.publish_index(demux, gops(true));
        assert_eq!(p.control().seekable(), Seekable::Byte);
        assert_eq!(
            planned(&p, MS(1_000), MS(5_000)),
            (seek(3_000, 4_000, 5_000), vec![(ALL, 3_000, MS(4_000))])
        );
        out(&p, demux, 0, MS(6_000));
        assert_eq!(planned(&p, MS(1_000), MS(5_000)).0, seek(3_000, 4_000, 5_000), "a later group");
        assert_eq!(
            planned(&p, MS(4_100), MS(5_000)),
            (SeekPlan::Skip { to_time: MS(5_000) }, vec![])
        );
        assert_eq!(
            planned(&p, Timestamp::NONE, MS(5_000)).0,
            seek(3_000, 4_000, 5_000),
            "no position never skips"
        );
    }

    #[test]
    fn a_remote_file_skips_to_all_that_was_read() {
        let mut p = Pipeline::new();
        let [source, demux, _] = chain(&mut p, &NET);
        p.publish_index(demux, gops(true));
        assert_eq!(
            planned(&p, MS(1_000), MS(5_000)).0,
            SeekPlan::Deferred,
            "the source has not answered"
        );
        assert_eq!(p.control().seekable(), Seekable::Unknown);
        serve(&p, source, Addressed::Byte, true);
        out(&p, demux, 0, MS(6_000));
        assert_eq!(planned(&p, MS(1_000), MS(5_000)).0, SeekPlan::Skip { to_time: MS(5_000) });
        assert_eq!(
            planned(&p, MS(1_000), MS(9_000)).0,
            seek(5_000, 8_000, 9_000),
            "past what was read"
        );
    }

    #[test]
    fn a_source_without_ranges_only_skips() {
        let mut p = Pipeline::new();
        let [source, demux, _] = chain(&mut p, &NET);
        p.publish_index(demux, gops(true));
        serve(&p, source, Addressed::No, true);
        assert_eq!(p.control().seekable(), Seekable::Forward);
        assert_eq!(planned(&p, MS(1_000), MS(5_000)).0, SeekPlan::Unseekable);
        out(&p, demux, 0, MS(6_000));
        assert_eq!(planned(&p, MS(1_000), MS(5_000)).0, SeekPlan::Skip { to_time: MS(5_000) });
    }

    #[test]
    fn a_source_addressed_by_time_is_told_the_target_and_its_demuxers_index_is_ignored() {
        let mut p = Pipeline::new();
        let [source, demux, _] = chain(&mut p, &NET);
        serve(&p, source, Addressed::Time, true);
        assert_eq!(p.control().seekable(), Seekable::Time);
        p.publish_index(demux, gops(true));
        out(&p, demux, 0, MS(6_000));
        assert_eq!(
            planned(&p, MS(1_000), MS(5_000)),
            (seek(0, 5_000, 5_000), vec![(ALL, 0, MS(5_000))])
        );
    }

    #[test]
    fn two_sources_are_each_told_the_keyframe_of_their_own_index() {
        let mut p = Pipeline::new();
        let [a, da, _] = chain(&mut p, &FILE);
        let [b, db, _] = chain(&mut p, &FILE);
        p.publish_index(da, gops(true));
        let entries = (0..32u64).map(|n| IndexEntry::key(n * 1_000_000_000, 50 * n)).collect();
        p.publish_index(
            db,
            Index {
                entries,
                file_len: Some(1_600),
                duration: MS(32_000),
                complete: true,
                ..Index::default()
            },
        );
        let (plan, mut targets) = planned(&p, MS(1_000), MS(5_500));
        targets.sort_by_key(|t| t.0.0);
        assert_eq!(plan, seek(250, 5_000, 5_500), "the latest keyframe decides");
        let want = [
            (a, 3_000, MS(4_000)),
            (da, 3_000, MS(4_000)),
            (b, 250, MS(5_000)),
            (db, 250, MS(5_000)),
        ];
        assert_eq!(targets, want);
    }

    #[test]
    fn every_path_into_a_mixer_is_planned() {
        let mut p = Pipeline::new();
        let mix = p.add(El(&MIX));
        let (a, da) = (p.add(El(&FILE)), p.add(El(&DEMUX)));
        let (b, db) = (p.add(El(&FILE)), p.add(El(&DEMUX)));
        for (n, (s, d)) in [(a, da), (b, db)].into_iter().enumerate() {
            link(&mut p, (s, 0), (d, 0));
            link(&mut p, (d, 0), (mix, n as u32));
        }
        p.publish_index(da, gops(true));
        let (plan, targets) = planned(&p, MS(1_000), MS(5_000));
        assert_eq!(
            (plan, targets.len()),
            (seek(3_000, 4_000, 5_000), 2),
            "the path without an index gets nothing"
        );
        p.publish_index(db, gops(false));
        let (plan, targets) = planned(&p, MS(1_000), MS(5_000));
        assert_eq!(plan, seek(3_000, 4_000, 5_000));
        assert_eq!(targets.len(), 4, "both sources and both demuxers, {targets:?}");
    }

    #[test]
    fn a_skip_waits_for_every_sinks_feeder_to_stamp() {
        let mut p = Pipeline::new();
        let [_, demux, _] = chain(&mut p, &FILE);
        let (dec, audio) = (p.add(El(&PASS)), p.add(El(&SINK)));
        link(&mut p, (demux, 1), (dec, 0));
        link(&mut p, (dec, 0), (audio, 0));
        p.publish_index(demux, gops(true));
        out(&p, demux, 0, MS(6_000));
        assert_eq!(
            planned(&p, MS(4_100), MS(5_000)).0,
            seek(3_000, 4_000, 5_000),
            "the audio feeder is unstamped"
        );
        out(&p, dec, 0, MS(4_200));
        assert_eq!(planned(&p, MS(4_100), MS(5_000)).0, SeekPlan::Skip { to_time: MS(5_000) });
    }

    #[test]
    fn a_restart_ahead_of_the_keyframe_tells_the_source_and_the_demuxer_apart() {
        let mut p = Pipeline::new();
        let [source, demux, _] = chain(&mut p, &FILE);
        let mut index = gops(true);
        (index.entries[2].restart, index.entries[2].restart_ns) = (2_000, 2_000_000_000);
        p.publish_index(demux, index);
        let (plan, targets) = planned(&p, MS(1_000), MS(4_500));
        assert_eq!(plan, seek(3_000, 4_000, 4_500));
        assert_eq!(targets, [(source, 2_000, MS(2_000)), (demux, 2_000, MS(4_000))]);
    }

    /// The restarts of a published index follow what is linked, so a link or an unlink made
    /// after it holds planning until the publisher publishes again or saw the change.
    #[test]
    fn a_link_after_the_index_holds_planning_until_the_publisher_sees_it() {
        let mut p = Pipeline::new();
        let [source, demux, _] = chain(&mut p, &FILE);
        serve(&p, source, Addressed::Byte, false);
        let other = p.add(El(&SINK));
        let id = p.link((demux, SrcPad(1)), (other, SinkPad(0))).expect("links");
        assert!(!p.counters_of(demux).indexing(), "no index to hold yet");
        p.publish_index(demux, gops(true));
        assert_eq!(planned(&p, MS(0), MS(5_000)).0, seek(3_000, 4_000, 5_000));
        p.unlink(id).expect("unlinks");
        assert!(p.counters_of(demux).indexing());
        assert_eq!(planned(&p, MS(0), MS(5_000)).0, SeekPlan::Deferred, "unlinked");
        p.publish_index(demux, gops(true));
        assert!(!p.counters_of(demux).indexing());
        assert_eq!(planned(&p, MS(0), MS(5_000)).0, seek(3_000, 4_000, 5_000), "published");
        p.link((demux, SrcPad(1)), (other, SinkPad(0))).expect("links");
        assert!(p.counters_of(demux).indexing(), "linked");
        p.publish_index(demux, gops(true));
        assert!(!p.counters_of(demux).indexing());
    }

    #[test]
    fn a_seek_before_the_index_is_held_and_sent_when_it_lands() {
        let mut p = Pipeline::new();
        let [_, demux, _] = chain(&mut p, &FILE);
        assert_eq!(p.control().duration(), None);
        // A head the element publishes before what it said is coming, as mkvdemux does.
        p.publish_index(
            demux,
            Index { file_len: Some(17_000), duration: MS(32_000), ..Index::default() },
        );
        p.counters_of(demux).set_indexing(true);
        assert_eq!(
            planned(&p, MS(0), MS(5_000)).0,
            SeekPlan::Deferred,
            "not on the head's estimate"
        );
        let run = p.step(Arc::new(MockClock::new())).expect("runs");
        let control = p.control();
        assert_eq!(control.seek(MS(5_000), SeekMode::Accurate), Ok(SeekPlan::Deferred));
        assert_eq!(control.pending(), Some((MS(5_000), SeekMode::Accurate)));
        assert_eq!(
            control.seek(MS(7_000), SeekMode::Keyframe),
            Ok(SeekPlan::Deferred),
            "a newer one replaces it"
        );
        assert_eq!(control.seek_gen(), 0);
        p.publish_index(demux, gops(true));
        assert_eq!((control.pending(), control.seek_gen()), (None, 1), "sent on the publication");
        assert_eq!(control.duration(), Some(MS(32_000)), "an index's");
        p.counters_of(demux).set_duration(40_000_000_000);
        assert_eq!(control.duration(), Some(MS(40_000)), "the stated one");
        control.stop();
        drop(run);
    }

    #[test]
    fn a_held_seek_is_planned_again_from_the_position_its_request_gave() {
        // The clock reads inside the group being presented, so the pipeline's position would skip.
        let held = |p: &mut Pipeline| {
            let [_, demux, _] = chain(p, &FILE);
            p.publish_index(
                demux,
                Index { file_len: Some(17_000), duration: MS(32_000), ..Index::default() },
            );
            p.counters_of(demux).set_indexing(true);
            let clock = Arc::new(MockClock::new());
            let run = p.step(Arc::clone(&clock) as Arc<dyn crate::clock::Clock>).expect("runs");
            clock.advance(MS(4_100));
            assert_eq!(p.control().position(), MS(4_100));
            (demux, run)
        };
        let land = |p: &Pipeline, demux: ElementId| {
            out(p, demux, 0, MS(6_000));
            p.publish_index(demux, gops(true));
        };

        let mut p = Pipeline::new();
        let (demux, run) = held(&mut p);
        let control = p.control();
        assert_eq!(
            control.seek_from(Timestamp::NONE, MS(5_000), SeekMode::Accurate),
            Ok(SeekPlan::Deferred)
        );
        land(&p, demux);
        assert_eq!(
            (control.pending(), control.seek_gen()),
            (None, 1),
            "no position never skips, a seek was sent"
        );
        control.stop();
        drop(run);

        let mut p = Pipeline::new();
        let (demux, run) = held(&mut p);
        let control = p.control();
        assert_eq!(control.seek(MS(5_000), SeekMode::Accurate), Ok(SeekPlan::Deferred));
        land(&p, demux);
        assert_eq!(
            (control.pending(), control.seek_gen()),
            (None, 0),
            "the pipeline's own position skips"
        );
        control.stop();
        drop(run);
    }

    #[test]
    fn a_head_without_entries_plans_on_the_estimate() {
        let mut p = Pipeline::new();
        let [_, demux, _] = chain(&mut p, &FILE);
        let head = Index { file_len: Some(17_000), duration: MS(32_000), ..Index::default() };
        p.publish_index(demux, head);
        let guess = SeekPlan::Seek { to_byte: 2_125, key: MS(4_000), to_time: MS(4_000) };
        assert_eq!(p.control().planned(MS(0), MS(4_000), SeekMode::Keyframe).plan, guess);
        assert_eq!(p.control().duration(), Some(MS(32_000)));
    }

    #[test]
    fn a_sink_fed_by_no_index_leaves_the_graph_unseekable() {
        let mut p = Pipeline::new();
        chain(&mut p, &FILE);
        assert_eq!(p.control().seekable(), Seekable::No);
        assert_eq!(planned(&p, MS(1_000), MS(5_000)).0, SeekPlan::Unseekable);
        let mut p = Pipeline::new();
        chain(&mut p, &NET);
        assert_eq!(p.control().seekable(), Seekable::No);
        assert_eq!(planned(&p, MS(1_000), MS(5_000)).0, SeekPlan::Unseekable);
    }

    #[test]
    fn a_source_that_cannot_restart_refuses_a_seek_another_path_needs() {
        let mut p = Pipeline::new();
        let [source, demux, _] = chain(&mut p, &NET);
        let [_, side, _] = chain(&mut p, &FILE);
        serve(&p, source, Addressed::No, true);
        p.publish_index(demux, gops(true));
        p.publish_index(side, gops(true));
        out(&p, demux, 0, MS(6_000));
        out(&p, side, 0, MS(6_000));
        assert_eq!(
            planned(&p, MS(4_100), MS(5_000)).0,
            SeekPlan::Skip { to_time: MS(5_000) },
            "both skip"
        );
        p.counters_of(side).forget_pts_out();
        out(&p, side, 0, MS(1_000));
        assert_eq!(
            planned(&p, MS(4_100), MS(5_000)),
            (SeekPlan::Unseekable, vec![]),
            "no byte for a source that cannot go back"
        );
        p.counters_of(demux).forget_pts_out();
        assert_eq!(
            planned(&p, MS(4_100), MS(5_000)),
            (SeekPlan::Unseekable, vec![]),
            "a source that cannot go back"
        );
    }

    #[test]
    fn a_seek_is_held_only_within_a_run_and_a_new_run_drops_the_last_hold() {
        let mut p = Pipeline::new();
        let [_, demux, _] = chain(&mut p, &FILE);
        p.counters_of(demux).set_indexing(true);
        let control = p.control();
        assert_eq!(
            control.seek(MS(5_000), SeekMode::Accurate),
            Err(crate::command::CommandError::NotRunning)
        );
        assert_eq!(control.pending(), None, "nothing held before the run");
        let run = p.step(Arc::new(MockClock::new())).expect("runs");
        assert_eq!(control.seek(MS(5_000), SeekMode::Accurate), Ok(SeekPlan::Deferred));
        drop(run.finish());
        assert_eq!(
            control.pending(),
            Some((MS(5_000), SeekMode::Accurate)),
            "the run ended with it held"
        );
        let station = Arc::new(crate::station::Station::new());
        control.begin_run(Arc::new(MockClock::new()), vec![station], &[true], false);
        assert_eq!(control.pending(), None, "not the new run's");
    }
}
