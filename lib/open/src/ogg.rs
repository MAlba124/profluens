use pf_ogg::{
    Mapping, OggReader, parse_comment_body, parse_opus_head, parse_speex_head, parse_vorbis_ident,
    stream_duration_ns,
};
use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

use crate::mp4::OPUS_PREROLL_NS;
use crate::{Desc, OpenError, SkipWhy, Span, TrackDesc, TrackKind};

/// Opus decodes at 48 kHz whatever the input was (RFC 7845 section 5.1).
const OPUS_RATE: u32 = 48_000;

/// A logical stream of the first link as its header packets state it.
#[derive(Debug)]
struct Stream {
    serial: u32,
    mapping: Mapping,
    /// The identification packet.
    ident: Span,
    /// Header packets the mapping opens with, the identification packet among them.
    headers: u32,
    packets: u32,
    language: Span,
    title: Span,
    /// The Vorbis comment body, kept for the tags.
    comment: Option<Span>,
}

/// The LANGUAGE and TITLE comments into the arena.
struct Names<'a> {
    d: &'a mut Desc,
    language: &'a mut Span,
    title: &'a mut Span,
}

impl TagSink for Names<'_> {
    fn text(&mut self, key: &str, value: &str) {
        let slot = match key {
            "LANGUAGE" => &mut *self.language,
            "TITLE" => &mut *self.title,
            _ => return,
        };
        if slot.is_empty() {
            *slot = self.d.put(value.as_bytes());
        }
    }

    fn picture(&mut self, _mime: &str, _data: &[u8]) {}
}

/// The kind, the channels, the rate and the timing of a stream by its identification packet,
/// false for one that does not parse.
fn form(mapping: Mapping, ident: &[u8], t: &mut TrackDesc) -> bool {
    match mapping {
        Mapping::Opus => {
            let Some(h) = parse_opus_head(ident) else { return false };
            (t.channels, t.rate) = (u16::from(h.channels), OPUS_RATE);
            t.delay_ns = u64::from(h.pre_skip) * 1_000_000_000 / u64::from(OPUS_RATE);
            t.seek_preroll_ns = OPUS_PREROLL_NS;
        }
        Mapping::Vorbis => {
            let Some(h) = parse_vorbis_ident(ident) else { return false };
            (t.channels, t.rate) = (u16::from(h.channels), h.sample_rate);
        }
        Mapping::Speex => {
            let Some(h) = parse_speex_head(ident) else { return false };
            (t.channels, t.rate) = (u16::from(h.channels), h.sample_rate);
        }
        Mapping::Flac => {
            // STREAMINFO behind the mapping header, fLaC and the block header
            let Some(&[a, b, c, e]) = ident.get(27..31) else { return false };
            t.rate = u32::from(a) << 12 | u32::from(b) << 4 | u32::from(c) >> 4;
            t.channels = u16::from(c >> 1 & 7) + 1;
            t.depth = ((c & 1) << 4 | e >> 4) + 1;
        }
        Mapping::Theora => t.kind = TrackKind::Video,
        Mapping::Skeleton | Mapping::Unknown => t.kind = TrackKind::Other,
    }
    true
}

/// The bos pages of a file and the comment packets behind them, read as the head grows. Every bos
/// page of the first link comes before its data pages (RFC 3533 section 4), a bos page after a
/// data packet starts a chained link and ends the scan.
#[derive(Default)]
pub(crate) struct Scan {
    reader: OggReader,
    streams: Vec<Stream>,
    grouped: bool,
    chained: bool,
    scratch: Arena,
}

impl Scan {
    pub fn new() -> Scan {
        Scan { grouped: true, ..Scan::default() }
    }

    /// Reads the packets `bytes` complete, the next bytes of the file, each comment body kept
    /// when `keep`.
    pub fn push(&mut self, bytes: &[u8], d: &mut Desc, keep: bool) {
        if self.chained {
            return;
        }
        self.reader.push(bytes);
        while let Some(p) = self.reader.next_packet() {
            if p.bos {
                if !self.grouped {
                    self.chained = true;
                    self.reader.recycle(p.data);
                    return;
                }
                let empty = Span::default();
                let mapping = Mapping::identify(&p.data);
                self.streams.push(Stream {
                    serial: p.serial,
                    mapping,
                    ident: d.put(&p.data),
                    headers: mapping.header_packets(&p.data),
                    packets: 1,
                    language: empty,
                    title: empty,
                    comment: None,
                });
                self.reader.recycle(p.data);
                continue;
            }
            if let Some(s) = self.streams.iter_mut().find(|s| s.serial == p.serial) {
                s.packets += 1;
                // a data packet, a bos page after it starts a chained link
                self.grouped &= s.packets <= s.headers;
                if s.packets == 2
                    && let Some(body) = s.mapping.comment_body(&p.data)
                {
                    let mut names =
                        Names { d: &mut *d, language: &mut s.language, title: &mut s.title };
                    parse_comment_body(body, &self.scratch, &mut names);
                    if keep {
                        s.comment = Some(d.put(body));
                    }
                }
            }
            self.reader.recycle(p.data);
        }
    }

    /// The first stream's comment packet is not read yet and may follow.
    pub fn wants_comment(&self) -> bool {
        let commented = |m| {
            matches!(
                m,
                Mapping::Opus | Mapping::Vorbis | Mapping::Theora | Mapping::Speex | Mapping::Flac
            )
        };
        let first = self.streams.first();
        !self.chained && first.is_some_and(|s| s.packets < 2 && commented(s.mapping))
    }

    /// The tracks, the longest stream's duration by the last granule in `tail`, the file's last
    /// bytes, and the comment of the first stream that has one.
    pub fn finish(
        self,
        tail: &[u8],
        len: u64,
        d: &mut Desc,
    ) -> Result<(u64, Option<Span>), OpenError> {
        let mut duration = 0;
        for (i, s) in self.streams.iter().enumerate() {
            let id = i as u32 + 1;
            let ident = d.arena.get(s.ident.range()).unwrap_or_default();
            let ns = stream_duration_ns(ident, tail, s.serial).unwrap_or(0);
            let mut t = TrackDesc::new(id, TrackKind::Audio, s.mapping.family());
            if !form(s.mapping, ident, &mut t) {
                d.skip(u64::from(id), s.mapping.family(), SkipWhy::BadHead);
                continue;
            }
            (t.stream, t.head, t.language, t.title) = (s.serial, s.ident, s.language, s.title);
            t.duration_ns = ns;
            duration = duration.max(ns);
            d.push(t);
        }
        if d.tracks.is_empty() {
            return Err(OpenError::NoStreams);
        }
        if let [t] = &mut d.tracks[..]
            && t.duration_ns > 0
        {
            let bps = u128::from(len) * 8 * 1_000_000_000 / u128::from(t.duration_ns);
            t.bitrate = bps.min(u128::from(u64::MAX)) as u64;
        }
        let comment = self.streams.iter().find_map(|s| s.comment);
        Ok((duration, comment))
    }
}
