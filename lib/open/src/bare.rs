use pf_bare::BareHead;

use crate::{Desc, TrackDesc, TrackKind};

/// The one track of a bare FLAC, MP3 or ADTS file.
pub(crate) fn describe(h: &BareHead, d: &mut Desc) {
    let mut t = TrackDesc::new(1, TrackKind::Audio, h.family);
    t.head = d.put(&h.head);
    t.channels = u16::from(h.channels());
    t.rate = h.rate;
    t.depth = h.depth();
    t.bitrate = u64::from(h.bitrate());
    // the LAME encoder delay, the stamps are not trimmed by it
    t.delay_ns = h.ns(u64::from(h.gapless.0)).max(0) as u64;
    t.duration_ns = h.duration_ns.max(0) as u64;
    d.push(t);
}
