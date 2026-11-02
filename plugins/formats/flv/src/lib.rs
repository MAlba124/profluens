//! Hand-written FLV demuxer, Adobe FLV 10.1 Annex E with the Enhanced RTMP video header.
//! [`tag`] reads tags sans-IO and resyncs over damage, [`amf`] the `onMetaData` header.

#![deny(unsafe_code)]

pub mod amf;
pub mod codec;
pub mod element;
pub mod index;
pub mod tag;

pub use codec::EMITS;
pub use element::{FlvDemux, FlvError, HEAD};

use pf_registry::{Elements, Registry};

pub static ELEMENTS: Elements = &[&element::DESC];

pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
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
