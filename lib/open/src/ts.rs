// App-side setup before any pipeline, the buffers of one open.
#![allow(clippy::disallowed_methods)]

use pf_mpegts::{Codec, Config, Demux, Encryption, Event, StreamInfo, UF_SCRAMBLED, Unwrapper};
use pf_ps::seek::{Line, Rate};

use crate::{Desc, OpenError, SkipWhy, TrackDesc, TrackKind, es, plus, sps};

/// The window is grown to this while the program map or a stream's head is not in it.
pub(crate) const HEAD_MAX: usize = 4 << 20;
/// The end of the file read for its last stamps, a few frames of a 40 Mbit/s stream.
pub(crate) const TAIL: u64 = 256 << 10;
/// Slack below the first decode time for a stamp read past it, as `tsdemux` takes it.
const ORIGIN_SLACK: i64 = 10 * 90_000;
/// The last presentation stamps kept, B-frames put the latest a few units back.
const LAST: usize = 64;

/// The family `tsdemux` sends a codec as, `bytes` for one no decoder takes.
fn family_of(codec: Codec) -> (&'static str, TrackKind) {
    match codec {
        Codec::Mpeg1Video => ("mpeg1", TrackKind::Video),
        Codec::Mpeg2Video => ("mpeg2", TrackKind::Video),
        Codec::Mpeg4Visual => ("mpeg4/asp", TrackKind::Video),
        Codec::H264 => ("h264/annexb", TrackKind::Video),
        Codec::Hevc => ("h265/annexb", TrackKind::Video),
        Codec::AacAdts => ("aac", TrackKind::Audio),
        Codec::Ac3 => ("ac3", TrackKind::Audio),
        Codec::Eac3 => ("eac3", TrackKind::Audio),
        Codec::MpegAudio => ("mp3", TrackKind::Audio),
        Codec::DvbSubtitle => ("subtitle/dvb", TrackKind::Text),
        _ => ("bytes", TrackKind::Other),
    }
}

/// A stream's first payload bytes, until its head is in them.
struct Seen {
    pid: u16,
    family: &'static str,
    sniff: Vec<u8>,
    settled: bool,
}

/// What the packets of a transport stream state as they are read, the first program map, each
/// stream's head and first stamp as `tsdemux` reads them at preroll, and the last stamps.
pub(crate) struct Scan {
    demux: Demux,
    st: State,
}

struct State {
    mapped: bool,
    streams: Vec<StreamInfo>,
    seen: Vec<Seen>,
    firsts: Vec<u16>,
    clock: Unwrapper,
    /// The first decode time and the earliest, in ticks.
    opening: Option<(i64, i64)>,
    /// MPEG-1 or MPEG-2 video by its sequence header, under either stream type.
    mpeg12: Vec<(u16, Codec)>,
    last: [u64; LAST],
    lasts: usize,
    tail: bool,
    /// The stream whose stamps tell the time bases, the first video else the first audio, and
    /// them in the head and in the tail.
    key: Option<u16>,
    line: Line,
    end: Line,
    /// The program clock against the bytes, and the file's length once the tail is read.
    rate: Rate,
    len: Option<u64>,
}

impl Scan {
    pub(crate) fn new() -> Result<Scan, OpenError> {
        let config = Config { limits: pf_tsdemux::limits(), select_all: true };
        let demux = Demux::new(config).map_err(|_| OpenError::Malformed { at: 0 })?;
        let st = State {
            mapped: false,
            streams: Vec::new(),
            seen: Vec::new(),
            firsts: Vec::new(),
            clock: Unwrapper::for_timestamps(),
            opening: None,
            mpeg12: Vec::new(),
            last: [0; LAST],
            lasts: 0,
            tail: false,
            key: None,
            line: Line::default(),
            end: Line::default(),
            rate: Rate::default(),
            len: None,
        };
        Ok(Scan { demux, st })
    }

    /// The next bytes of the file.
    pub(crate) fn push(&mut self, b: &[u8]) {
        let st = &mut self.st;
        self.demux.push(b, &mut |e| st.event(e));
    }

    /// Whether the map or a stream's head is still to come.
    pub(crate) fn wants(&self) -> bool {
        !self.st.mapped || self.st.seen.iter().any(|s| !s.settled)
    }

    /// The end of the file from `at`, for its last stamps only.
    pub(crate) fn tail(&mut self, at: u64, b: &[u8]) {
        self.st.tail = true;
        self.st.rate.restart();
        self.st.len = Some(at + b.len() as u64);
        self.demux.discontinuity();
        self.demux.set_position(at);
        self.push(b);
    }

    /// The streams of the program described, and how long the file lasts, 0 when it does not
    /// tell. With `hdr10plus` each video stream's first payloads are looked in for HDR10+.
    pub(crate) fn finish(&mut self, d: &mut Desc, hdr10plus: bool) -> Result<u64, OpenError> {
        let st = &mut self.st;
        self.demux.flush(&mut |e| st.event(e));
        st.describe(d, hdr10plus)
    }
}

impl State {
    fn event(&mut self, e: Event<'_>) {
        match e {
            Event::Pmt { streams, .. } if !self.mapped => {
                self.mapped = true;
                self.streams = streams.to_vec();
                for s in streams {
                    let (family, kind) = family_of(s.codec);
                    let wanted = matches!(kind, TrackKind::Video | TrackKind::Audio);
                    if wanted && s.encryption == Encryption::None {
                        let sniff = Vec::new();
                        self.seen.push(Seen { pid: s.pid, family, sniff, settled: false });
                    }
                }
                let of = |kind| streams.iter().find(|s| family_of(s.codec).1 == kind);
                self.key = of(TrackKind::Video).or_else(|| of(TrackKind::Audio)).map(|s| s.pid);
            }
            Event::Pes(u) => {
                let Some(at) = self.streams.iter().position(|s| s.pid == u.pid) else { return };
                let codec = self.streams[at].codec;
                if let (Some(pts), true) = (u.pts, family_of(codec).1 != TrackKind::Other) {
                    self.last[self.lasts % LAST] = pts;
                    self.lasts += 1;
                }
                if let (Some(pts), true) = (u.pts, self.key == Some(u.pid)) {
                    let line = if self.tail { &mut self.end } else { &mut self.line };
                    line.note(u.dts.unwrap_or(pts), pts);
                }
                if self.tail {
                    return;
                }
                if matches!(codec, Codec::Mpeg1Video | Codec::Mpeg2Video)
                    && !self.mpeg12.iter().any(|(pid, _)| *pid == u.pid)
                    && let Some(mpeg1) = pf_tsdemux::es::sequence_is_mpeg1(u.payload)
                {
                    let c = if mpeg1 { Codec::Mpeg1Video } else { Codec::Mpeg2Video };
                    self.mpeg12.push((u.pid, c));
                }
                // one unwrapper over each stream's first stamp, as tsdemux reads the head
                if let Some(raw) = u.dts.or(u.pts).filter(|_| !self.firsts.contains(&u.pid)) {
                    let ticks = self.clock.unwrap(raw).ticks;
                    self.firsts.push(u.pid);
                    let (first, least) = self.opening.unwrap_or((ticks, ticks));
                    self.opening = Some((first, least.min(ticks)));
                }
                let seen = self.seen.iter_mut().find(|s| s.pid == u.pid && !s.settled);
                if let Some(s) = seen.filter(|_| u.flags & UF_SCRAMBLED == 0) {
                    let take = es::SNIFF.saturating_sub(s.sniff.len()).min(u.payload.len());
                    s.sniff.extend_from_slice(&u.payload[..take]);
                    s.settled = es::head(s.family, &s.sniff).is_some() || take < u.payload.len();
                }
            }
            Event::Pcr { pcr, offset, discontinuity, .. } => {
                if discontinuity {
                    self.rate.restart();
                }
                self.rate.note(offset, (pcr / 300) as i64);
            }
            _ => {}
        }
    }

    fn describe(&self, d: &mut Desc, hdr10plus: bool) -> Result<u64, OpenError> {
        if !self.mapped {
            return Err(OpenError::NoStreams);
        }
        let mut first = [false; 4];
        for info in &self.streams {
            let pid = u64::from(info.pid);
            let told = self.mpeg12.iter().find(|(p, _)| *p == info.pid).map(|(_, c)| *c);
            let (family, kind) = family_of(told.unwrap_or(info.codec));
            if family == "bytes" || info.encryption != Encryption::None {
                d.skip(pid, family, SkipWhy::NoFamily);
                continue;
            }
            let mut t = TrackDesc::new(u32::from(info.pid), kind, family);
            t.language = d.put(pf_mpegts::language_str(info).unwrap_or("").as_bytes());
            t.default = !std::mem::replace(&mut first[kind as usize], true);
            let sniff = self.seen.iter().find(|s| s.pid == info.pid).map_or(&[][..], |s| &s.sniff);
            if let Some(head) = es::head(family, sniff) {
                match family {
                    "mpeg1" | "mpeg2" => es::describe_sequence(&head, &mut t),
                    "mpeg4/asp" => es::describe_vol(&head, &mut t),
                    _ => {}
                }
                t.head = d.put(&head);
            }
            if let Some(codec) = sps::nal_codec(family) {
                t.colour.hdr = sps::sei_hdr(codec, sniff);
                if hdr10plus {
                    t.hdr10plus = plus::stream(family, 0, sniff);
                }
            }
            if let Some((rate, channels)) = es::audio(family, sniff) {
                (t.rate, t.channels) = (rate, channels);
            }
            d.push(t);
        }
        if d.tracks.is_empty() {
            return Err(OpenError::NoStreams);
        }
        let Some((_, least)) = self.opening else { return Ok(0) };
        // summed across the clock breaks head and tail show, unknown across one between them
        // when far longer than the program clock's rate says
        let lasts = self.len.and_then(|len| self.rate.lasts(len));
        let mut end = self.end.clone();
        if self.tail && end.runs().is_empty() {
            // no unit of the key stream in the tail, the others' last stamps in their order
            let n = self.lasts.min(LAST);
            (self.lasts - n..self.lasts)
                .for_each(|i| end.note(self.last[i % LAST], self.last[i % LAST]));
        }
        let Some(ticks) = pf_ps::seek::extent(&self.line, &end, least, lasts, true) else {
            return Ok(0);
        };
        // the other streams' last stamps, a little past the key stream's
        let key_last = end.runs().last().or(self.line.runs().last()).map_or(0, |r| r.last);
        let floor = key_last - ORIGIN_SLACK;
        let ahead = |p: u64| {
            let d = pf_mpegts::congruent(floor, p) - key_last;
            if (0..=ORIGIN_SLACK).contains(&d) { d } else { 0 }
        };
        let over = self.last[..self.lasts.min(LAST)].iter().map(|&p| ahead(p)).max().unwrap_or(0);
        Ok((i128::from((ticks + over).max(0)) * 100_000 / 9) as u64)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// `ts` with every PCR, PTS and DTS `by` ticks later, modulo 2^33.
    fn shifted(ts: &[u8], by: u64) -> Vec<u8> {
        const M: u64 = 1 << 33;
        let stamp = |b: &mut [u8]| {
            let old = u64::from(b[0] >> 1 & 7) << 30
                | u64::from(b[1]) << 22
                | u64::from(b[2] >> 1) << 15
                | u64::from(b[3]) << 7
                | u64::from(b[4] >> 1);
            let v = (old + by) % M;
            b[0] = (b[0] & 0xF0) | ((v >> 29) as u8 & 0x0E) | 1;
            b[1] = (v >> 22) as u8;
            b[2] = ((v >> 14) as u8 & 0xFE) | 1;
            b[3] = (v >> 7) as u8;
            b[4] = ((v << 1) as u8) | 1;
        };
        let mut out = ts.to_vec();
        for p in out.as_chunks_mut::<188>().0 {
            let field = (p[3] >> 4) & 3;
            let start = p[1] & 0x40 != 0;
            let mut payload = 4;
            if field & 2 != 0 {
                if p[4] >= 7 && p[5] & 0x10 != 0 {
                    let base = u64::from(p[6]) << 25
                        | u64::from(p[7]) << 17
                        | u64::from(p[8]) << 9
                        | u64::from(p[9]) << 1
                        | u64::from(p[10] >> 7);
                    let v = (base + by) % M;
                    p[6..10].copy_from_slice(&((v >> 1) as u32).to_be_bytes());
                    p[10] = (p[10] & 0x7F) | ((v as u8 & 1) << 7);
                }
                payload = 5 + usize::from(p[4]);
            }
            let pes = &mut p[payload.min(188)..];
            if field & 1 == 0 || !start || pes.len() < 19 || pes[..3] != [0, 0, 1] {
                continue;
            }
            let flags = pes[7] >> 6;
            if flags & 2 != 0 {
                stamp(&mut pes[9..14]);
            }
            if flags == 3 {
                stamp(&mut pes[14..19]);
            }
        }
        out
    }

    /// The duration of `head` then `tail` as the opener reads them.
    fn lasts(head: &[u8], tail: &[u8]) -> u64 {
        let mut scan = Scan::new().expect("scan");
        scan.push(head);
        scan.tail(head.len() as u64, tail);
        scan.finish(&mut Desc::default(), false).expect("described")
    }

    /// A file whose clock restarts or jumps between head and tail is of unknown length, not 16.5
    /// or 10 h. One on one clock lasts as before, one restarting in the tail the sum.
    #[test]
    fn a_clock_set_back_between_head_and_tail_leaves_the_duration_unknown() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../plugins/formats/ts/tests/fixtures");
        let data = std::fs::read(format!("{path}/h264_aac.ts")).expect("fixture");
        let half = data.len() / 2 / 188 * 188;
        let whole = lasts(&data[..half], &data[half..]);
        assert!((1_900_000_000..2_100_000_000).contains(&whole), "{whole}");
        let later = shifted(&data, 36_000 * 90_000);
        assert_eq!(lasts(&later, &data), 0, "10 h, then a clock from 1.4 s");
        assert_eq!(lasts(&data, &later), 0, "a clock 10 h ahead between head and tail");
        let tail = [data[half..].to_vec(), later.clone()].concat();
        let summed = lasts(&data[..half], &tail);
        assert!(summed.abs_diff(2 * whole) < 100_000_000, "{summed} for twice {whole}");
    }
}
