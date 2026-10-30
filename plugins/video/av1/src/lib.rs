//! AV1 decoder plugin over VideoLAN's dav1d.

pub mod av1dec;
pub mod dav1d;

pub use av1dec::{Av1Dec, Av1DecError};

use pf_registry::Registry;
use profluens_core::element::Element;

/// Register this crate's elements for name-based construction.
pub fn register(registry: &mut Registry) {
    registry.register(Av1Dec::new().desc());
}
