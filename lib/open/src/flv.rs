use pf_flv::amf;
use pf_flv::codec::{self, Audio, Packet, Video};
use pf_flv::tag::{self, Reader, Tag};

use crate::{Desc, OpenError, SkipWhy, TrackDesc, TrackKind};

/// What the head's first tags and its `onMetaData` state, as the demuxer reads them at preroll.
#[derive(Default)]
struct Found {
    meta: Option<amf::Meta>,
    video: Option<Video>,
    /// The codec and the first body octet, its sound flags.
    audio: Option<(Audio, u8)>,
    video_record: Option<(usize, usize, u64)>,
    audio_record: Option<(usize, usize, u64)>,
    mp3: Option<(u32, u32)>,
}

impl Found {
    /// `tag` read, the configuration records kept by their place in the head.
    fn tag(&mut self, tag: &Tag<'_>, base: u64) {
        let at = tag.pos - base + (tag::TAG_HEADER as u64);
        match tag.header.kind {
            tag::SCRIPT if self.meta.is_none() => self.meta = amf::on_meta_data(tag.body).ok(),
            tag::VIDEO => {
                let Some(v) = codec::video_tag(tag.body) else { return };
                if v.packet == Packet::Skip || v.codec == Video::Unknown {
                    return;
                }
                let codec = *self.video.get_or_insert(v.codec);
                if v.packet == Packet::Config && codec == v.codec && self.video_record.is_none() {
                    self.video_record = Some((v.from, v.to, at));
                }
            }
            tag::AUDIO => {
                let Some(a) = codec::audio_tag(tag.body) else { return };
                let (codec, _) = *self.audio.get_or_insert((a.codec, tag.body[0]));
                let payload = tag.body.get(a.from..).unwrap_or_default();
                match a.packet {
                    Packet::Config if codec == Audio::Aac && self.audio_record.is_none() => {
                        self.audio_record = Some((a.from, tag.body.len(), at));
                    }
                    Packet::Frame if self.mp3.is_none() && codec.family() == Some("mp3") => {
                        self.mp3 = codec::mp3_form(payload);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// The record `r` of a tag body in `head`.
fn record(head: &[u8], r: Option<(usize, usize, u64)>) -> &[u8] {
    let Some((from, to, at)) = r else { return &[] };
    let at = at as usize;
    head.get(at + from..at + to).unwrap_or_default()
}

/// The video then the audio stream the head's tags and `onMetaData` name, and the duration that
/// states.
pub(crate) fn describe(head: &[u8], d: &mut Desc) -> Result<u64, OpenError> {
    if tag::file_header(head).is_none() {
        return Err(OpenError::NoStreams);
    }
    let mut found = Found::default();
    let mut reader = Reader::new();
    reader.push(head, &mut |t: Tag<'_>| found.tag(&t, 0));
    let meta = found.meta.unwrap_or_default();
    let pixels = |v: Option<f64>| v.filter(|v| (1.0..65536.0).contains(v)).map_or(0, |v| v as u32);
    let video = found.video.or_else(|| meta.video_codec.map(Video::from_meta));
    let audio = found.audio.map(|a| a.0).or_else(|| meta.audio_codec.map(Audio::from_meta));
    if let Some(v) = video.filter(|v| *v != Video::Unknown) {
        match v.family() {
            Some(family) => {
                let mut t = TrackDesc::new(1, TrackKind::Video, family);
                (t.width, t.height) = (pixels(meta.width), pixels(meta.height));
                let rec = record(head, found.video_record);
                if v.nal() {
                    // a broken record leaves the parameter sets in band, the units 4 octet long
                    t.nal_length = 4;
                    if let Ok((h, n)) = pf_mkv::nal_head_from_config(rec, v == Video::Hevc) {
                        (t.head, t.nal_length) = (d.put(&h), n as u8);
                    }
                }
                d.push(t);
            }
            None => d.skip(1, "bytes", SkipWhy::NoFamily),
        }
    }
    if let Some(a) = audio.filter(|a| *a != Audio::Reserved) {
        match a.family() {
            Some(family) => {
                let mut t = TrackDesc::new(2, TrackKind::Audio, family);
                let flags = found.audio.map_or(0, |a| a.1);
                // the SoundRate and SoundType bits, Annex E.4.2.1
                t.rate = [5_512, 11_025, 22_050, 44_100][usize::from(flags >> 2 & 3)];
                t.channels = 1 + u16::from(flags & 1);
                t.depth = if flags & 2 != 0 { 16 } else { 8 };
                let form = match a {
                    Audio::Aac => {
                        let rec = record(head, found.audio_record);
                        t.head = d.put(rec);
                        codec::aac_form(rec)
                    }
                    Audio::Mp3 | Audio::Mp3At8k => found.mp3,
                    _ => None,
                };
                if let Some((rate, channels)) = form {
                    (t.rate, t.channels) = (rate, channels.min(u32::from(u16::MAX)) as u16);
                }
                if a != Audio::Pcm && a != Audio::PcmLe {
                    t.depth = 0;
                }
                d.push(t);
            }
            None => d.skip(2, "bytes", SkipWhy::NoFamily),
        }
    }
    if d.tracks.is_empty() {
        return Err(OpenError::NoStreams);
    }
    let duration = meta.duration.filter(|d| d.is_finite() && *d > 0.0 && *d < 4.3e6);
    Ok(duration.map_or(0, |s| (s * 1e9) as u64))
}
