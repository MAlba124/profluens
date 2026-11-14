//! The logical bitstreams an Ogg file opens with, read from its first bytes (RFC 3533 section 4).

use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

use crate::comment::parse_comment_body;
use crate::ident::{Mapping, parse_opus_head, parse_speex_head, parse_vorbis_ident};
use crate::reader::OggReader;

/// One logical bitstream of the first link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadStream {
    pub serial: u32,
    pub mapping: Mapping,
    /// The channels its identification header states, 0 for none.
    pub channels: u16,
    /// Its `LANGUAGE` comment, empty for none.
    pub language: Box<str>,
    /// Its `TITLE` comment, empty for none.
    pub title: Box<str>,
    commented: bool,
    packets: u32,
}

struct Wanted<'a> {
    language: &'a mut Box<str>,
    title: &'a mut Box<str>,
}

impl TagSink for Wanted<'_> {
    // Cold, once per stream at preroll.
    #[allow(clippy::disallowed_methods)]
    fn text(&mut self, key: &str, value: &str) {
        let slot = match key {
            "LANGUAGE" => &mut *self.language,
            "TITLE" => &mut *self.title,
            _ => return,
        };
        if slot.is_empty() {
            *slot = value.into();
        }
    }

    fn picture(&mut self, _mime: &str, _data: &[u8]) {}
}

pub(crate) fn channels_of(mapping: Mapping, ident: &[u8]) -> u16 {
    match mapping {
        Mapping::Opus => parse_opus_head(ident).map_or(0, |h| u16::from(h.channels)),
        Mapping::Vorbis => parse_vorbis_ident(ident).map_or(0, |h| u16::from(h.channels)),
        Mapping::Speex => parse_speex_head(ident).map_or(0, |h| u16::from(h.channels)),
        Mapping::Flac => ident.get(17 + 12).map_or(0, |b| u16::from((b >> 1) & 7) + 1),
        Mapping::Theora | Mapping::Skeleton | Mapping::Unknown => 0,
    }
}

// Cold, once per open, at preroll.
#[allow(clippy::disallowed_methods)]
pub fn scan_head(head: &[u8]) -> Vec<HeadStream> {
    let mut reader = OggReader::new();
    reader.push(head);
    let mut found: Vec<HeadStream> = Vec::new();
    let mut grouped = true;
    let scratch = Arena::with_cap(64 << 10);
    while let Some(packet) = reader.next_packet() {
        if packet.bos {
            if !grouped {
                break;
            }
            let mapping = Mapping::identify(&packet.data);
            found.push(HeadStream {
                serial: packet.serial,
                mapping,
                channels: channels_of(mapping, &packet.data),
                language: "".into(),
                title: "".into(),
                commented: false,
                packets: 1,
            });
            continue;
        }
        grouped = false;
        let Some(s) = found.iter_mut().find(|s| s.serial == packet.serial) else {
            continue;
        };
        s.packets += 1;
        if s.commented || s.packets != 2 {
            continue;
        }
        if let Some(body) = s.mapping.comment_body(&packet.data) {
            s.commented = true;
            parse_comment_body(
                body,
                &scratch,
                &mut Wanted { language: &mut s.language, title: &mut s.title },
            );
        }
    }
    found
}

#[cfg(test)]
// Tests build page fixtures, the scan allocates once per open.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::writer::OggWriter;

    fn tags(pairs: &[&str]) -> Vec<u8> {
        let mut b = b"OpusTags".to_vec();
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(b"pf0");
        b.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
        for p in pairs {
            b.extend_from_slice(&(p.len() as u32).to_le_bytes());
            b.extend_from_slice(p.as_bytes());
        }
        b
    }

    fn head(channels: u8) -> Vec<u8> {
        let mut h = b"OpusHead\x01".to_vec();
        h.push(channels);
        h.extend_from_slice(&[0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        h
    }

    fn grouped_then_chained() -> Vec<u8> {
        let mut out = Vec::new();
        let (mut a, mut b) = (OggWriter::new(7), OggWriter::new(9));
        a.write_packet(&mut out, &head(2), 0).unwrap();
        a.flush(&mut out);
        b.write_packet(&mut out, &head(1), 0).unwrap();
        b.flush(&mut out);
        a.write_packet(&mut out, &tags(&["language=fra", "TITLE=Une"]), 0).unwrap();
        a.flush(&mut out);
        b.write_packet(&mut out, &tags(&["LANGUAGE=eng"]), 0).unwrap();
        b.flush(&mut out);
        a.write_packet(&mut out, &[0xF8, 1, 2], 960).unwrap();
        a.finish(&mut out);
        b.write_packet(&mut out, &[0xF8, 3, 4], 960).unwrap();
        b.finish(&mut out);
        let mut c = OggWriter::new(11);
        c.write_packet(&mut out, b"\x80theora\x03\x02", 0).unwrap();
        c.finish(&mut out);
        out
    }

    #[test]
    fn grouped_streams_are_every_bos_ahead_of_data_and_a_chained_link_ends_the_scan() {
        let found = scan_head(&grouped_then_chained());
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!((found[0].serial, found[0].mapping, found[0].channels), (7, Mapping::Opus, 2));
        assert_eq!((&*found[0].language, &*found[0].title), ("fra", "Une"));
        assert_eq!((found[1].serial, found[1].channels, &*found[1].language), (9, 1, "eng"));
    }

    #[test]
    fn a_head_cut_short_still_names_its_streams() {
        let all = grouped_then_chained();
        let found = scan_head(&all[..100]);
        assert_eq!(found.iter().map(|s| s.serial).collect::<Vec<_>>(), vec![7, 9]);
        assert!(found.iter().all(|s| s.language.is_empty()));
        assert!(scan_head(&all[..20]).is_empty());
        assert!(scan_head(b"not an ogg file at all").is_empty());
    }
}
