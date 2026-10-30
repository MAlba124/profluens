//! Hand-written FLV demuxer, Adobe FLV 10.1 Annex E with the Enhanced RTMP video header.
//! [`tag`] reads tags sans-IO and resyncs over damage, [`amf`] the `onMetaData` header.

#![deny(unsafe_code)]

pub mod amf;
pub mod codec;
pub mod element;
pub mod index;
pub mod tag;

pub use element::{FlvDemux, FlvError, HEAD};

use pf_registry::Registry;
use profluens_core::element::Element;

// Cold path, one-time registration.
#[allow(clippy::disallowed_methods)]
pub fn register(registry: &mut Registry) {
    registry.register(FlvDemux::new(Vec::new()).desc());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_exposes_the_demuxer() {
        let mut reg = Registry::new();
        register(&mut reg);
        assert_eq!(reg.names(), vec!["flvdemux"]);
        assert!(reg.get("flvdemux").unwrap().make_default.is_none(), "made from its head");
    }
}
