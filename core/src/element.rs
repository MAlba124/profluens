use std::sync::Arc;

use crate::batch::Inputs;
use crate::clock::Clock;
use crate::couple::Coupling;
use crate::ctx::Ctx;
use crate::error::Error;
use crate::event::Event;
use crate::format::{Constraint, OfferDesc, ValueDesc};
use crate::time::Timestamp;

/// The object-safe element contract, its static shape lives in [`ElementDesc`].
pub trait Element: Send {
    fn desc(&self) -> &'static ElementDesc;
    /// Adds dynamic pads and supplies probed offers, once before linking.
    fn preroll(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        Ok(())
    }
    /// Structural property values the constructor chose, read once at `add`. A later set wins.
    fn presets(&self) -> &[(&'static str, ValueDesc)] {
        &[]
    }
    /// A clock this element can master the pipeline on, the most downstream one wins.
    fn provide_clock(&mut self) -> Option<Arc<dyn Clock>> {
        None
    }
    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, inputs: Inputs<'_>) -> Result<Flow, Error>;
    /// In-band events at batch boundaries.
    fn event(&mut self, _ctx: &mut Ctx, _event: &Event) -> Result<(), Error> {
        Ok(())
    }
    fn stop(&mut self, _ctx: &mut Ctx) {}
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    Ok,
    NeedMore,
    Eos,
}

/// Passive runs inline as a pure transform, Active blocks and needs its own thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchedHint {
    Passive,
    Active,
}

/// How the scheduler waits on an element's inputs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputPolicy {
    None,
    Single,
    Any,
    All { by: AlignBy },
    AllDeadline { by: AlignBy, timeout: Timestamp },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlignBy {
    Dts,
    RunningTime,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LatencyDesc {
    pub min: Timestamp,
    pub max: Timestamp,
    pub is_live: bool,
    pub jitter: Timestamp,
}

impl LatencyDesc {
    /// Adds no latency and is not live.
    pub const NONE: Self = Self {
        min: Timestamp::ZERO,
        max: Timestamp::ZERO,
        is_live: false,
        jitter: Timestamp::ZERO,
    };
}

/// One pad, its side given by the [`ElementDesc`] list it is in.
pub struct PadDesc {
    pub name: &'static str,
    /// Supported formats in preference order, empty matches nothing.
    pub offers: &'static [OfferDesc],
}

pub struct PropDesc {
    pub name: &'static str,
    pub allowed: Constraint<'static>,
    /// Settable at batch boundaries while `Playing`.
    pub live: bool,
}

pub struct ElementDesc {
    pub name: &'static str,
    pub sinks: &'static [PadDesc],
    pub srcs: &'static [PadDesc],
    pub props: &'static [PropDesc],
    pub sched: SchedHint,
    pub inputs: InputPolicy,
    pub latency: LatencyDesc,
    /// Submits reactor IO, so a run fails with `Error::NoReactor` when the pipeline has no
    /// factory.
    pub io: bool,
    /// Default constructor for the string and parse path.
    pub make_default: Option<fn() -> Box<dyn Element>>,
    /// What the src pads take from the sink pads and the properties. Wildcard pads couple without
    /// a rule.
    pub couplings: &'static [Coupling],
}

/// A construction-time subgraph expander, not built yet.
pub struct Template {
    _priv: (),
}
