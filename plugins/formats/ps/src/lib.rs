//! MPEG program stream demuxer, H.222.0 2.5 and ISO/IEC 11172-1, so `.mpg`, `.mpeg`, DVD `.vob`
//! and the MPEG-1 system streams of VCD. The pack layer and PES headers are sans-IO in [`demux`],
//! the element is [`PsDemux`]. Not covered are AAC (a map naming it is refused), scrambled PES,
//! the DVD navigation in private stream 2 and the extended stream ids of 0xFD.

#![forbid(unsafe_code)]

pub mod demux;
mod element;
pub mod es;
pub mod lpcm;
pub mod pack;
pub mod seek;
pub mod streams;

pub use demux::{Anomaly, AnomalyKind, Demux, Event, Stats, Unit, key_of};
pub use element::{Head, PsDemux, PsError, Seen, TAIL_LEN, last_ticks, scan_head};
pub use streams::Codec;

use pf_registry::Registry;
use profluens_core::element::Element;

#[allow(clippy::disallowed_methods)] // one-time registry setup
pub fn register(registry: &mut Registry) {
    registry.register(PsDemux::new(Vec::new()).desc());
}
