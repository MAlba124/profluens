//! The deterministic simulator over core's stepped driver. Only the edges are fake. The fake
//! elements and run loop need the `driver` feature.

pub mod gop;
pub mod map;
pub mod model;
pub mod net;
pub mod offers;
pub mod oracle;
pub mod retime;
pub mod scenario;
pub mod trace;
pub mod verdict;
pub mod wire;

#[cfg(feature = "driver")]
pub mod decoder;
#[cfg(feature = "driver")]
pub mod farm;
#[cfg(feature = "driver")]
pub mod feed;
#[cfg(feature = "driver")]
pub mod pads;
#[cfg(feature = "driver")]
pub mod reactor;
#[cfg(feature = "driver")]
pub mod retimer;
#[cfg(feature = "driver")]
pub mod rig;
#[cfg(feature = "driver")]
pub mod run;
#[cfg(feature = "driver")]
pub mod sink;
#[cfg(feature = "driver")]
pub mod source;
#[cfg(feature = "driver")]
pub mod topo;
#[cfg(feature = "driver")]
pub mod transform;
