//! pf-hls, the parts of an HLS packager that know nothing of where segments go. A rendition's
//! segment grid fixed before any media, where a stream of frames splits into it, and the m3u8
//! texts (RFC 8216bis) that list it. pf-mp4's `cmaf` writes the segments, pf-subparse's
//! `vttwrite` the WebVTT ones, and the caller keeps them and serves them.

#![deny(unsafe_code)]

pub mod cut;
pub mod playlist;
pub mod timeline;

pub use cut::{Cutter, Kind, Offer};
pub use timeline::Timeline;
