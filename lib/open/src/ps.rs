// App-side setup before any pipeline, the buffers of one open.
#![allow(clippy::disallowed_methods)]

use pf_ps::seek::Base;
use pf_ps::streams::{self, Codec};
use profluens_core::ctx::StreamKind;

use crate::{Desc, OpenError, SkipWhy, TrackDesc, TrackKind, es};

/// The window is grown to this, a DVD's audio and video streams start within it.
pub(crate) const HEAD: usize = 1 << 20;
/// Slack below the earliest stamp for one read past it, as `psdemux` takes it.
const ORIGIN_SLACK: i64 = 10 * 90_000;

/// The streams `head` holds as `psdemux` finds them at preroll, by its order, and how long the
/// file lasts by the last stamp in `tail`, 0 when no stamp tells.
pub(crate) fn describe(head: &[u8], tail: &[u8], d: &mut Desc) -> Result<u64, OpenError> {
    let h = pf_ps::scan_head(head);
    let mut seen: Vec<&pf_ps::Seen> = h.seen.iter().collect();
    seen.sort_by_key(|s| streams::rank(s.key));
    let mut first = [false; 4];
    for s in seen {
        let psm_type = h.psm[usize::from(s.key >> 8)];
        let Some(codec) = streams::codec_of(s.key, psm_type, &s.sniff, h.mpeg2) else {
            d.skip(u64::from(s.key), "bytes", SkipWhy::NoFamily);
            continue;
        };
        let (family, kind) = streams::family_of(codec);
        let kind = match kind {
            StreamKind::Video => TrackKind::Video,
            StreamKind::Audio => TrackKind::Audio,
            StreamKind::Text => TrackKind::Text,
            StreamKind::Other => TrackKind::Other,
        };
        let mut t = TrackDesc::new(u32::from(s.key), kind, family);
        t.default = !std::mem::replace(&mut first[kind as usize], true);
        match (codec, s.lpcm) {
            (Codec::Lpcm, Some(l)) => {
                (t.rate, t.channels, t.depth) = (l.rate, u16::from(l.channels), l.bits);
            }
            _ => {
                if let Some(head) = es::head(family, &s.sniff) {
                    match family {
                        "mpeg1" | "mpeg2" => es::describe_sequence(&head, &mut t),
                        "mpeg4/asp" => es::describe_vol(&head, &mut t),
                        _ => {}
                    }
                    t.head = d.put(&head);
                }
                if let Some((rate, channels)) = es::audio(family, &s.sniff) {
                    (t.rate, t.channels) = (rate, channels);
                }
            }
        }
        d.push(t);
    }
    // subpictures alone are a VobSub `.sub`, a subtitle file and no media
    if !d.tracks.iter().any(|t| matches!(t.kind, TrackKind::Video | TrackKind::Audio)) {
        return Err(OpenError::NoStreams);
    }
    let Some((_, least)) = h.opening else { return Ok(0) };
    let last = pf_ps::last_ticks(tail, Base { origin: least, slack: ORIGIN_SLACK });
    Ok(last.map_or(0, |t| (i128::from(t.max(0)) * 100_000 / 9) as u64))
}
