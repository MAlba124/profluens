//! profluens-elements, the built-in pure-Rust elements.

pub mod flow;
pub mod io;
pub mod testing;

use pf_registry::Registry;
use profluens_core::element::Element;

/// Register the built-in elements for name-based construction.
pub fn register(registry: &mut Registry) {
    registry.register(io::FileSrc::new("").desc());
    registry.register(io::FileSink::new("").desc());
    registry.register(io::GrowingFileSrc::new("").0.desc());
    registry.register(flow::Queue::new().desc());
    registry.register(testing::TestSrc::new(0).desc());
    registry.register(testing::TestSink::new().0.desc());
}
