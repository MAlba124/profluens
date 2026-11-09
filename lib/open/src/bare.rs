use pf_bare::BareHead;

use crate::{Desc, TrackDesc, TrackKind};

/// The one track of a bare FLAC, MP3, ADTS, AC-3, E-AC-3 or DTS file, or of a WAV file's PCM.
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
    // a WAVE header states its speakers, `Desc::push` reads codec heads only
    if let (Some(t), l) = (d.tracks.last_mut(), h.layout())
        && l > 0
    {
        t.layout = l;
    }
}
