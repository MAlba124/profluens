// App-side setup before any pipeline, the buffers of one open.
#![allow(clippy::disallowed_methods)]

use pf_bare::sync::{self, SyncKind};
use pf_mkv::config::{NalCodec, nals};

use crate::TrackDesc;

/// Payload bytes kept of a stream until its head is found, the parameter sets and SEI ahead of
/// a key picture fit.
pub(crate) const SNIFF: usize = 64 << 10;

/// The codec head a track of `family` states in `payload`, the first bytes of its stream, None
/// while they hold none. Annex B parameter sets for H.264 and H.265, the sequence header and its
/// extensions for MPEG-1 and MPEG-2 video, the VOL for MPEG-4 Part 2, the AudioSpecificConfig
/// for ADTS AAC, empty for audio whose frames state themselves.
pub(crate) fn head(family: &str, payload: &[u8]) -> Option<Vec<u8>> {
    match family {
        "h264/annexb" => sets(NalCodec::H264, payload),
        "h265/annexb" => sets(NalCodec::H265, payload),
        "mpeg1" | "mpeg2" => sequence(payload).map(<[u8]>::to_vec),
        "mpeg4/asp" => pf_avi::vol_prefix(payload).map(<[u8]>::to_vec),
        "aac" => adts_at(payload).map(|(_, a)| a.config().to_vec()),
        _ => audio(family, payload).map(|_| Vec::new()),
    }
}

/// The VPS, SPS and PPS units of `payload` in Annex B, once an SPS and a PPS are among them.
fn sets(codec: NalCodec, payload: &[u8]) -> Option<Vec<u8>> {
    let (mut out, mut sps, mut pps) = (Vec::new(), false, false);
    for (from, to) in nals(payload) {
        let nal = &payload[from..to];
        let kind = match codec {
            NalCodec::H264 => nal[0] & 0x1F,
            NalCodec::H265 => (nal[0] >> 1) & 0x3F,
        };
        let (is_sps, is_pps, keep) = match codec {
            NalCodec::H264 => (kind == 7, kind == 8, kind == 7 || kind == 8),
            NalCodec::H265 => (kind == 33, kind == 34, (32..=34).contains(&kind)),
        };
        if keep {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        (sps, pps) = (sps || is_sps, pps || is_pps);
    }
    (sps && pps).then_some(out)
}

/// The sequence header of MPEG-1 or MPEG-2 video and the extensions right behind it, ISO/IEC
/// 13818-2 6.2.2.
fn sequence(payload: &[u8]) -> Option<&[u8]> {
    let code = |from: usize| {
        payload.get(from..)?.windows(3).position(|w| w == [0, 0, 1]).map(|at| from + at)
    };
    let mut at = 0;
    let start = loop {
        at = code(at)?;
        if *payload.get(at + 3)? == 0xB3 {
            break at;
        }
        at += 3;
    };
    // the header and its quantiser matrices, then any extension start codes
    let mut end = start + 12;
    payload.get(start..end)?;
    loop {
        match code(end) {
            Some(n) if payload.get(n + 3) == Some(&0xB5) => end = n + 4,
            Some(n) => return Some(&payload[start..n]),
            None => return None,
        }
    }
}

/// Frame rates by frame_rate_code, Table 6-4.
const FRAME_RATES: [(u64, u64); 9] = [
    (0, 1),
    (24_000, 1001),
    (24, 1),
    (25, 1),
    (30_000, 1001),
    (30, 1),
    (50, 1),
    (60_000, 1001),
    (60, 1),
];

/// The size, frame duration and pixel aspect a sequence header and its extension state.
pub(crate) fn describe_sequence(head: &[u8], t: &mut TrackDesc) {
    let Some(h) = head.get(4..8) else { return };
    let (mut w, mut hgt) = (
        u32::from(h[0]) << 4 | u32::from(h[1] >> 4),
        (u32::from(h[1] & 0xF) << 8) | u32::from(h[2]),
    );
    let (aspect, rate) = (h[3] >> 4, usize::from(h[3] & 0xF));
    // the sequence extension's size bits, 6.2.2.3
    let ext = head.windows(5).position(|x| x[..4] == [0, 0, 1, 0xB5] && x[4] >> 4 == 1);
    if let Some(e) = ext.and_then(|e| head.get(e + 4..e + 7)) {
        w |= (u32::from(e[1] & 1) << 1 | u32::from(e[2] >> 7)) << 12;
        hgt |= (u32::from(e[2] >> 5) & 3) << 12;
    }
    (t.width, t.height) = (w, hgt);
    if let Some(&(n, d)) = FRAME_RATES.get(rate).filter(|r| r.0 > 0) {
        t.frame_ns = d * 1_000_000_000 / n;
    }
    // MPEG-2 states the display aspect, 6.3.3
    let dar = match (ext.is_some(), aspect) {
        (true, 2) => Some((4u64, 3u64)),
        (true, 3) => Some((16, 9)),
        (true, 4) => Some((221, 100)),
        _ => None,
    };
    if let Some((x, y)) = dar.filter(|_| w > 0 && hgt > 0) {
        t.par = crate::ratio(x * u64::from(hgt), y * u64::from(w));
    }
}

/// Bits of a header MSB first, 0 past its end.
struct Bits<'a> {
    b: &'a [u8],
    p: usize,
}

impl Bits<'_> {
    fn take(&mut self, n: usize) -> u32 {
        let (b, p) = (self.b, self.p);
        self.p += n;
        (p..p + n)
            .fold(0, |v, i| v << 1 | u32::from(b.get(i / 8).map_or(0, |x| x >> (7 - i % 8) & 1)))
    }
}

/// Pixel aspects by aspect_ratio_info, ISO/IEC 14496-2 Table 6-12.
const VOL_PAR: [(u32, u32); 6] = [(1, 1), (1, 1), (12, 11), (10, 11), (16, 11), (40, 33)];

/// The size and frame duration of an MPEG-4 Part 2 VOL with a fixed VOP rate, ISO/IEC 14496-2
/// 6.2.3, a rectangular shape stating its size.
pub(crate) fn describe_vol(head: &[u8], t: &mut TrackDesc) {
    let Some(at) = head.windows(4).position(|w| w[..3] == [0, 0, 1] && w[3] & 0xF0 == 0x20) else {
        return;
    };
    let mut r = Bits { b: &head[at + 4..], p: 0 };
    r.take(9);
    let verid = match r.take(1) {
        1 => r.take(7) >> 3,
        _ => 1,
    };
    let par = match r.take(4) {
        15 => (r.take(8), r.take(8)),
        a => VOL_PAR.get(a as usize).copied().unwrap_or((1, 1)),
    };
    // vol_control_parameters, then the VBV's 79 bits
    if r.take(1) == 1 {
        r.take(3);
        if r.take(1) == 1 {
            r.take(79);
        }
    }
    let shape = r.take(2);
    if shape == 3 && verid != 1 {
        r.take(4);
    }
    r.take(1);
    let res = r.take(16);
    r.take(1);
    let step = match r.take(1) {
        1 => r.take((32 - res.saturating_sub(1).leading_zeros()).max(1) as usize),
        _ => 0,
    };
    if shape != 0 || r.p + 29 > r.b.len() * 8 {
        return;
    }
    r.take(1);
    let w = r.take(13);
    r.take(1);
    (t.width, t.height) = (w, r.take(13));
    if par.0 > 0 && par.1 > 0 {
        t.par = crate::ratio(u64::from(par.0), u64::from(par.1));
    }
    if res > 0 && step > 0 {
        t.frame_ns = u64::from(step) * 1_000_000_000 / u64::from(res);
    }
}

fn adts_at(payload: &[u8]) -> Option<(usize, pf_tsdemux::es::Adts)> {
    let at = payload.windows(2).position(|w| w[0] == 0xFF && w[1] & 0xF6 == 0xF0)?;
    pf_tsdemux::es::Adts::parse(&payload[at..]).filter(|a| a.channels > 0).map(|a| (at, a))
}

/// The rate and channels the first frame of `family` audio in `payload` states.
pub(crate) fn audio(family: &str, payload: &[u8]) -> Option<(u32, u16)> {
    let kind = match family {
        "aac" => return adts_at(payload).map(|(_, a)| (a.rate(), u16::from(a.channels))),
        "mp3" => {
            let h = (0..payload.len().saturating_sub(3)).find_map(|i| {
                let h = pf_mp3::frame::Header::parse(&payload[i..])?;
                h.frame_len().map(|_| h)
            })?;
            return Some((h.sample_rate_hz, u16::from(h.channel_count())));
        }
        "ac3" => SyncKind::Ac3,
        "eac3" => SyncKind::Eac3,
        "dts" => SyncKind::Dts,
        _ => return None,
    };
    let f = (0..payload.len().saturating_sub(1))
        .filter(|&i| sync::is_sync(kind, payload[i], payload[i + 1]))
        .find_map(|i| sync::frame(kind, &payload[i..]).filter(|f| !f.attached))?;
    Some((f.rate, u16::from(f.channels)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::TrackKind;

    const SEQ: [u8; 12] = [0, 0, 1, 0xB3, 0x2D, 0x02, 0x40, 0x33, 0xFF, 0xFF, 0xE0, 0x18];
    const EXT: [u8; 10] = [0, 0, 1, 0xB5, 0x14, 0x8A, 0x00, 0x01, 0x00, 0x00];

    #[test]
    fn an_mpeg2_sequence_states_size_rate_and_aspect() {
        let mut p = vec![0, 0, 1, 0xB8, 0, 8, 0, 0];
        p.extend_from_slice(&SEQ);
        p.extend_from_slice(&EXT);
        p.extend_from_slice(&[0, 0, 1, 0xB8, 0, 8]);
        let h = head("mpeg2", &p).unwrap();
        assert_eq!(h, [&SEQ[..], &EXT].concat(), "the header and its extension");
        let mut t = TrackDesc::new(1, TrackKind::Video, "mpeg2");
        describe_sequence(&h, &mut t);
        assert_eq!((t.width, t.height, t.frame_ns), (720, 576, 40_000_000));
        assert_eq!(t.par, (64, 45), "16:9 over 720 by 576");
        assert_eq!(head("mpeg2", &SEQ), None, "nothing seen behind it");
        assert_eq!(head("mpeg2", &[0, 0, 1, 0xB3, 1]), None, "cut");
    }

    #[test]
    fn parameter_sets_are_kept_without_the_slices() {
        let p = [
            &[0, 0, 0, 1, 0x09, 0xF0][..],
            &[0, 0, 0, 1, 0x67, 0x42, 0, 0x1E],
            &[0, 0, 1, 0x68, 0xCE, 0x3C, 0x80],
            &[0, 0, 1, 0x65, 0x88, 0x84],
        ]
        .concat();
        let h = head("h264/annexb", &p).unwrap();
        assert_eq!(h, [0, 0, 0, 1, 0x67, 0x42, 0, 0x1E, 0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80]);
        assert_eq!(head("h264/annexb", &p[..14]), None, "no PPS yet");
        assert_eq!(head("h265/annexb", &p), None);
    }

    #[test]
    fn audio_frames_state_rate_and_channels_past_leading_bytes() {
        let mut p = vec![1, 2, 3];
        p.extend_from_slice(&pf_bare::fixture::ac3_frame(0, 192, false, 7));
        assert_eq!(audio("ac3", &p), Some((48_000, 6)));
        assert_eq!(head("ac3", &p), Some(Vec::new()));
        let adts = [0xFF, 0xF1, 0x4C, 0x80, 0x2E, 0x7F, 0xFC];
        assert_eq!(audio("aac", &adts), Some((48_000, 2)));
        assert_eq!(head("aac", &adts), Some(vec![0x11, 0x90]));
        let mp2 = [0, 0xFF, 0xFD, 0x84, 0x04, 0, 0, 0, 0];
        assert_eq!(audio("mp3", &mp2), Some((48_000, 2)));
        assert_eq!(audio("dts", &[0x7F, 0xFE]), None);
        assert_eq!(audio("vorbis", &[]), None);
    }
}
