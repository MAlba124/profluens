use profluens_core::pipeline::{Caption, Index, IndexEntry, Reach, SeekMode, SeekPlan};
use profluens_core::time::Timestamp;

const MS: fn(u64) -> Timestamp = Timestamp::from_millis;
const DURATION: Timestamp = Timestamp::from_secs(40);
const ACCURATE: SeekMode = SeekMode::Accurate;

fn whole(entries: &[(u64, u64)], file_len: u64) -> Index {
    let entries = entries.iter().map(|&(t, b)| IndexEntry::key(t, b)).collect();
    Index {
        entries,
        file_len: Some(file_len),
        duration: DURATION,
        complete: true,
        ..Index::default()
    }
}

fn gops() -> Index {
    let entries: Vec<(u64, u64)> = (0..4).map(|k| (k * 10_000_000_000, k * 10_000)).collect();
    whole(&entries, 40_000)
}

fn skip(ms: u64) -> SeekPlan {
    SeekPlan::Skip { to_time: MS(ms) }
}

fn seek(to_byte: u64, key: u64, ms: u64) -> SeekPlan {
    SeekPlan::Seek { to_byte, key: MS(key), to_time: MS(ms) }
}

fn plan(index: &Index, target: u64, position: u64, demuxed: Option<u64>) -> SeekPlan {
    index.plan(MS(target), ACCURATE, MS(position), demuxed.map(MS), Reach::Read)
}

fn video(index: &Index, target: u64, position: u64, demuxed: Option<u64>) -> SeekPlan {
    index.plan(MS(target), ACCURATE, MS(position), demuxed.map(MS), Reach::Group)
}

#[test]
fn a_target_at_or_behind_the_play_position_is_a_seek() {
    let index = gops();
    assert_eq!(plan(&index, 4_000, 4_000, Some(6_000)), seek(0, 0, 4_000), "the position itself");
    assert_eq!(plan(&index, 3_999, 4_000, Some(6_000)), seek(0, 0, 3_999), "one frame back");
    assert_eq!(plan(&index, 0, 4_000, Some(6_000)), seek(0, 0, 0));
    assert_eq!(plan(&index, 4_001, 4_000, Some(6_000)), skip(4_001), "and one ahead is a skip");
    assert_eq!(plan(&index, 12_000, 25_000, Some(39_000)), seek(10_000, 10_000, 12_000));
    for reach in [plan, video] {
        assert_eq!(reach(&index, 4_000, 4_000, Some(6_000)), seek(0, 0, 4_000));
        assert_eq!(reach(&index, 4_001, 4_000, Some(6_000)), skip(4_001));
    }
}

/// A video skip decodes on from where the decoder is, so it is kept for the group being presented.
#[test]
fn a_video_skip_reaches_only_into_the_group_being_presented() {
    let index = gops();
    assert_eq!(
        video(&index, 14_000, 9_000, Some(12_000)),
        seek(10_000, 10_000, 14_000),
        "the next group"
    );
    assert_eq!(plan(&index, 14_000, 9_000, Some(12_000)), skip(14_000), "where audio skips");
    assert_eq!(video(&index, 14_000, 10_000, Some(12_000)), skip(14_000), "on its keyframe");
    assert_eq!(video(&index, 19_999, 12_000, Some(15_000)), skip(19_999), "its last moment");
    assert_eq!(
        video(&index, 20_000, 12_000, Some(25_000)),
        seek(20_000, 20_000, 20_000),
        "the next keyframe"
    );
    assert_eq!(video(&index, 9_999, 4_000, Some(6_000)), skip(9_999));
    assert_eq!(
        video(&index, 25_000, 4_000, Some(21_000)),
        seek(20_000, 20_000, 25_000),
        "two groups on"
    );
    assert_eq!(
        video(&index, 14_000, 10_000, Some(9_999)),
        seek(10_000, 10_000, 14_000),
        "and not read"
    );
    assert_eq!(video(&index, 14_000, 10_000, None), seek(10_000, 10_000, 14_000), "or unsettled");
    // An estimated byte names no keyframe, and a skip reaches what was read, as for audio.
    let blind = whole(&[], 40_000);
    assert_eq!(video(&blind, 5_000, 4_000, Some(6_000)), skip(5_000));
    assert_eq!(video(&blind, 6_001, 4_000, Some(6_000)), seek(6_001, 6_001, 6_001));
    let late = whole(&[(MS(2_000).0, 900)], 40_000);
    assert_eq!(video(&late, 1_000, 500, Some(2_500)), skip(1_000));
    assert_eq!(video(&late, 1_000, 500, Some(1_500)), seek(900, 2_000, 1_000), "first keyframe");
    assert_eq!(video(&late, 2_500, 1_500, Some(3_000)), seek(900, 2_000, 2_500));
}

#[test]
fn the_keyframe_at_or_before_the_target_decides_against_the_demuxer() {
    let index = gops();
    assert_eq!(plan(&index, 14_000, 9_000, Some(10_000)), skip(14_000));
    let short = Timestamp(MS(10_000).0 - 1);
    let got = index.plan(MS(14_000), ACCURATE, MS(9_000), Some(short), Reach::Read);
    assert_eq!(got, seek(10_000, 10_000, 14_000), "the target is the time, the keyframe the byte");
    assert_eq!(plan(&index, 9_999, 4_000, Some(6_000)), skip(9_999));
    assert_eq!(
        plan(&index, 10_000, 4_000, Some(6_000)),
        seek(10_000, 10_000, 10_000),
        "the next group"
    );
    assert_eq!(plan(&index, 25_000, 4_000, Some(19_000)), seek(20_000, 20_000, 25_000));
    assert_eq!(plan(&index, 25_000, 4_000, Some(21_000)), skip(25_000));
}

#[test]
fn a_demuxer_position_nobody_may_trust_is_a_seek() {
    let index = gops();
    assert_eq!(plan(&index, 5_000, 4_000, None), seek(0, 0, 5_000), "the last seek is not applied");
    let unset = Some(Timestamp::NONE);
    assert_eq!(index.plan(MS(5_000), ACCURATE, MS(4_000), unset, Reach::Read), seek(0, 0, 5_000));
    let got = index.plan(MS(5_000), ACCURATE, Timestamp::NONE, Some(MS(6_000)), Reach::Group);
    assert_eq!(got, seek(0, 0, 5_000));
    let none = index.plan(Timestamp::NONE, ACCURATE, MS(4_000), Some(MS(6_000)), Reach::Group);
    assert_eq!(none, SeekPlan::Unseekable, "no target");
}

#[test]
fn keyframe_mode_is_a_seek_to_the_keyframe_at_its_own_time() {
    let index = gops();
    let key = |target, position, demuxed: Option<u64>| {
        index.plan(MS(target), SeekMode::Keyframe, MS(position), demuxed.map(MS), Reach::Group)
    };
    assert_eq!(key(14_000, 2_000, None), seek(10_000, 10_000, 10_000));
    assert_eq!(key(14_000, 12_000, Some(30_000)), seek(10_000, 10_000, 10_000), "never a skip");
    assert_eq!(key(39_999, 0, Some(0)), seek(30_000, 30_000, 30_000));
}

#[test]
fn an_index_with_no_entry_skips_only_into_what_was_read() {
    let index = whole(&[], 40_000);
    assert_eq!(plan(&index, 5_000, 4_000, Some(6_000)), skip(5_000));
    assert_eq!(plan(&index, 6_000, 4_000, Some(6_000)), skip(6_000), "the newest unit out");
    assert_eq!(plan(&index, 6_001, 4_000, Some(6_000)), seek(6_001, 6_001, 6_001), "one past it");
    assert_eq!(plan(&index, 3_000, 4_000, Some(6_000)), seek(3_000, 3_000, 3_000), "backwards");
    assert_eq!(plan(&index, 5_000, 4_000, None), seek(5_000, 5_000, 5_000));
    let late = whole(&[(MS(2_000).0, 900)], 40_000);
    assert_eq!(plan(&late, 1_000, 500, Some(2_500)), skip(1_000));
    assert_eq!(plan(&late, 1_800, 500, Some(1_500)), seek(900, 2_000, 1_800), "first keyframe");
    let blind = Index::default();
    assert_eq!(plan(&blind, 5_000, 4_000, Some(6_000)), SeekPlan::Unseekable);
    let timeless = Index { duration: Timestamp::NONE, ..index };
    let got = timeless.plan(MS(5_000), ACCURATE, MS(4_000), Some(MS(6_000)), Reach::Read);
    assert_eq!(got, SeekPlan::Unseekable);
}

#[test]
fn audio_with_every_entry_a_keyframe_skips_within_what_was_read() {
    let entries: Vec<(u64, u64)> = (0..400).map(|k| (k * 100_000_000, 4_096 + k * 1_000)).collect();
    let index = whole(&entries, 404_096);
    assert_eq!(plan(&index, 7_250, 5_000, Some(8_000)), skip(7_250));
    assert_eq!(plan(&index, 8_000, 5_000, Some(8_000)), skip(8_000));
    assert_eq!(plan(&index, 8_099, 5_000, Some(8_000)), skip(8_099), "the packet being read");
    assert_eq!(
        plan(&index, 8_100, 5_000, Some(8_000)),
        seek(85_096, 8_100, 8_100),
        "the one after"
    );
    assert_eq!(plan(&index, 4_950, 5_000, Some(8_000)), seek(53_096, 4_900, 4_950), "backwards");
}

// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

struct Case {
    index: Index,
    start: u64,
    position: u64,
    demuxed: Option<u64>,
    target: u64,
    mode: SeekMode,
    reach: Reach,
}

fn draw(rng: &mut Rng) -> Case {
    let (count, end) = (rng.below(12), 1_000 + rng.below(100_000));
    let mut times: Vec<u64> = (0..count).map(|_| rng.below(end)).collect();
    times.sort_unstable();
    times.dedup();
    let entries: Vec<(u64, u64)> = times.iter().map(|t| (*t, 7 + t * 3)).collect();
    let start = match entries.is_empty() || rng.below(4) == 0 {
        true => rng.below(entries.first().map_or(end, |e| e.0 + 1)),
        false => entries[rng.below(entries.len() as u64) as usize].0,
    };
    let position = start + rng.below(end - start + 1);
    let demuxed = (rng.below(5) != 0).then(|| start + rng.below(end - start + 1));
    let target = match rng.below(6) {
        0 => position,
        1 => demuxed.unwrap_or(position),
        2 if !entries.is_empty() => entries[rng.below(entries.len() as u64) as usize].0,
        _ => rng.below(end + 1),
    };
    let mode = if rng.below(4) == 0 { SeekMode::Keyframe } else { SeekMode::Accurate };
    let reach = if rng.below(2) == 0 { Reach::Group } else { Reach::Read };
    let mut index = whole(&entries, end * 3 + 7);
    index.duration = Timestamp(100_000 + 1_000);
    Case { index, start, position, demuxed, target, mode, reach }
}

#[test]
fn a_skip_needs_nothing_the_demuxer_has_passed_and_a_seek_goes_to_the_floor() {
    let mut rng = Rng(0x5EED);
    let (mut skips, mut seeks, mut blind) = (0u32, 0u32, 0u32);
    for n in 0..200_000 {
        let case = draw(&mut rng);
        let (target, duration) = (Timestamp(case.target), Timestamp(100_000 + 1_000));
        let floor = case
            .index
            .entries
            .iter()
            .rev()
            .find(|e| e.time_ns <= case.target)
            .map(|e| (e.time_ns, e.byte));
        // Before the first keyframe a seek starts at it.
        let first = case.index.entries.first().map(|e| (e.time_ns, e.byte));
        let from = floor.or(first.filter(|f| case.target < f.0));
        let demuxed = case.demuxed.map(Timestamp);
        let got = case.index.plan(target, case.mode, Timestamp(case.position), demuxed, case.reach);
        let grouped = case.reach == Reach::Group && floor.is_some();
        let needed_from = from.map_or(case.target, |(t, _)| t);
        match got {
            SeekPlan::Skip { to_time } => {
                skips += 1;
                assert_eq!((to_time, case.mode), (target, SeekMode::Accurate), "case {n}");
                assert!(case.target > case.position, "case {n}: a skip is forwards");
                assert!(needed_from >= case.start, "case {n}: the demuxer passed what it needs");
                let out = case.demuxed.unwrap_or_else(|| panic!("case {n}: no position known"));
                assert!(needed_from <= out, "case {n}: the keyframe has not left the demuxer");
                assert!(
                    !grouped || needed_from <= case.position,
                    "case {n}: a group ahead of the play position"
                );
            }
            SeekPlan::Seek { to_byte, key, to_time } => {
                seeks += 1;
                assert_eq!(key, Timestamp(needed_from), "case {n}: timed as its byte is");
                let estimate = (case.target as u128 * (case.index.file_len.unwrap_or(0) as u128)
                    / duration.0 as u128) as u64;
                assert_eq!(to_byte, from.map_or(estimate, |(_, b)| b), "case {n}");
                let at = match case.mode {
                    SeekMode::Accurate | SeekMode::Preview => case.target,
                    SeekMode::Keyframe => needed_from,
                };
                assert_eq!(to_time, Timestamp(at), "case {n}");
                let cheap = case.demuxed.is_some_and(|out| needed_from <= out);
                let forwards =
                    case.target > case.position && (!grouped || needed_from <= case.position);
                assert!(
                    !(cheap && forwards && case.mode == SeekMode::Accurate),
                    "case {n}: flushed what a skip would have kept"
                );
            }
            SeekPlan::Unseekable | SeekPlan::Deferred => {
                panic!("case {n}: an index with a length has an answer")
            }
        }
        blind += case.demuxed.is_none() as u32;
    }
    assert!(skips > 10_000 && seeks > 50_000 && blind > 20_000, "{skips} {seeks} {blind}");
}

fn read_so_far(bytes_at_ten: u64) -> Index {
    Index { complete: false, ..whole(&[(0, 0), (MS(10_000).0, bytes_at_ten)], 40_000) }
}

#[test]
fn past_the_last_entry_of_an_incomplete_index_the_byte_is_estimated() {
    let index = read_so_far(10_000);
    assert_eq!(plan(&index, 5_000, 20_000, None), seek(0, 0, 5_000), "inside what was read");
    assert_eq!(
        plan(&index, 10_000, 20_000, None),
        seek(10_000, 10_000, 10_000),
        "on the last entry"
    );
    assert_eq!(
        plan(&index, 15_000, 20_000, None),
        seek(15_000, 15_000, 15_000),
        "past it, an estimate"
    );
    let whole = Index { complete: true, ..index.clone() };
    assert_eq!(
        plan(&whole, 15_000, 20_000, None),
        seek(10_000, 10_000, 15_000),
        "a whole index floors"
    );
    let dense = read_so_far(30_000);
    assert_eq!(plan(&dense, 15_000, 20_000, None), seek(30_000, 10_000, 15_000));
    // An estimated byte names no keyframe, so a skip reaches what was read whatever the reach.
    assert_eq!(video(&index, 15_000, 12_000, Some(16_000)), skip(15_000));
    assert_eq!(video(&whole, 15_000, 9_000, Some(16_000)), seek(10_000, 10_000, 15_000));
    assert_eq!(plan(&Index::default(), 15_000, 20_000, None), SeekPlan::Unseekable);
}

/// A growing index knows no keyframe past its last entry whatever the estimate says, a link
/// there is told so and a keyframe seek is presented from its target, not from the entry.
#[test]
fn past_the_last_entry_of_an_incomplete_index_the_keyframe_is_unknown() {
    let dense = read_so_far(30_000);
    let timeless = Index { duration: Timestamp::NONE, ..read_so_far(10_000) };
    for (index, what) in
        [(read_so_far(10_000), "estimated"), (dense, "dense"), (timeless, "timeless")]
    {
        assert_eq!(index.keyframe(MS(15_000)), None, "{what}");
        assert_eq!(index.keyframe(MS(10_000)), Some(&index.entries[1]), "{what}: on the entry");
        assert_eq!(index.restart_for(7, MS(15_000)), index.resolve(MS(15_000)), "{what}");
        assert_eq!(index.restart_for(7, MS(10_000)), Some((index.entries[1].byte, MS(10_000))));
        let key = index.plan(MS(15_000), SeekMode::Keyframe, MS(2_000), None, Reach::Group);
        let SeekPlan::Seek { to_time, .. } = key else { panic!("{what}: {key:?}") };
        assert_eq!(to_time, MS(15_000), "{what}: from the target");
        let whole = Index { complete: true, ..index.clone() };
        let entry = Some((index.entries[1].byte, MS(10_000)));
        assert_eq!(whole.restart_for(7, MS(15_000)), entry, "{what}: a whole index floors");
        assert_eq!(whole.keyframe(MS(15_000)), Some(&index.entries[1]), "{what}");
    }
    let key =
        |index: &Index| index.plan(MS(15_000), SeekMode::Keyframe, MS(2_000), None, Reach::Group);
    assert_eq!(key(&read_so_far(30_000)), seek(30_000, 10_000, 15_000), "the entry's byte");
    let timeless = Index { duration: Timestamp::NONE, ..read_so_far(10_000) };
    assert_eq!(key(&timeless), seek(10_000, 10_000, 15_000), "no estimate, the entry's byte");
    assert_eq!(key(&read_so_far(10_000)), seek(15_000, 15_000, 15_000), "the estimate");
    // What the demuxer put out decides a skip past the entry, not the entry.
    let dense = read_so_far(30_000);
    assert_eq!(video(&dense, 15_000, 9_000, Some(16_000)), skip(15_000));
    assert_eq!(video(&dense, 15_000, 9_000, Some(12_000)), seek(30_000, 10_000, 15_000));
}

#[test]
fn a_seek_restarts_the_source_where_its_keyframe_entry_says() {
    let mut index = gops();
    index.entries[1] = IndexEntry { restart: 9_000, restart_ns: MS(9_500).0, ..index.entries[1] };
    let plan = index.plan(MS(14_000), ACCURATE, MS(30_000), None, Reach::Group);
    assert_eq!(plan, seek(10_000, 10_000, 14_000));
    assert_eq!(index.restart(plan), Some((9_000, MS(9_500))), "ahead of the keyframe");
    assert_eq!(
        index.restart(seek(20_000, 20_000, 24_000)),
        Some((20_000, MS(20_000))),
        "nothing ahead"
    );
    let estimate = read_so_far(10_000).plan(MS(15_000), ACCURATE, MS(30_000), None, Reach::Read);
    assert_eq!(
        read_so_far(10_000).restart(estimate),
        Some((15_000, MS(15_000))),
        "an estimate restarts at itself"
    );
    assert_eq!(index.restart(skip(14_000)), None);
    assert_eq!(index.restart(SeekPlan::Unseekable), None);
}

mod published {
    use std::sync::Arc;

    use profluens_core::batch::Inputs;
    use profluens_core::clock::{Clock, MockClock};
    use profluens_core::command::{COMMAND_LOG_CAP, Command};
    use profluens_core::ctx::Ctx;
    use profluens_core::element::{
        Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
    };
    use profluens_core::error::Error;
    use profluens_core::format::OfferDesc;
    use profluens_core::id::{ElementId, SinkPad, SrcPad};
    use profluens_core::log::DebugSpec;
    use profluens_core::pipeline::{Index, IndexEntry, Pipeline, SeekMode, SeekPlan};
    use profluens_core::sched::{Outcome, Stepped};
    use profluens_core::time::Timestamp;

    static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
    static ONE_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
    static ONE_OUT: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];

    const fn desc(
        name: &'static str,
        sinks: &'static [PadDesc],
        srcs: &'static [PadDesc],
        sched: SchedHint,
    ) -> ElementDesc {
        ElementDesc {
            name,
            sinks,
            srcs,
            props: &[],
            sched,
            inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
            latency: LatencyDesc::NONE,
            make_default: None,
            io: false,
            couplings: &[],
        }
    }

    static MAPPER: ElementDesc = desc("mapper", &[], &ONE_OUT, SchedHint::Active);
    static DRAIN: ElementDesc = desc("drain", &ONE_IN, &[], SchedHint::Passive);
    static SINK: ElementDesc = desc("sink", &ONE_IN, &[], SchedHint::Active);

    fn first(n: u64) -> Index {
        let entries = (0..n).map(|k| IndexEntry::key(k * 1_000_000_000, k * 100)).collect();
        Index {
            entries,
            file_len: Some(400),
            duration: Timestamp::from_secs(4),
            complete: n == 4,
            ..Index::default()
        }
    }

    struct Mapper {
        read: u64,
    }

    impl Element for Mapper {
        fn desc(&self) -> &'static ElementDesc {
            &MAPPER
        }

        fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
            ctx.publish_index(first(1));
            Ok(())
        }

        fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
            if self.read == 3 {
                return Ok(Flow::Eos);
            }
            let Some(buf) = ctx.try_alloc() else {
                return Ok(Flow::Ok);
            };
            ctx.out(SrcPad(0)).push(buf);
            self.read += 1;
            ctx.publish_index(first(self.read + 1));
            Ok(Flow::Ok)
        }
    }

    /// Says it will index at start, and publishes once, on its second buffer.
    struct Indexer {
        read: u64,
    }

    impl Element for Indexer {
        fn desc(&self) -> &'static ElementDesc {
            &MAPPER
        }

        fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
            ctx.will_index();
            Ok(())
        }

        fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
            if self.read == 6 {
                return Ok(Flow::Eos);
            }
            let Some(buf) = ctx.try_alloc() else {
                return Ok(Flow::Ok);
            };
            ctx.out(SrcPad(0)).push(buf);
            self.read += 1;
            if self.read == 2 {
                ctx.publish_index(first(4));
            }
            Ok(Flow::Ok)
        }
    }

    struct Drain(&'static ElementDesc);

    impl Element for Drain {
        fn desc(&self) -> &'static ElementDesc {
            self.0
        }

        fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
            while inputs.pop().is_some() {}
            Ok(Flow::Ok)
        }
    }

    #[test]
    fn a_seek_held_for_an_index_is_sent_once_the_full_log_has_room() {
        let clock = MockClock::new();
        let shared: Arc<dyn Clock> = Arc::new(clock.clone());
        let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
        p.set_pool(64, 4);
        let indexer = p.add(Indexer { read: 0 });
        let drain = p.add(Drain(&SINK));
        p.link(indexer.src(SrcPad(0)), drain.sink(SinkPad(0))).expect("link");
        let mut run = p.step(Arc::clone(&shared)).expect("step");
        let group = |run: &Stepped, el: ElementId| {
            (0..run.groups()).find(|g| run.elements(*g).contains(&el)).expect("a group")
        };
        let (gi, gd) = (group(&run, indexer), group(&run, drain));
        let to = Timestamp::from_secs(2);
        assert_eq!(run.control().seek(to, SeekMode::Accurate), Ok(SeekPlan::Deferred));
        assert_eq!(run.control().pending(), Some((to, SeekMode::Accurate)));
        let mut sent = 0;
        while run.control().append(Command::SetOffset { element: drain, by: sent }).is_ok() {
            sent += 1;
        }
        assert_eq!(sent as usize, COMMAND_LOG_CAP, "the log is full");
        for _ in 0..3 {
            run.pass(gi).expect("the indexer reads and publishes");
        }
        assert_eq!(
            run.control().pending(),
            Some((to, SeekMode::Accurate)),
            "the index landed on a full log, so the seek is still held"
        );
        run.pass(gd).expect("the drain catches up");
        assert_eq!(run.control().pending(), None, "the held seek went out once there was room");
        run.finish().expect("finish");
    }

    #[test]
    fn an_element_publishes_its_index_and_the_controller_reads_the_newest() {
        let clock = MockClock::new();
        let shared: Arc<dyn Clock> = Arc::new(clock.clone());
        let mut p = Pipeline::with(DebugSpec::default(), false, Arc::clone(&shared));
        p.set_pool(64, 4);
        let mapper = p.add(Mapper { read: 0 });
        let drain = p.add(Drain(&DRAIN));
        p.link(mapper.src(SrcPad(0)), drain.sink(SinkPad(0))).expect("link");
        let tap = p.tap_handle();
        assert_eq!((p.index(mapper), tap.index(mapper)), (None, None), "nothing published yet");
        p.preroll().expect("preroll");
        assert_eq!(p.index(mapper).as_deref(), Some(&first(1)), "the head");
        let held = p.index(mapper).expect("an index");
        let mut run = p.step(Arc::clone(&shared)).expect("step");
        for _ in 0..1_000 {
            let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
            let Some(&g) = ready.first() else { break };
            if let Outcome::Exit = run.pass(g).expect("pass") {
                continue;
            }
        }
        run.finish().expect("finish");
        assert_eq!(tap.index(mapper).as_deref(), Some(&first(4)), "grown as it read, and whole");
        assert_eq!(*held, first(1), "a reader keeps the version it took");
        p.publish_index(drain, first(2));
        assert_eq!(tap.index(drain).as_deref(), Some(&first(2)));
        assert_eq!(p.index(mapper).as_deref(), Some(&first(4)), "each element its own");
        assert_eq!(p.index(ElementId(9)), None, "no such element");
    }
}

/// An estimate before the first keyframe lands past it, so the seek starts at the first.
#[test]
fn a_target_before_the_first_keyframe_restarts_at_it() {
    let index = whole(&[(10_000_000_000, 1_000), (20_000_000_000, 50_000)], 100_000);
    assert_eq!(index.resolve(MS(8_000)), Some((1_000, MS(10_000))));
    let keyframe = index.plan(MS(8_000), SeekMode::Keyframe, MS(25_000), None, Reach::Group);
    assert_eq!(keyframe, seek(1_000, 10_000, 10_000));
    assert_eq!(video(&index, 8_000, 25_000, None), seek(1_000, 10_000, 8_000));
    assert_eq!(index.resolve(Timestamp::NONE), None, "no target, no seek");
    assert_eq!(index.resolve(MS(12_000)), Some((1_000, MS(10_000))), "past it the floor");
}

fn caption(stream: u32, start_ms: u64, end_ms: u64, byte: u64) -> Caption {
    Caption { stream, time_ns: MS(start_ms).0, end_ns: MS(end_ms).0, byte }
}

#[test]
fn a_link_restarts_at_the_key_before_the_point() {
    let index = gops();
    assert_eq!(index.restart_for(7, MS(25_000)), Some((20_000, MS(20_000))));
    assert_eq!(index.restart_for(7, MS(5_000)), Some((0, MS(0))), "before the second key");
    assert_eq!(Index::default().restart_for(7, MS(5_000)), None, "no entry");
}

#[test]
fn a_link_goes_back_for_the_stream_s_caption_up_at_the_point() {
    let mut index = gops();
    index.captions = vec![
        caption(7, 8_000, 12_000, 8_500),
        caption(8, 9_000, 40_000, 9_500),
        caption(7, 19_500, 19_900, 19_600),
        caption(7, 21_000, 22_000, 19_700),
        caption(7, 23_000, 24_000, 23_500),
    ];
    assert_eq!(index.restart_for(7, MS(11_000)), Some((0, MS(0))), "up from the GOP before");
    assert_eq!(index.restart_for(7, MS(12_000)), Some((10_000, MS(10_000))), "ended at the point");
    assert_eq!(
        index.restart_for(7, MS(25_000)),
        Some((20_000, MS(20_000))),
        "the later one is past"
    );
    assert_eq!(index.restart_for(8, MS(25_000)), Some((0, MS(0))), "another stream's long one");
    assert_eq!(
        index.restart_for(7, MS(20_500)),
        Some((10_000, MS(10_000))),
        "stored below the key, starting before the next"
    );
    assert_eq!(index.restart_for(9, MS(25_000)), Some((20_000, MS(20_000))), "no caption");
}

#[test]
fn a_link_of_an_uncued_stream_goes_back_the_reach() {
    let mut index = gops();
    index.uncued = vec![7];
    assert_eq!(index.restart_for(7, MS(25_000)), Some((0, MS(0))));
    assert_eq!(index.restart_for(7, MS(35_000)), Some((10_000, MS(10_000))));
    assert_eq!(index.restart_for(8, MS(35_000)), Some((30_000, MS(30_000))), "a cued stream");
}

mod warm {
    use profluens_core::batch::Inputs;
    use profluens_core::ctx::Ctx;
    use profluens_core::element::{
        Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
    };
    use profluens_core::error::Error;
    use profluens_core::format::OfferDesc;
    use profluens_core::id::{SinkPad, SrcPad};
    use profluens_core::pipeline::{Pipeline, SeekMode, SeekPlan};

    use super::{MS, gops};
    use crate::link_leak::alloc_calls;

    static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
    static ONE: [PadDesc; 1] = [PadDesc { name: "pad", offers: &BYTES }];

    const fn desc(
        name: &'static str,
        sinks: &'static [PadDesc],
        srcs: &'static [PadDesc],
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
            io: false,
            make_default: None,
            couplings: &[],
        }
    }

    static FILE: ElementDesc = desc("file", &[], &ONE);
    static DEMUX: ElementDesc = desc("demux", &ONE, &ONE);
    static SINK: ElementDesc = desc("sink", &ONE, &[]);

    struct El(&'static ElementDesc);

    impl Element for El {
        fn desc(&self) -> &'static ElementDesc {
            self.0
        }

        fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
            Ok(Flow::Eos)
        }
    }

    #[test]
    fn a_warm_seek_plan_allocates_nothing() {
        let mut p = Pipeline::new();
        let (file, demux, sink) = (p.add(El(&FILE)), p.add(El(&DEMUX)), p.add(El(&SINK)));
        p.link((file, SrcPad(0)), (demux, SinkPad(0))).expect("links");
        p.link((demux, SrcPad(0)), (sink, SinkPad(0))).expect("links");
        p.publish_index(demux, gops());
        let control = p.control();
        let want = SeekPlan::Seek { to_byte: 10_000, key: MS(10_000), to_time: MS(15_000) };
        let before = alloc_calls();
        assert_eq!(control.plan_from(MS(1_000), MS(15_000), SeekMode::Accurate), want);
        assert!(alloc_calls() > before, "a cold plan grows its lists");
        let before = alloc_calls();
        for _ in 0..4 {
            assert_eq!(control.plan_from(MS(1_000), MS(15_000), SeekMode::Accurate), want);
            control.seekable();
            control.can_skip();
        }
        assert_eq!(alloc_calls() - before, 0, "a warm plan reuses them");
    }

    #[test]
    fn an_edit_then_costs_one_box_and_answers_once() {
        let control = Pipeline::new().control();
        // Bounded, so a send allocates nothing.
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let before = alloc_calls();
        control.edit_then(|_: &mut Pipeline| 1, move |r| tx.send(r).unwrap_or(()));
        assert_eq!(alloc_calls() - before, 1, "the edit and its answer in one box");
        let refused = Err(profluens_core::command::CommandError::NotRunning);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [refused], "no run serves edits");
    }
}
