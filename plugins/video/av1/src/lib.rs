//! AV1 decoder plugin over VideoLAN's dav1d.

pub mod av1dec;
pub mod dav1d;
pub mod tu;

pub use av1dec::{Av1Dec, Av1DecError};

use pf_registry::{Elements, Registry};

pub static ELEMENTS: Elements = &[&av1dec::DESC];

/// Register this crate's elements for name-based construction.
pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
}
