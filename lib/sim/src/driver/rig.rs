//! A scenario turned into a pipeline through core's public API, each link held against the
//! scenario's [`Plan`].

use core::fmt;
use std::sync::{Arc, Mutex};

use profluens_core::clock::MockClock;
use profluens_core::error::{Error, LinkError, NoCommonFormat};
use profluens_core::format::Value;
use profluens_core::id::{ElementId, LinkId, SinkPad, SrcPad};
use profluens_core::level::Limit;
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::{Index, IndexEntry, Pipeline};
use profluens_core::ring::{Consumer, Leaky, spsc};
use profluens_core::time::Timestamp;

use super::decoder::FakeDecoder;
use super::feed;
use super::gop::{GopDecl, Pictures};
use super::offers::{FAMILIES, FIELD, Fixed, MAX_LINKS, Plan};
use super::pads::{IN, OUT};
use super::reactor::{Serve, World};
use super::retimer::FakeRetimer;
use super::scenario::{MAX_SINKS, Scenario, Topology};
use super::sink::{FakeSink, Judged, SinkArgs};
use super::source::FakeSource;
use super::transform::{FakeTransform, Shape};
use crate::action::Anchor;
use crate::media::{MediaSpec, ScheduleError, StreamKind};
use crate::record::Row;
use crate::time::{Ns, SEC};

/// Not zero, so running time and the clock differ and code that mixes them up shows.
pub const START: Ns = SEC;

const SLOT: usize = 64;
const SWING_SLOT: usize = 256;

fn slot_of(sc: &Scenario) -> usize {
    if sc.source.swing > 1 { SWING_SLOT } else { SLOT }
}
const TAP_ROWS: usize = 256;
/// Its own pool, so a copy never waits for a slot only its own output would free.
const COPY_SLOTS: u32 = 4;
const RETIMER_SLOTS: u32 = 8;
const RETIMER_TAP: usize = 1024;
const NO_FAMILY: u8 = u8::MAX;

#[derive(Clone, Debug)]
pub enum BuildError {
    Schedule(ScheduleError),
    Core(Error),
    Refused { link: u8, want: Fixed },
    Made { link: u8, got: Fixed },
    Format { link: u8, want: Fixed, got: Fixed },
    Unnamed { link: u8 },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Schedule(e) => write!(f, "the declared media cannot be served, {e}"),
            Self::Core(e) => write!(f, "the pipeline cannot be built, {e}"),
            Self::Refused { link, want } => {
                write!(f, "core refused link {link}, which the offers fix to {want}")
            }
            Self::Made { link, got } => {
                write!(f, "core fixed {got} on link {link}, whose pads offer nothing in common")
            }
            Self::Format { link, want, got } => {
                write!(f, "core fixed {got} on link {link}, where the offers give {want}")
            }
            Self::Unnamed { link } => {
                write!(f, "the refusal of link {link} does not name both pads and their offers")
            }
        }
    }
}

impl std::error::Error for BuildError {}

pub struct Rig {
    pub pipeline: Pipeline,
    pub clock: MockClock,
    pub taps: [Option<Consumer<Row>>; MAX_SINKS],
    pub sinks: [ElementId; MAX_SINKS],
    pub source: ElementId,
    pub sources: [ElementId; 2],
    pub decoded: Option<Consumer<Row>>,
}

#[derive(Clone, Copy)]
struct Join {
    src: (ElementId, SrcPad),
    sink: (ElementId, SinkPad),
    lines: (usize, usize),
    carried: bool,
}

fn sink_args(sc: &Scenario, sink: usize) -> SinkArgs {
    let mut args = SinkArgs::new(sink as u16, sc.sinks[sink].profile, sc.offers.sinks[sink]);
    let stream = sc.source.streams[sc.topology.stream_of(sink) as usize].kind;
    let judged =
        if matches!(stream, StreamKind::Video { .. }) { Judged::Picture } else { Judged::Span };
    args.point = sc.gop.on.then_some(judged);
    for (index, action) in sc.script.as_slice().iter().enumerate() {
        if let Anchor::InFlush { sink: at, count } = action.anchor
            && at as usize == sink
        {
            // A full table leaves the action unfired, which the run reports.
            args.meddle(count, index as u8, action.kind);
        }
    }
    args
}

fn add_sink(sc: &Scenario, rig: &mut Rig, sink: usize) -> ElementId {
    let (tx, rx) = spsc::<Row>(TAP_ROWS);
    let control = rig.pipeline.control();
    let id = rig.pipeline.add(FakeSink::new(sink_args(sc, sink), tx, control));
    let decl = &sc.sinks[sink];
    if decl.queue > 0 {
        rig.pipeline.set_queue_capacity(id, decl.queue as usize);
    }
    if decl.leaky {
        rig.pipeline.set_queue_leaky(id, Leaky::DropNewest);
    }
    if decl.limit > 0 {
        rig.pipeline.set_link_limit(id, Limit::time(Timestamp(decl.limit)));
    }
    rig.taps[sink] = Some(rx);
    rig.sinks[sink] = id;
    id
}

fn add_decoder(sc: &Scenario, rig: &mut Rig, shape: Shape) -> ElementId {
    let (tx, rx) = spsc::<Row>(TAP_ROWS);
    let inline = shape == Shape::Inline;
    let id = rig.pipeline.add(FakeDecoder::new(sc.pictures(), &sc.gop, inline, tx));
    if sc.transform_queue > 0 {
        rig.pipeline.set_queue_capacity(id, sc.transform_queue as usize);
    }
    rig.decoded = Some(rx);
    id
}

fn add_retimer(sc: &Scenario, rig: &mut Rig, shape: Shape) -> ElementId {
    let (tx, rx) = spsc::<Row>(RETIMER_TAP);
    let inline = shape == Shape::Inline;
    let id = rig.pipeline.add(FakeRetimer::new(sc.time.retimer, inline, tx));
    if sc.transform_queue > 0 {
        rig.pipeline.set_queue_capacity(id, sc.transform_queue as usize);
    }
    rig.pipeline.set_element_pool(id, slot_of(sc), RETIMER_SLOTS);
    rig.decoded = Some(rx);
    id
}

fn add_transform(sc: &Scenario, rig: &mut Rig, shape: Shape) -> ElementId {
    if sc.gop.on {
        return add_decoder(sc, rig, shape);
    }
    if sc.time.retimer.on {
        return add_retimer(sc, rig, shape);
    }
    let id = rig.pipeline.add(FakeTransform::new(sc.transform, shape));
    if sc.transform_queue > 0 {
        rig.pipeline.set_queue_capacity(id, sc.transform_queue as usize);
    }
    if shape == Shape::Tee {
        rig.pipeline.set_element_pool(id, slot_of(sc), COPY_SLOTS);
    }
    id
}

fn add_rest(sc: &Scenario, rig: &mut Rig, src: ElementId) -> [Option<Join>; MAX_LINKS] {
    let shape = match sc.topology {
        Topology::Inline => Shape::Inline,
        Topology::FanOut => Shape::Tee,
        _ => Shape::Active,
    };
    let of_src = |stream: usize| sc.offers.srcs[stream].offers().len();
    let of_sink = |sink: usize| sc.offers.sinks[sink].offers().len();
    let mut joins = [None; MAX_LINKS];
    let (mut from, mut carried, mut next) = (src.src(OUT), false, 0);
    if sc.topology.has_transform() {
        let transform = add_transform(sc, rig, shape);
        let sink = transform.sink(IN);
        joins[next] = Some(Join { src: from, sink, lines: (of_src(0), 1), carried });
        (from, carried, next) = (transform.src(OUT), true, next + 1);
    }
    let lines = (if carried { 1 } else { of_src(0) }, of_sink(0));
    let first = add_sink(sc, rig, 0);
    joins[next] = Some(Join { src: from, sink: first.sink(IN), lines, carried });
    if sc.topology.sinks() == 2 {
        let fan = sc.topology == Topology::FanOut;
        let branch = if fan { from.0 } else { src };
        let lines = (if fan { 1 } else { of_src(1) }, of_sink(1));
        let second = add_sink(sc, rig, 1);
        let src = match sc.time.two_sources {
            true => rig.sources[1].src(OUT),
            false => branch.src(SrcPad(1)),
        };
        joins[next + 1] = Some(Join { src, sink: second.sink(IN), lines, carried: fan });
    }
    joins
}

fn fixed_of(pipeline: &Pipeline, link: LinkId) -> Fixed {
    let Some(format) = pipeline.negotiated(link) else {
        return Fixed { family: NO_FAMILY, id: 0 };
    };
    let name = pipeline.family_name(format.family);
    let family = FAMILIES.iter().position(|known| Some(*known) == name);
    let id = match pipeline.field_id(FIELD).and_then(|field| format.get(field)) {
        Some(Value::Int(id)) => id as u8,
        _ => 0,
    };
    Fixed { family: family.map_or(NO_FAMILY, |n| n as u8), id }
}

fn names(join: &Join, ends: &NoCommonFormat) -> bool {
    let (src, sink) = (&ends.src, &ends.sink);
    (src.element, sink.element) == (join.src.0, join.sink.0)
        && (src.offers.len(), sink.offers.len()) == join.lines
        && (src.bound.is_some(), sink.bound.is_some()) == (join.carried, false)
}

fn link(
    pipeline: &mut Pipeline,
    join: &Join,
    index: usize,
    plan: &Plan,
) -> Result<bool, BuildError> {
    let link = index as u8;
    let want = plan.links.get(index).filter(|_| index < plan.made as usize).copied();
    let made = pipeline.link(join.src, join.sink).map(|id| fixed_of(pipeline, id));
    match (made, want) {
        (Ok(got), Some(want)) if got == want => Ok(true),
        (Ok(got), Some(want)) => Err(BuildError::Format { link, want, got }),
        (Ok(got), None) => Err(BuildError::Made { link, got }),
        (Err(Error::Link(LinkError::NoCommonFormat(ends))), None) => match names(join, &ends) {
            true => Ok(false),
            false => Err(BuildError::Unnamed { link }),
        },
        (Err(Error::Link(LinkError::NoCommonFormat(_))), Some(want)) => {
            Err(BuildError::Refused { link, want })
        }
        (Err(error), _) => Err(BuildError::Core(error)),
    }
}

pub fn build(sc: &Scenario, plan: &Plan, spec: Arc<MediaSpec>) -> Result<Option<Rig>, BuildError> {
    let clock = MockClock::starting_at(Timestamp(START));
    // No debug spec, tracing or wall clock from the environment, so a run depends only on its seed.
    let mut pipeline = Pipeline::with(DebugSpec::default(), false, Arc::new(clock.clone()));
    pipeline.set_pool(slot_of(sc), sc.pool_slots);
    pipeline.start_paused(sc.start_paused);
    let make =
        |spec| FakeSource::new(sc.source, sc.offers, spec, sc.seed).map_err(BuildError::Schedule);
    let source = make(Arc::clone(&spec))?.over_net(sc.net);
    let source = if sc.gop.on { source.indexed(sc.pictures(), sc.gop.mapped) } else { source };
    if sc.net.on {
        let mut world = World::new(sc.net.faults);
        world.breakage = sc.net.breakage;
        let addr = std::net::SocketAddr::from(([10, 0, 0, 1], feed::PORT));
        world.peer(feed::HOST, addr, Serve::Pattern { key: sc.net.key, len: u64::MAX / 2 });
        let world = Arc::new(Mutex::new(world));
        pipeline.set_reactor_factory(World::factory(world, Arc::new(clock.clone()), sc.seed));
    }
    let two = sc.time.two_sources && sc.topology == Topology::TwoStreams;
    let src = pipeline.add(if two { source.alone(0) } else { source });
    let other = if two { pipeline.add(make(spec)?.alone(1)) } else { src };
    let (taps, sinks, sources) = ([None, None], [src; MAX_SINKS], [src, other]);
    let mut rig = Rig { pipeline, clock, taps, sinks, source: src, sources, decoded: None };
    let joins = add_rest(sc, &mut rig, src);
    rig.pipeline.preroll().map_err(BuildError::Core)?;
    if two {
        for (n, el) in sources.into_iter().enumerate() {
            rig.pipeline.publish_index(el, index_of(sc, n));
        }
    }
    for (index, join) in joins.iter().flatten().enumerate() {
        if !link(&mut rig.pipeline, join, index, plan)? {
            return Ok(None);
        }
    }
    Ok(Some(rig))
}

// Once per run.
#[allow(clippy::disallowed_methods)]
fn index_of(sc: &Scenario, n: usize) -> Index {
    let grid = Pictures::of(&sc.source.streams[n], &GopDecl::OFF);
    let key = |k| {
        let (time, byte) = grid.key(k);
        IndexEntry::key(time, byte)
    };
    let entries =
        (0..grid.keys()).filter(|k| grid.key(*k).0 != crate::time::NONE).map(key).collect();
    let duration = Timestamp(sc.source.streams[n].duration);
    Index { entries, file_len: Some(grid.count()), duration, complete: true, ..Index::default() }
}
