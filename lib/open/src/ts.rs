// App-side setup before any pipeline, the buffers of one open.
#![allow(clippy::disallowed_methods)]

use pf_mpegts::{Codec, Config, Demux, Encryption, Event, StreamInfo, UF_SCRAMBLED, Unwrapper};
use pf_ps::seek::Base;

use crate::{Desc, OpenError, SkipWhy, TrackDesc, TrackKind, es, sps};

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
        self.demux.discontinuity();
        self.demux.set_position(at);
        self.push(b);
    }

    /// The streams of the program described, and how long the file lasts, 0 when it does not
    /// tell.
    pub(crate) fn finish(&mut self, d: &mut Desc) -> Result<u64, OpenError> {
        let st = &mut self.st;
        self.demux.flush(&mut |e| st.event(e));
        st.describe(d)
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
            }
            Event::Pes(u) => {
                let Some(at) = self.streams.iter().position(|s| s.pid == u.pid) else { return };
                let codec = self.streams[at].codec;
                if let (Some(pts), true) = (u.pts, family_of(codec).1 != TrackKind::Other) {
                    self.last[self.lasts % LAST] = pts;
                    self.lasts += 1;
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
            _ => {}
        }
    }

    fn describe(&self, d: &mut Desc) -> Result<u64, OpenError> {
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
        let base = Base { origin: least, slack: ORIGIN_SLACK };
        let last = self.last[..self.lasts.min(LAST)].iter().map(|&p| base.ticks(p)).max();
        Ok(last.map_or(0, |t| (i128::from(t.max(0)) * 100_000 / 9) as u64))
    }
}
