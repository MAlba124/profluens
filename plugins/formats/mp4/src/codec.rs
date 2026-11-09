//! Sample entry parsing (`stsd`, ISO/IEC 14496-12 8.5.2) and the four-CC to family map.
//! A malformed config record degrades to an empty head, never an error.

// Cold path, once per stream at preroll.
#![allow(clippy::disallowed_methods)]

use crate::boxes::{self, BoxError, BoxHeader};
use profluens_video::color::{
    ChromaSite, ContentLight, DoviConfig, Mastering, Matrix, Primaries, Range, StreamColor,
    TransferFn,
};

pub use pf_mkv::{Reframer, nal_head_from_config};

/// Uncompressed audio, `sample` as `audio/raw` names it and `frame` the bytes of one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pcm {
    pub sample: &'static str,
    pub frame: u32,
}

#[derive(Clone, Debug)]
pub struct SampleEntry {
    pub kind: boxes::FourCc,
    pub family: &'static str,
    pub width: u32,
    pub height: u32,
    /// The `pasp` pixel aspect ratio (12.1.4), 0 to 0 where the entry has none.
    pub par: (u32, u32),
    pub sample_rate: u32,
    pub channels: u32,
    pub codec_head: Vec<u8>,
    /// The raw codec configuration record, the Matroska CodecPrivate on remux (RFC 9559 12).
    /// `avcC`/`hvcC` body, or the esds DecoderSpecificInfo for AAC and MPEG Visual. Kept even if
    /// unparseable.
    pub config_record: Vec<u8>,
    pub reframer: Reframer,
    pub colour: StreamColor,
    pub pcm: Option<Pcm>,
    /// Bits a second the entry states, its `btrt` avgBitrate (8.5.2.2), else the esds
    /// DecoderConfigDescriptor's (ISO/IEC 14496-1 7.2.6.6). 0 where neither does.
    pub avg_bitrate: u32,
    /// Bits a sample of an audio entry, a PCM entry's depth, FLAC's or ALAC's from its config,
    /// else the AudioSampleEntry samplesize (12.2.3), 16 for most coded audio. 0 for video.
    pub bit_depth: u32,
}

pub fn family_for(kind: boxes::FourCc) -> &'static str {
    match &kind {
        b"avc1" | b"avc3" => "h264/annexb",
        b"hvc1" | b"hev1" => "h265/annexb",
        b"vp09" => "vp9",
        b"av01" => "av1",
        // Dolby Vision entries as FFmpeg `libavformat/isom.c` maps them. The base layer decodes
        // as the plain codec.
        b"dvh1" | b"dvhe" => "h265/annexb",
        b"dva1" | b"dvav" => "h264/annexb",
        b"dav1" => "av1",
        b"Opus" => "opus",
        // ETSI TS 102 366 Annex F.
        b"ac-3" => "ac3",
        b"ec-3" => "eac3",
        // Promoted to `aac` once the esds yields an AudioSpecificConfig.
        b"mp4a" => "bytes",
        // Promoted by the esds objectTypeIndication.
        b"mp4v" => "bytes",
        b"fLaC" => "flac",
        b".mp3" => "mp3",
        b"alac" => "alac",
        // QTFF uncompressed sound and ISO/IEC 23003-5, back to `bytes` where no sample format fits.
        b"twos" | b"sowt" | b"raw " | b"in24" | b"in32" | b"fl32" | b"fl64" | b"lpcm" | b"ipcm"
        | b"fpcm" => "audio/raw",
        // 3GPP TS 26.245 and ISO/IEC 14496-30.
        b"tx3g" => "subtitle/srt",
        b"wvtt" => "subtitle/vtt",
        b"stpp" => "subtitle/ttml",
        _ => "bytes",
    }
}

pub const MAX_CUES: usize = 16;

/// The cues of one text sample as ranges of `sample` into `out`, and how many, a `tx3g` cue's
/// text, a `wvtt` cue's `vttc` body for [`vtt_cue`]. A sample that does not hold together puts
/// out what came before the break.
pub fn text_cues(
    kind: boxes::FourCc,
    sample: &[u8],
    out: &mut [core::ops::Range<usize>; MAX_CUES],
) -> usize {
    match &kind {
        b"tx3g" => {
            let Some(&[a, b]) = sample.get(..2) else {
                return 0;
            };
            let len = usize::from(u16::from_be_bytes([a, b]));
            if len == 0 || 2 + len > sample.len() {
                return 0;
            }
            out[0] = 2..2 + len;
            1
        }
        b"wvtt" => {
            let (mut at, mut n) = (0usize, 0usize);
            while at + 8 <= sample.len() && n < MAX_CUES {
                let Ok(h) = boxes::read_box_header(sample, at) else {
                    break;
                };
                if &h.kind == b"vttc"
                    && let Ok(Some(payl)) =
                        boxes::find_child(h.body(sample), boxes::fourcc(b"payl"))
                    && payl.end > payl.body_start
                {
                    out[n] = h.body_start..h.end;
                    n += 1;
                }
                at = h.end;
            }
            n
        }
        _ => 0,
    }
}

/// A `vttc` body (ISO/IEC 14496-30 7.4) as the WebVTT cue every `subtitle/vtt` buffer holds,
/// WebM's `D_WEBVTT` block form, the `iden` text, a line feed, the `sttg` text, a line feed,
/// the `payl` text. A missing box is empty text.
pub fn vtt_cue(vttc: &[u8], out: &mut Vec<u8>) {
    let child = |k: &[u8; 4]| match boxes::find_child(vttc, boxes::fourcc(k)) {
        Ok(Some(h)) => h.body(vttc),
        _ => &[],
    };
    let (id, settings, text) = (child(b"iden"), child(b"sttg"), child(b"payl"));
    out.clear();
    out.reserve(id.len() + settings.len() + text.len() + 2);
    out.extend_from_slice(id);
    out.push(b'\n');
    out.extend_from_slice(settings);
    out.push(b'\n');
    out.extend_from_slice(text);
}

pub fn is_video_family(family: &str) -> bool {
    matches!(
        family,
        "vp9" | "av1" | "h264/annexb" | "h265/annexb" | "mpeg4/asp" | "mpeg2" | "mpeg1"
    )
}

/// `avc3`/`hev1` also length-prefix their NALs, only their parameter sets are in band.
fn is_nal_entry(kind: &boxes::FourCc) -> bool {
    matches!(kind, b"avc1" | b"avc3" | b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" | b"dva1" | b"dvav")
}

pub(crate) fn is_hevc_entry(kind: &boxes::FourCc) -> bool {
    matches!(kind, b"hvc1" | b"hev1" | b"dvh1" | b"dvhe")
}

/// Parse the first sample entry of an `stsd` payload (8.5.2). Later entries are ignored.
pub fn parse_stsd(body: &[u8]) -> Result<SampleEntry, BoxError> {
    if body.len() < 8 {
        return Err(BoxError::Truncated("stsd header"));
    }
    let entry_count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    if entry_count == 0 {
        return Err(BoxError::Malformed("stsd with no sample entries"));
    }
    let entries = &body[8..];
    let h = boxes::read_box_header(entries, 0)?;
    parse_sample_entry(&h, entries)
}

fn parse_sample_entry(h: &BoxHeader, data: &[u8]) -> Result<SampleEntry, BoxError> {
    let kind = h.kind;
    let family = family_for(kind);
    let payload = h.body(data);
    let mut entry = SampleEntry {
        kind,
        family,
        width: 0,
        height: 0,
        par: (0, 0),
        sample_rate: 0,
        channels: 0,
        codec_head: Vec::new(),
        config_record: Vec::new(),
        reframer: Reframer::Passthrough,
        colour: StreamColor::default(),
        pcm: None,
        avg_bitrate: 0,
        bit_depth: 0,
    };
    if is_video_family(family) || is_nal_entry(&kind) {
        parse_visual_entry(payload, &mut entry)?;
    } else if &kind == b"mp4v" {
        // A broken entry stays bytes, as an unknown entry does.
        if parse_visual_entry(payload, &mut entry).is_err() {
            entry.family = "bytes";
        }
    } else if family == "audio/raw" {
        // A broken uncompressed entry leaves its track as bytes, as an unknown entry does.
        if parse_audio_entry(payload, &mut entry).is_err() {
            entry.family = "bytes";
        }
    } else if matches!(family, "opus" | "ac3" | "eac3" | "flac" | "mp3" | "alac")
        || &kind == b"mp4a"
    {
        parse_audio_entry(payload, &mut entry)?;
    }
    Ok(entry)
}

fn parse_visual_entry(payload: &[u8], entry: &mut SampleEntry) -> Result<(), BoxError> {
    const WIDTH_OFF: usize = 24;
    // The fixed VisualSampleEntry fields run to 78 (12.1.3).
    const CHILDREN_OFF: usize = 78;
    if payload.len() < CHILDREN_OFF {
        return Err(BoxError::Truncated("VisualSampleEntry fixed fields"));
    }
    entry.width = u16::from_be_bytes([payload[WIDTH_OFF], payload[WIDTH_OFF + 1]]) as u32;
    entry.height = u16::from_be_bytes([payload[WIDTH_OFF + 2], payload[WIDTH_OFF + 3]]) as u32;

    let children = &payload[CHILDREN_OFF..];
    if is_nal_entry(&entry.kind) {
        let is_hevc = is_hevc_entry(&entry.kind);
        let cfg_type: &[u8; 4] = if is_hevc { b"hvcC" } else { b"avcC" };
        match boxes::find_child(children, boxes::fourcc(cfg_type))? {
            Some(cfg) => {
                entry.config_record = cfg.body(children).to_vec();
                match nal_head_from_config(cfg.body(children), is_hevc) {
                    Ok((head, length_size)) => {
                        entry.codec_head = head;
                        entry.reframer = Reframer::Nal { length_size };
                    }
                    // A broken config record must not kill the demuxer. The decoder resyncs in
                    // band.
                    Err(_) => entry.reframer = Reframer::Nal { length_size: 4 },
                }
            }
            // lengthSizeMinusOne is almost always 3 (ISO/IEC 14496-15 5.2.4.1.1).
            None => entry.reframer = Reframer::Nal { length_size: 4 },
        }
    }
    // The av1C record is kept for the player's bit depth and as `V_AV1` CodecPrivate.
    if matches!(&entry.kind, b"av01" | b"dav1")
        && let Some(cfg) = boxes::find_child(children, boxes::fourcc(b"av1C"))?
    {
        entry.config_record = cfg.body(children).to_vec();
    }
    entry.colour = visual_colour(children);
    if &entry.kind == b"mp4v" {
        mp4v_esds(children, entry);
    }
    entry.avg_bitrate = btrt_avg(children).unwrap_or(entry.avg_bitrate);
    entry.par = pasp(children).unwrap_or((0, 0));
    Ok(())
}

/// The hSpacing and vSpacing of a `pasp` among `children` (12.1.4), None without one or with a 0.
fn pasp(children: &[u8]) -> Option<(u32, u32)> {
    let b = boxes::find_child(children, boxes::fourcc(b"pasp")).ok().flatten()?;
    let b = b.body(children).get(..8)?;
    let (h, v) = (
        u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
    );
    (h != 0 && v != 0).then_some((h, v))
}

/// The avgBitrate of a `btrt` among `children` (8.5.2.2), None without one or for 0.
fn btrt_avg(children: &[u8]) -> Option<u32> {
    let btrt = boxes::find_child(children, boxes::fourcc(b"btrt")).ok().flatten()?;
    let b = btrt.body(children).get(8..12)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]])).filter(|&v| v != 0)
}

/// Promotes `mp4v` by its esds objectTypeIndication (ISO/IEC 14496-1 7.2.6.6 Table 5, FFmpeg
/// `libavformat/isom.c`). The DecoderSpecificInfo, the VOS and VOL headers for MPEG-4 Visual,
/// leads in band and is the CodecPrivate on remux. Samples are plain elementary frames.
fn mp4v_esds(children: &[u8], entry: &mut SampleEntry) {
    let Some(esds) = boxes::find_child(children, boxes::fourcc(b"esds")).ok().flatten() else {
        return;
    };
    let Some((oti, dsi, avg)) = parse_esds(esds.body(children)) else { return };
    entry.avg_bitrate = avg;
    entry.family = match oti {
        0x20 => "mpeg4/asp",
        0x60..=0x65 => "mpeg2",
        0x6A => "mpeg1",
        _ => return,
    };
    entry.config_record = dsi.clone();
    entry.codec_head = dsi;
}

/// The sample format, byte order and frame size of an uncompressed sound entry. QTFF fixes the
/// depth by the four-cc (`in24`, `fl32`), `twos` and `sowt` by the v0 depth, `enda` in `wave`
/// flips to little-endian, `lpcm` states all in its v2 fields and ISO/IEC 23003-5 in `pcmC`.
fn pcm_entry<'a>(
    payload: &[u8],
    version: u16,
    entry: &mut SampleEntry,
    child: impl Fn(&[u8; 4]) -> Option<&'a [u8]>,
) {
    let be32 =
        |o: usize| payload.get(o..o + 4).and_then(|b| b.try_into().ok()).map(u32::from_be_bytes);
    let little = || {
        let wave = child(b"wave")?;
        let enda = boxes::find_child(wave, boxes::fourcc(b"enda")).ok().flatten()?;
        Some(enda.body(wave).iter().any(|&b| b != 0))
    };
    let le = || little().unwrap_or(false);
    let depth = payload.get(18..20).map_or(0, |b| u32::from(u16::from_be_bytes([b[0], b[1]])));
    // Bits, float, little-endian, signed.
    let shape = match &entry.kind {
        b"raw " => Some((depth, false, true, false)),
        b"twos" => Some((depth, false, false, true)),
        b"sowt" => Some((depth, false, true, true)),
        b"in24" => Some((24, false, le(), true)),
        b"in32" => Some((32, false, le(), true)),
        b"fl32" => Some((32, true, le(), true)),
        b"fl64" => Some((64, true, le(), true)),
        b"lpcm" if version == 2 => {
            let rate = payload.get(32..40).and_then(|b| b.try_into().ok()).map(f64::from_be_bytes);
            entry.sample_rate = rate.filter(|r| (1.0..1e7).contains(r)).map_or(0, |r| r as u32);
            // A count past what any mixer takes is a broken entry, said as no channels.
            let most = u32::from(profluens_audio::MAX_CHANNELS);
            entry.channels = be32(40).filter(|&c| c <= most).unwrap_or(0);
            let flags = be32(52).unwrap_or(0);
            Some((be32(48).unwrap_or(0), flags & 1 != 0, flags & 2 == 0, flags & 4 != 0))
        }
        b"ipcm" | b"fpcm" => child(b"pcmC")
            .and_then(|c| c.get(4..6))
            .map(|f| (u32::from(f[1]), &entry.kind == b"fpcm", f[0] & 1 != 0, true)),
        _ => None,
    };
    let Some((bits, float, le, signed)) = shape else { return };
    entry.bit_depth = bits;
    let order = |width: u8| if le { Reframer::Passthrough } else { Reframer::Swap { width } };
    let (sample, reframer) = match (bits, float, signed) {
        (8, false, false) => ("u8", Reframer::Passthrough),
        (8, false, true) => ("u8", Reframer::Flip { width: 1, big: false }),
        (16, false, false) => ("s16", Reframer::Flip { width: 2, big: !le }),
        (24, false, false) => ("s24", Reframer::Flip { width: 3, big: !le }),
        (32, false, false) => ("s32", Reframer::Flip { width: 4, big: !le }),
        (16, false, true) => ("s16", order(2)),
        (24, false, true) => ("s24", order(3)),
        (32, false, true) => ("s32", order(4)),
        (32, true, _) => ("f32", order(4)),
        (64, true, _) => ("f32", Reframer::Narrow { big: !le }),
        _ => return,
    };
    let most = u32::from(profluens_audio::MAX_CHANNELS);
    if entry.channels == 0 || entry.channels > most || entry.sample_rate == 0 {
        return;
    }
    // Bits is 8 to 64 here and channels at most MAX_CHANNELS, the product cannot overflow.
    let Some(frame) = entry.channels.checked_mul(bits).map(|b| b / 8).filter(|&f| f != 0) else {
        return;
    };
    entry.reframer = reframer;
    entry.pcm = Some(Pcm { sample, frame });
}

/// The colour a visual sample entry's `colr` (12.1.5), `mdcv` and `clli` boxes state.
/// A missing or broken box states nothing.
pub fn visual_colour(children: &[u8]) -> StreamColor {
    let mut c = StreamColor::default();
    let child = |kind: &[u8; 4]| {
        boxes::find_child(children, boxes::fourcc(kind)).ok().flatten().map(|h| h.body(children))
    };
    if let Some(b) =
        child(b"colr").filter(|b| b.len() >= 10 && matches!(&b[..4], b"nclx" | b"nclc"))
    {
        let point = |o: usize| u8::try_from(u16::from_be_bytes([b[o], b[o + 1]])).ok();
        c.primaries = point(4).and_then(Primaries::from_h273);
        c.transfer = point(6).and_then(TransferFn::from_h273);
        c.matrix = point(8).and_then(Matrix::from_h273);
        if &b[..4] == b"nclx" {
            c.range = b.get(10).map(|f| if f & 0x80 != 0 { Range::Full } else { Range::Limited });
        }
    }
    if let Some(m) = child(b"mdcv").and_then(Mastering::from_st2086) {
        c.hdr.mastering = m;
    }
    if let Some(l) = child(b"clli").and_then(ContentLight::from_cta861) {
        c.hdr.light = l;
    }
    // FFmpeg `libavformat/mov.c` `mov_read_dvcc_dvvc`.
    c.dovi = [b"dvcC", b"dvvC", b"dvwC"]
        .into_iter()
        .find_map(&child)
        .and_then(|b| DoviConfig::from_record(b).ok());
    // `vpcC` version 1 (VP Codec ISO Media File Format Binding 2.3.3). A `colr` wins.
    if let Some(b) = child(b"vpcC").filter(|b| b.len() >= 10 && b[0] == 1) {
        let bits = b[6];
        c.chroma = c.chroma.or(match bits >> 1 & 0x07 {
            0 => Some(ChromaSite::Left),
            1 => Some(ChromaSite::TopLeft),
            _ => None,
        });
        c.range = c.range.or(Some(Range::from_flag(bits & 1 == 1)));
        c.primaries = c.primaries.or(Primaries::from_h273(b[7]));
        c.transfer = c.transfer.or(TransferFn::from_h273(b[8]));
        c.matrix = c.matrix.or(Matrix::from_h273(b[9]));
    }
    // AV1 ISOBMFF binding 2.3.3, the third byte ends in chroma_sample_position.
    if let Some(b) = child(b"av1C").filter(|b| b.len() >= 3) {
        c.chroma = c.chroma.or(ChromaSite::from_av1(u32::from(b[2] & 0x03)));
    }
    c
}

fn parse_audio_entry(payload: &[u8], entry: &mut SampleEntry) -> Result<(), BoxError> {
    // Samplerate at octet 24 (12.2.3).
    const CHANNELS_OFF: usize = 16;
    const RATE_OFF: usize = 24;
    const CHILDREN_OFF: usize = 28;
    if payload.len() < RATE_OFF + 4 {
        return Err(BoxError::Truncated("AudioSampleEntry fixed fields"));
    }
    entry.channels = u16::from_be_bytes([payload[CHANNELS_OFF], payload[CHANNELS_OFF + 1]]) as u32;
    entry.sample_rate = u32::from_be_bytes([
        payload[RATE_OFF],
        payload[RATE_OFF + 1],
        payload[RATE_OFF + 2],
        payload[RATE_OFF + 3],
    ]) >> 16;

    // A QTFF SoundDescription (version at octet 8) inserts 16 (v1) or 36 (v2) octets
    // before the children.
    let version = u16::from_be_bytes([payload[8], payload[9]]);
    entry.bit_depth = u32::from(u16::from_be_bytes([payload[18], payload[19]]));
    let children_off = match version {
        1 => CHILDREN_OFF + 16,
        2 => CHILDREN_OFF + 36,
        _ => CHILDREN_OFF,
    };
    let child = |kind: &[u8; 4]| {
        let children = payload.get(children_off..)?;
        boxes::find_child(children, boxes::fourcc(kind)).ok().flatten().map(|h| h.body(children))
    };
    match &entry.kind {
        b"fLaC" => {
            if let Some(head) = child(b"dfLa").and_then(flac_head) {
                if let Some(rate) = streaminfo_rate(&head) {
                    entry.sample_rate = rate;
                }
                // STREAMINFO's bits per sample less one, 5 bits behind the channels
                if let Some(b) = head.get(20..22) {
                    entry.bit_depth = u32::from((b[0] & 1) << 4 | b[1] >> 4) + 1;
                }
                entry.config_record = head.clone();
                entry.codec_head = head;
            }
        }
        _ if entry.family == "audio/raw" => {
            pcm_entry(payload, version, entry, child);
            if entry.pcm.is_none() {
                entry.family = "bytes";
            }
        }
        // Kept as the Matroska CodecPrivate and the open's head, the demuxer sends none.
        b"Opus" => {
            if let Some(head) = child(b"dOps").and_then(crate::writer::opus_head_from_dops) {
                entry.config_record = head;
            }
        }
        b"alac" => {
            if let Some(cookie) = child(b"alac").and_then(|b| b.get(4..)) {
                // ALACSpecificConfig bitDepth, behind frameLength and compatibleVersion
                entry.bit_depth = cookie.get(5).map_or(entry.bit_depth, |&d| u32::from(d));
                entry.config_record = cookie.to_vec();
                entry.codec_head = cookie.to_vec();
            }
        }
        _ => {}
    }
    if &entry.kind == b"mp4a" {
        // A broken esds keeps the `bytes` fallback.
        let esds = payload.get(children_off..).and_then(|children| {
            boxes::find_child(children, boxes::fourcc(b"esds"))
                .ok()
                .flatten()
                .map(|h| h.body(children).to_vec())
        });
        if let Some((oti, asc, avg)) = esds.as_deref().and_then(parse_esds) {
            entry.avg_bitrate = avg;
            // 14496-1 7.2.6.6 Table 8. 0x40 and 0x66-0x68 are AAC and need the ASC. 0x6B and 0x69
            // are MPEG audio with no DecoderSpecificInfo.
            match oti {
                0x40 | 0x66 | 0x67 | 0x68 if !asc.is_empty() => {
                    entry.family = "aac";
                    entry.config_record = asc.clone();
                    entry.codec_head = asc;
                }
                0x69 | 0x6B => entry.family = "mp3",
                _ => {}
            }
        }
    }
    // a btrt wins over the esds
    if let Some(avg) = payload.get(children_off..).and_then(btrt_avg) {
        entry.avg_bitrate = avg;
    }
    Ok(())
}

/// A native FLAC stream head from a `dfLa` body (RFC 9639 8.1). The first block must be
/// STREAMINFO. The last flag is rewritten. `None` for version != 0 or a short block.
fn flac_head(dfla: &[u8]) -> Option<Vec<u8>> {
    let (head, blocks) = (dfla.get(..4)?, dfla.get(4..)?);
    if head[0] != 0 || blocks.first().is_none_or(|b| b & 0x7F != 0) {
        return None;
    }
    let mut out = Vec::with_capacity(4 + blocks.len());
    out.extend_from_slice(b"fLaC");
    let (mut at, mut last) = (0usize, 4usize);
    while at < blocks.len() {
        let h = blocks.get(at..at + 4)?;
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        let end = (at + 4).checked_add(len).filter(|&e| e <= blocks.len())?;
        last = out.len();
        out.push(h[0] & 0x7F);
        out.extend_from_slice(&blocks[at + 1..end]);
        at = end;
    }
    out[last] |= 0x80;
    Some(out)
}

fn streaminfo_rate(head: &[u8]) -> Option<u32> {
    let b = head.get(18..21)?;
    let rate = (u32::from(b[0]) << 12) | (u32::from(b[1]) << 4) | (u32::from(b[2]) >> 4);
    (rate != 0).then_some(rate)
}

const TAG_ES_DESCR: u8 = 0x03;
const TAG_DECODER_CONFIG: u8 = 0x04;
const TAG_DECODER_SPECIFIC_INFO: u8 = 0x05;

/// Read one descriptor header at `data[at]` (ISO/IEC 14496-1 8.3.3), at most 4 size octets.
fn read_descriptor(data: &[u8], at: usize) -> Option<(u8, core::ops::Range<usize>)> {
    let tag = *data.get(at)?;
    let mut size = 0usize;
    let mut i = at + 1;
    for n in 0.. {
        let b = *data.get(i)?;
        i += 1;
        size = (size << 7) | (b & 0x7f) as usize;
        if b & 0x80 == 0 {
            break;
        }
        if n == 3 {
            return None; // 8.3.3 sizeOfInstance uses at most 4 octets
        }
    }
    let end = i.checked_add(size)?;
    if end > data.len() {
        return None;
    }
    Some((tag, i..end))
}

/// `(objectTypeIndication, DecoderSpecificInfo, avgBitrate)` from an `esds` payload (ISO/IEC
/// 14496-14 5.6, 14496-1 7.2.6.5). `None` on any structural violation.
fn parse_esds(payload: &[u8]) -> Option<(u8, Vec<u8>, u32)> {
    let (tag, es) = read_descriptor(payload, 4)?;
    if tag != TAG_ES_DESCR {
        return None;
    }
    let es_data = &payload[es];
    let flags = *es_data.get(2)?;
    let mut at = 3usize;
    if flags & 0x80 != 0 {
        at += 2; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        at += 1 + *es_data.get(at)? as usize; // URLlength + URLstring
    }
    if flags & 0x20 != 0 {
        at += 2; // OCR_ES_Id
    }
    while at < es_data.len() {
        let (tag, r) = read_descriptor(es_data, at)?;
        if tag == TAG_DECODER_CONFIG {
            let dc = &es_data[r];
            let oti = *dc.first()?;
            // behind streamType, bufferSizeDB and maxBitrate
            let avg = dc.get(9..13).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
            let mut dat = 13usize;
            while dat < dc.len() {
                let (t, r) = read_descriptor(dc, dat)?;
                if t == TAG_DECODER_SPECIFIC_INFO {
                    return Some((oti, dc[r].to_vec(), avg));
                }
                dat = r.end;
            }
            return Some((oti, Vec::new(), avg)); // no DecoderSpecificInfo
        }
        at = r.end;
    }
    None
}

#[cfg(test)]
mod tests {

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn colr_mdcv_and_clli_state_the_colour() {
        let mut nclx = b"nclx".to_vec();
        nclx.extend_from_slice(&[0, 9, 0, 16, 0, 9, 0x00]);
        let mut mdcv = Vec::new();
        for v in [8500u16, 39850, 6550, 2300, 35400, 14600, 15635, 16450] {
            mdcv.extend_from_slice(&v.to_be_bytes());
        }
        mdcv.extend_from_slice(&10_000_000u32.to_be_bytes());
        mdcv.extend_from_slice(&50u32.to_be_bytes());
        let children = [
            bx(b"hvcC", &[1, 2, 3]),
            bx(b"colr", &nclx),
            bx(b"mdcv", &mdcv),
            bx(b"clli", &[3, 0xE8, 1, 0x90]),
        ]
        .concat();
        let c = visual_colour(&children);
        assert_eq!((c.matrix, c.range), (Some(Matrix::Bt2020), Some(Range::Limited)));
        assert_eq!((c.transfer, c.primaries), (Some(TransferFn::Pq), Some(Primaries::Bt2020)));
        assert_eq!(c.hdr.mastering.primaries, [[35400, 14600], [8500, 39850], [6550, 2300]]);
        assert_eq!(
            (c.hdr.mastering.max_luminance, c.hdr.mastering.min_luminance),
            (10_000_000, 50)
        );
        assert_eq!((c.hdr.light.max_cll, c.hdr.light.max_fall), (1000, 400));
        let other =
            [bx(b"colr", b"prof\x00\x00\x00\x00\x00\x00\x00"), bx(b"mdcv", &mdcv[..20])].concat();
        assert_eq!(visual_colour(&other), StreamColor::default());
    }

    use super::*;

    #[test]
    fn family_mapping_covers_the_box_types() {
        assert_eq!(family_for(*b"avc1"), "h264/annexb");
        assert_eq!(family_for(*b"avc3"), "h264/annexb");
        assert_eq!(family_for(*b"hvc1"), "h265/annexb");
        assert_eq!(family_for(*b"hev1"), "h265/annexb");
        assert_eq!(family_for(*b"vp09"), "vp9");
        assert_eq!(family_for(*b"av01"), "av1");
        assert_eq!(family_for(*b"Opus"), "opus");
        assert_eq!(family_for(*b"ac-3"), "ac3");
        assert_eq!(family_for(*b"ec-3"), "eac3");
        assert_eq!(family_for(*b"mp4a"), "bytes", "mp4a waits for its esds");
        assert_eq!(family_for(*b"fLaC"), "flac");
        assert_eq!(family_for(*b"alac"), "alac");
        assert_eq!(family_for(*b".mp3"), "mp3");
        assert_eq!(family_for(*b"tx3g"), "subtitle/srt");
        assert_eq!(family_for(*b"wvtt"), "subtitle/vtt");
        assert_eq!(family_for(*b"stpp"), "subtitle/ttml");
        assert_eq!(family_for(*b"twos"), "audio/raw");
        assert_eq!(family_for(*b"mp4v"), "bytes", "mp4v waits for its esds");
        assert_eq!(family_for(*b"xxxx"), "bytes", "unknown, the bytes fallback");
    }

    #[test]
    fn stsd_avc1_yields_dims_and_annex_b_head() {
        let sps = [0x67u8, 0x42, 0x00, 0x0A];
        let pps = [0x68u8, 0xCE, 0x3C, 0x80];
        let mut avcc = vec![1u8, 0x42, 0x00, 0x0A, 0xFF, 0xE1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&sps);
        avcc.push(1);
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&pps);
        let avcc_box = box_bytes(b"avcC", &avcc);

        let mut ve = vec![0u8; 78];
        ve[24..26].copy_from_slice(&640u16.to_be_bytes());
        ve[26..28].copy_from_slice(&480u16.to_be_bytes());
        ve.extend_from_slice(&avcc_box);
        let avc1_box = box_bytes(b"avc1", &ve);

        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&avc1_box);

        let entry = parse_stsd(&stsd).unwrap();
        assert_eq!(entry.kind, *b"avc1");
        assert_eq!(entry.family, "h264/annexb");
        assert_eq!((entry.width, entry.height), (640, 480));
        assert!(matches!(entry.reframer, Reframer::Nal { length_size: 4 }));
        let mut want = Vec::new();
        for nal in [&sps[..], &pps[..]] {
            want.extend_from_slice(&[0, 0, 0, 1]);
            want.extend_from_slice(nal);
        }
        assert_eq!(entry.codec_head, want);
    }

    #[test]
    fn stsd_vp09_is_passthrough_with_dims() {
        let mut ve = vec![0u8; 78];
        ve[24..26].copy_from_slice(&128u16.to_be_bytes());
        ve[26..28].copy_from_slice(&96u16.to_be_bytes());
        let vp09_box = box_bytes(b"vp09", &ve);
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&vp09_box);
        let entry = parse_stsd(&stsd).unwrap();
        assert_eq!(entry.family, "vp9");
        assert_eq!((entry.width, entry.height), (128, 96));
        assert!(matches!(entry.reframer, Reframer::Passthrough));
        assert!(entry.codec_head.is_empty());
        assert_eq!(entry.par, (0, 0));
    }

    #[test]
    fn stsd_pasp_states_the_pixel_aspect() {
        let entry_with = |pasp: &[u8]| {
            let mut ve = vec![0u8; 78];
            ve[24..26].copy_from_slice(&720u16.to_be_bytes());
            ve[26..28].copy_from_slice(&480u16.to_be_bytes());
            ve.extend_from_slice(&box_bytes(b"pasp", pasp));
            let mut stsd = vec![0u8; 4];
            stsd.extend_from_slice(&1u32.to_be_bytes());
            stsd.extend_from_slice(&box_bytes(b"vp09", &ve));
            parse_stsd(&stsd).unwrap()
        };
        let par = [32u32.to_be_bytes(), 27u32.to_be_bytes()].concat();
        assert_eq!(entry_with(&par).par, (32, 27));
        let zero = [32u32.to_be_bytes(), 0u32.to_be_bytes()].concat();
        assert_eq!(entry_with(&zero).par, (0, 0), "a 0 states none");
        assert_eq!(entry_with(&par[..6]).par, (0, 0), "cut");
    }

    #[test]
    fn stsd_mp4a_audio_params() {
        let mut ae = vec![0u8; 28];
        ae[16..18].copy_from_slice(&2u16.to_be_bytes()); // channelcount
        ae[24..28].copy_from_slice(&(48_000u32 << 16).to_be_bytes()); // 16.16 samplerate
        let mp4a_box = box_bytes(b"mp4a", &ae);
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&mp4a_box);
        let entry = parse_stsd(&stsd).unwrap();
        assert_eq!(entry.family, "bytes");
        assert_eq!((entry.channels, entry.sample_rate), (2, 48_000));
        assert!(entry.config_record.is_empty(), "no esds → no ASC");
    }

    fn esds_body(oti: u8, asc: &[u8], long_sizes: bool) -> Vec<u8> {
        let size = |v: &mut Vec<u8>, n: usize| {
            if long_sizes {
                v.extend_from_slice(&[0x80, 0x80, 0x80, n as u8]);
            } else {
                v.push(n as u8);
            }
        };
        let mut dsi = vec![TAG_DECODER_SPECIFIC_INFO];
        size(&mut dsi, asc.len());
        dsi.extend_from_slice(asc);
        let mut dc_payload = vec![oti, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        dc_payload.extend_from_slice(&dsi);
        let mut dc = vec![TAG_DECODER_CONFIG];
        size(&mut dc, dc_payload.len());
        dc.extend_from_slice(&dc_payload);
        let mut es_payload = vec![0, 1, 0];
        es_payload.extend_from_slice(&dc);
        let mut es = vec![TAG_ES_DESCR];
        size(&mut es, es_payload.len());
        es.extend_from_slice(&es_payload);
        let mut body = vec![0u8; 4];
        body.extend_from_slice(&es);
        body
    }

    fn mp4a_entry(esds: &[u8], rate: u32, channels: u16) -> Vec<u8> {
        let mut ae = vec![0u8; 28];
        ae[16..18].copy_from_slice(&channels.to_be_bytes());
        ae[24..28].copy_from_slice(&(rate << 16).to_be_bytes());
        ae.extend_from_slice(&box_bytes(b"esds", esds));
        ae
    }

    #[test]
    fn stsd_mp4a_with_esds_promotes_to_aac() {
        // AAC-LC, 48 kHz, 2 channels (ISO/IEC 14496-3 1.6.2.1).
        let asc = [0x11u8, 0x90];
        let ae = mp4a_entry(&esds_body(0x40, &asc, false), 48_000, 2);
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&box_bytes(b"mp4a", &ae));
        let entry = parse_stsd(&stsd).unwrap();
        assert_eq!(entry.family, "aac");
        assert_eq!((entry.channels, entry.sample_rate), (2, 48_000));
        assert_eq!(entry.config_record, asc, "CodecPrivate = the ASC verbatim");
        assert_eq!(entry.codec_head, asc, "in-band head = the ASC (first-buffer contract)");
        assert!(matches!(entry.reframer, Reframer::Passthrough), "raw AAC AUs, no reframe");
    }

    #[test]
    fn esds_long_expandable_sizes_parse() {
        let asc = [0x12u8, 0x08]; // AOT=2, freq index 4 = 44.1 kHz, 1 channel
        let ae = mp4a_entry(&esds_body(0x40, &asc, true), 44_100, 1);
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&box_bytes(b"mp4a", &ae));
        let entry = parse_stsd(&stsd).unwrap();
        assert_eq!(entry.family, "aac");
        assert_eq!(entry.config_record, asc);
    }

    #[test]
    fn esds_mpeg_audio_is_mp3_and_other_oti_stays_bytes() {
        let stsd_of = |esds: &[u8]| {
            let mut stsd = vec![0u8; 4];
            stsd.extend_from_slice(&1u32.to_be_bytes());
            stsd.extend_from_slice(&box_bytes(b"mp4a", &mp4a_entry(esds, 44_100, 2)));
            parse_stsd(&stsd).unwrap()
        };
        for oti in [0x6B, 0x69] {
            let entry = stsd_of(&esds_body(oti, &[0x11, 0x90], false));
            assert_eq!(entry.family, "mp3");
            assert!(entry.config_record.is_empty() && entry.codec_head.is_empty());
        }
        let ffmpeg = [
            0, 0, 0, 0, 0x03, 0x80, 0x80, 0x80, 0x1B, 0, 1, 0, 0x04, 0x80, 0x80, 0x80, 0x0D, 0x6B,
            0x15, 0, 0, 0, 0, 0, 0xFB, 0x89, 0, 0, 0xFB, 0x89, 0x06, 0x80, 0x80, 0x80, 0x01, 0x02,
        ];
        assert_eq!(stsd_of(&ffmpeg).family, "mp3");
        assert_eq!(
            stsd_of(&esds_body(0xA5, &[1], false)).family,
            "bytes",
            "AC-3 by OTI is not taken here"
        );
    }

    fn mp4v_stsd(esds: Option<&[u8]>) -> Vec<u8> {
        let mut ve = vec![0u8; 78];
        ve[24..26].copy_from_slice(&176u16.to_be_bytes());
        ve[26..28].copy_from_slice(&144u16.to_be_bytes());
        if let Some(esds) = esds {
            ve.extend_from_slice(&box_bytes(b"esds", esds));
        }
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&box_bytes(b"mp4v", &ve));
        stsd
    }

    #[test]
    fn mp4v_takes_its_family_and_decoder_specific_info_from_the_esds() {
        // visual_object_sequence_start_code and a video_object_start_code (14496-2 6.2.2).
        let vol = [0u8, 0, 1, 0xB0, 0x01, 0, 0, 1, 0x00];
        for (oti, family) in [
            (0x20, "mpeg4/asp"),
            (0x60, "mpeg2"),
            (0x61, "mpeg2"),
            (0x65, "mpeg2"),
            (0x6A, "mpeg1"),
        ] {
            let e = parse_stsd(&mp4v_stsd(Some(&esds_body(oti, &vol, oti == 0x20)))).unwrap();
            assert_eq!((e.family, e.width, e.height), (family, 176, 144), "oti {oti:#X}");
            assert_eq!(e.config_record, vol);
            assert_eq!(e.codec_head, vol, "the DSI leads in band");
            assert!(matches!(e.reframer, Reframer::Passthrough), "plain elementary frames");
        }
        // FFmpeg's MPEG-2 Main esds, no DecoderSpecificInfo, the sequence header is in band.
        let ffmpeg = [
            0, 0, 0, 0, 0x03, 0x80, 0x80, 0x80, 0x1B, 0, 1, 0, 0x04, 0x80, 0x80, 0x80, 0x0D, 0x61,
            0x11, 0, 0, 0, 0, 0x03, 0x0D, 0x40, 0, 0x02, 0x9F, 0, 0x06, 0x80, 0x80, 0x80, 0x01,
            0x02,
        ];
        let e = parse_stsd(&mp4v_stsd(Some(&ffmpeg))).unwrap();
        assert_eq!(e.family, "mpeg2");
        assert!(e.codec_head.is_empty() && e.config_record.is_empty());
        for (esds, why) in [
            (Some(esds_body(0x21, &vol, false)), "H.264 by OTI is not taken here"),
            (Some(esds_body(0x6C, &vol, false)), "JPEG"),
            (Some(vec![0, 0, 0, 0, 0x03, 0x7F]), "a broken esds"),
            (None, "no esds"),
        ] {
            let e = parse_stsd(&mp4v_stsd(esds.as_deref())).unwrap();
            assert_eq!(e.family, "bytes", "{why}");
            assert!(e.codec_head.is_empty(), "{why}");
        }
        let mut short = vec![0u8; 4];
        short.extend_from_slice(&1u32.to_be_bytes());
        short.extend_from_slice(&box_bytes(b"mp4v", &[0u8; 40]));
        assert_eq!(parse_stsd(&short).unwrap().family, "bytes", "a cut entry stays bytes");
    }

    fn audio_stsd(kind: &[u8; 4], rate: u32, child: &[u8]) -> Vec<u8> {
        let mut ae = vec![0u8; 28];
        ae[16..18].copy_from_slice(&2u16.to_be_bytes());
        ae[24..28].copy_from_slice(&(rate << 16).to_be_bytes());
        ae.extend_from_slice(child);
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&box_bytes(kind, &ae));
        stsd
    }

    /// A QTFF v2 `lpcm` entry, its rate, channels, depth and flags in the v2 fields.
    fn lpcm_stsd(rate: f64, channels: u32, bits: u32, flags: u32) -> Vec<u8> {
        let mut ae = vec![0u8; 28];
        ae[8..10].copy_from_slice(&2u16.to_be_bytes());
        ae[24..28].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        for word in [72u32.to_be_bytes().to_vec(), rate.to_be_bytes().to_vec()] {
            ae.extend_from_slice(&word);
        }
        for v in [channels, 0x7F00_0000, bits, flags, channels * bits / 8, 1] {
            ae.extend_from_slice(&v.to_be_bytes());
        }
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&box_bytes(b"lpcm", &ae));
        stsd
    }

    #[test]
    fn lpcm_v2_takes_rate_channels_and_format_from_its_fields() {
        let e = parse_stsd(&lpcm_stsd(44_100.0, 6, 24, 0x4)).unwrap();
        assert_eq!((e.family, e.sample_rate, e.channels), ("audio/raw", 44_100, 6));
        assert_eq!(e.pcm, Some(Pcm { sample: "s24", frame: 18 }));
        assert!(matches!(e.reframer, Reframer::Passthrough));
        let e = parse_stsd(&lpcm_stsd(96_000.0, 2, 32, 0x1 | 0x2)).unwrap();
        assert_eq!(e.pcm, Some(Pcm { sample: "f32", frame: 8 }));
        assert!(matches!(e.reframer, Reframer::Swap { width: 4 }));
        let unsigned = parse_stsd(&lpcm_stsd(48_000.0, 2, 16, 0)).unwrap();
        assert_eq!(unsigned.pcm, Some(Pcm { sample: "s16", frame: 4 }));
        assert!(matches!(unsigned.reframer, Reframer::Flip { width: 2, big: false }));
        let unsigned_be = parse_stsd(&lpcm_stsd(48_000.0, 1, 24, 0x2)).unwrap();
        assert_eq!(unsigned_be.pcm, Some(Pcm { sample: "s24", frame: 3 }));
        assert!(matches!(unsigned_be.reframer, Reframer::Flip { width: 3, big: true }));
        let odd = parse_stsd(&lpcm_stsd(48_000.0, 2, 12, 0x4)).unwrap();
        assert_eq!((odd.family, odd.pcm), ("bytes", None), "12 bits has no name");
        let bad = parse_stsd(&lpcm_stsd(f64::NAN, 2, 16, 0x4)).unwrap();
        assert_eq!(bad.pcm, None);
    }

    #[test]
    fn flac_in_mp4_rebuilds_the_stream_head() {
        let mut info = vec![0x00, 0, 0, 34, 0x10, 0, 0x10, 0, 0, 0, 0, 0, 0, 0];
        info.extend_from_slice(&[0x17, 0x70, 0x01, 0xF0]);
        info.extend_from_slice(&[0u8; 20]);
        let blocks = [&info[..], &[0x01, 0, 0, 2, 0, 0]].concat();
        let dfla = box_bytes(b"dfLa", &[&[0u8, 0, 0, 0][..], &blocks].concat());
        let entry = parse_stsd(&audio_stsd(b"fLaC", 0, &dfla)).unwrap();
        assert_eq!(entry.family, "flac");
        assert_eq!(&entry.codec_head[..4], b"fLaC");
        assert_eq!(entry.codec_head.len(), 4 + blocks.len());
        assert_eq!(
            (entry.codec_head[4], entry.codec_head[4 + 38]),
            (0x00, 0x81),
            "only the last block is flagged"
        );
        assert_eq!(entry.sample_rate, 96_000, "the rate STREAMINFO states");
        assert_eq!(entry.bit_depth, 32, "the depth STREAMINFO states");
        assert_eq!(entry.config_record, entry.codec_head);
        for bad in [
            [&[1u8, 0, 0, 0][..], &blocks].concat(),
            [&[0u8; 4][..], &blocks[38..]].concat(),
            [&[0u8; 4][..], &blocks[..30]].concat(),
        ] {
            let entry =
                parse_stsd(&audio_stsd(b"fLaC", 44_100, &box_bytes(b"dfLa", &bad))).unwrap();
            assert_eq!(entry.family, "flac");
            assert!(entry.codec_head.is_empty(), "{bad:02X?}");
        }
    }

    #[test]
    fn alac_and_text_entries_name_their_families() {
        let cookie = [0u8, 0, 0x10, 0, 0, 0x10, 0x28, 0x0A, 0x0E, 2, 0, 0xFF];
        let alac = box_bytes(b"alac", &[&[0u8; 4][..], &cookie].concat());
        let entry = parse_stsd(&audio_stsd(b"alac", 44_100, &alac)).unwrap();
        assert_eq!((entry.family, entry.sample_rate, entry.channels), ("alac", 44_100, 2));
        assert_eq!(entry.config_record, cookie);
        for (kind, family) in
            [(b"tx3g", "subtitle/srt"), (b"wvtt", "subtitle/vtt"), (b"stpp", "subtitle/ttml")]
        {
            let mut stsd = vec![0u8; 4];
            stsd.extend_from_slice(&1u32.to_be_bytes());
            stsd.extend_from_slice(&box_bytes(kind, &[0u8; 8]));
            assert_eq!(parse_stsd(&stsd).unwrap().family, family);
        }
    }

    #[test]
    fn an_entry_states_its_bitrate_and_an_audio_one_its_depth() {
        let btrt = |avg: u32| {
            let body = [&[0u8; 4][..], &9_000_000u32.to_be_bytes(), &avg.to_be_bytes()].concat();
            box_bytes(b"btrt", &body)
        };
        let mut esds = esds_body(0x40, &[0x11, 0x90], false);
        // the DecoderConfigDescriptor's avgBitrate, 13 octets into its 15 octet head
        let dc = esds.iter().position(|&b| b == TAG_DECODER_CONFIG).unwrap() + 2;
        esds[dc + 9..dc + 13].copy_from_slice(&128_000u32.to_be_bytes());
        let mut ae = mp4a_entry(&esds, 48_000, 2);
        ae[18..20].copy_from_slice(&16u16.to_be_bytes());
        let aac = |ae: &[u8]| {
            parse_stsd(&[&[0u8; 4][..], &1u32.to_be_bytes(), &box_bytes(b"mp4a", ae)].concat())
                .unwrap()
        };
        let e = aac(&ae);
        assert_eq!((e.family, e.avg_bitrate, e.bit_depth), ("aac", 128_000, 16));
        ae.extend_from_slice(&btrt(96_000));
        assert_eq!(aac(&ae).avg_bitrate, 96_000, "a btrt wins over the esds");
        let mut ve = mp4v_stsd(None);
        ve.extend_from_slice(&btrt(2_500_000));
        let grown = (u32::from_be_bytes(ve[8..12].try_into().unwrap()) + 20).to_be_bytes();
        ve[8..12].copy_from_slice(&grown);
        let e = parse_stsd(&ve).unwrap();
        assert_eq!((e.avg_bitrate, e.bit_depth), (2_500_000, 0));
        assert_eq!(parse_stsd(&mp4v_stsd(None)).unwrap().avg_bitrate, 0, "no btrt");
        let twos = parse_stsd(&lpcm_stsd(48_000.0, 2, 24, 0x4)).unwrap();
        assert_eq!(twos.bit_depth, 24, "the v2 fields' depth");
        let cookie = [0u8, 0, 0x10, 0, 0, 0x18, 0x28, 0x0A, 0x0E, 2, 0, 0xFF];
        let alac = box_bytes(b"alac", &[&[0u8; 4][..], &cookie].concat());
        assert_eq!(parse_stsd(&audio_stsd(b"alac", 44_100, &alac)).unwrap().bit_depth, 24);
    }

    #[test]
    fn esds_malformed_degrades_to_bytes() {
        for bad in [
            &[0u8; 4][..],                          // FullBox head only, no descriptor
            &[0, 0, 0, 0, 0x03][..],                // tag with no size octet
            &[0, 0, 0, 0, 0x03, 0x80][..],          // dangling continuation bit
            &[0, 0, 0, 0, 0x03, 0x7F][..],          // size runs past the payload
            &[0, 0, 0, 0, 0x04, 0x02, 0x40, 0][..], // DecoderConfig without ES wrapper
        ] {
            let ae = mp4a_entry(bad, 48_000, 2);
            let mut stsd = vec![0u8; 4];
            stsd.extend_from_slice(&1u32.to_be_bytes());
            stsd.extend_from_slice(&box_bytes(b"mp4a", &ae));
            let entry = parse_stsd(&stsd).unwrap();
            assert_eq!(entry.family, "bytes", "bad esds {bad:02X?} must fall back");
            assert!(entry.config_record.is_empty());
        }
    }

    #[test]
    fn text_samples_give_their_cues() {
        let mut out: [core::ops::Range<usize>; MAX_CUES] = Default::default();
        let cue = |s: &[u8], k: &[u8; 4], out: &mut [core::ops::Range<usize>; MAX_CUES]| {
            let n = text_cues(*k, s, out);
            let mut vtt = Vec::new();
            out[..n]
                .iter()
                .map(|r| {
                    let text = match k {
                        b"wvtt" => {
                            vtt_cue(&s[r.clone()], &mut vtt);
                            &vtt[..]
                        }
                        _ => &s[r.clone()],
                    };
                    String::from_utf8_lossy(text).into_owned()
                })
                .collect::<Vec<_>>()
        };
        let mut tx3g = vec![0, 9];
        tx3g.extend_from_slice(b"first cue");
        tx3g.extend_from_slice(&box_bytes(b"styl", &[0, 0]));
        assert_eq!(cue(&tx3g, b"tx3g", &mut out), ["first cue"]);
        assert!(cue(&[0, 0], b"tx3g", &mut out).is_empty(), "a gap");
        assert!(cue(&[0, 9, b'a'], b"tx3g", &mut out).is_empty(), "a length past the sample");
        assert!(cue(&[], b"tx3g", &mut out).is_empty());
        let vttc = |text: &str| {
            box_bytes(
                b"vttc",
                &[box_bytes(b"sttg", b"align:start"), box_bytes(b"payl", text.as_bytes())].concat(),
            )
        };
        let two = [vttc("one"), box_bytes(b"vtta", b"note"), vttc("two\nlines")].concat();
        assert_eq!(
            cue(&two, b"wvtt", &mut out),
            ["\nalign:start\none", "\nalign:start\ntwo\nlines"]
        );
        assert!(cue(&box_bytes(b"vtte", &[]), b"wvtt", &mut out).is_empty(), "a gap");
        let broken = [vttc("kept"), vec![0, 0, 0, 3, b'v', b't', b't', b'c']].concat();
        assert_eq!(cue(&broken, b"wvtt", &mut out), ["\nalign:start\nkept"]);
        let named =
            box_bytes(b"vttc", &[box_bytes(b"payl", b"hi"), box_bytes(b"iden", b"c1")].concat());
        assert_eq!(cue(&named, b"wvtt", &mut out), ["c1\n\nhi"], "its id, no settings");
        let silent =
            box_bytes(b"vttc", &[box_bytes(b"iden", b"c1"), box_bytes(b"payl", b"")].concat());
        assert!(cue(&silent, b"wvtt", &mut out).is_empty(), "no text, no cue");
        assert!(cue(b"anything", b"stpp", &mut out).is_empty(), "TTML is not read");
    }

    #[test]
    fn malformed_stsd_errors_not_panic() {
        assert!(parse_stsd(&[]).is_err());
        assert!(parse_stsd(&[0, 0, 0, 0, 0, 0, 0, 0]).is_err(), "entry_count 0");
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&[0, 0, 0, 8]); // a box header cut short
        assert!(parse_stsd(&stsd).is_err());
    }

    fn dvvc_body() -> Vec<u8> {
        let mut r = vec![0u8; 24];
        r[0] = 1;
        r[2..4].copy_from_slice(&(8u16 << 9 | 6 << 3 | 1 << 2 | 1).to_be_bytes());
        r[4] = 1 << 4;
        r
    }

    #[test]
    fn dolby_vision_entries_map_to_their_base_codec_and_state_the_record() {
        assert_eq!(family_for(*b"dvh1"), "h265/annexb");
        assert_eq!(family_for(*b"dvhe"), "h265/annexb");
        assert_eq!(family_for(*b"dva1"), "h264/annexb");
        assert_eq!(family_for(*b"dvav"), "h264/annexb");
        assert_eq!(family_for(*b"dav1"), "av1");
        let hvcc = {
            let mut h = vec![0u8; 23];
            h[0] = 1;
            h[21] = 0x03; // lengthSizeMinusOne 3
            h
        };
        let mut ve = vec![0u8; 78];
        ve[24..26].copy_from_slice(&1920u16.to_be_bytes());
        ve[26..28].copy_from_slice(&1080u16.to_be_bytes());
        ve.extend(box_bytes(b"hvcC", &hvcc));
        ve.extend(box_bytes(b"dvvC", &dvvc_body()));
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend(box_bytes(b"dvh1", &ve));
        let entry = parse_stsd(&stsd).unwrap();
        assert_eq!((entry.family, entry.width, entry.height), ("h265/annexb", 1920, 1080));
        assert!(
            matches!(entry.reframer, Reframer::Nal { length_size: 4 }),
            "length prefixed like hvc1"
        );
        let d = entry.colour.dovi.expect("the record");
        assert_eq!(
            (d.profile, d.level, d.rpu, d.el, d.bl, d.compat_id),
            (8, 6, true, false, true, 1)
        );
        for kind in [b"dvcC", b"dvwC"] {
            assert_eq!(visual_colour(&box_bytes(kind, &dvvc_body())).dovi, Some(d));
        }
        assert_eq!(
            visual_colour(&box_bytes(b"dvcC", &[1, 0])).dovi,
            None,
            "a short record states none"
        );
    }

    #[test]
    fn vpcc_and_av1c_state_their_colour_and_siting() {
        let vpcc = [1, 0, 0, 0, 2, 10, 10 << 4 | 1 << 1, 9, 16, 9, 0, 0];
        let c = visual_colour(&box_bytes(b"vpcC", &vpcc));
        assert_eq!(
            (c.matrix, c.transfer, c.primaries),
            (Some(Matrix::Bt2020), Some(TransferFn::Pq), Some(Primaries::Bt2020))
        );
        assert_eq!((c.range, c.chroma), (Some(Range::Limited), Some(ChromaSite::TopLeft)));
        let mut v = vpcc;
        v[6] = 10 << 4 | 1;
        let c = visual_colour(&box_bytes(b"vpcC", &v));
        assert_eq!((c.range, c.chroma), (Some(Range::Full), Some(ChromaSite::Left)));
        let colr = [b"nclx".as_slice(), &[0, 1, 0, 1, 0, 1, 0x80]].concat();
        let both = [box_bytes(b"colr", &colr), box_bytes(b"vpcC", &vpcc)].concat();
        assert_eq!(visual_colour(&both).matrix, Some(Matrix::Bt709));
        let c = visual_colour(&box_bytes(b"av1C", &[0x81, 0x08, 0x0E, 0x00]));
        assert_eq!(c.chroma, Some(ChromaSite::TopLeft));
    }

    fn box_bytes(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn an_absurd_lpcm_channel_count_is_no_pcm_and_no_channels() {
        // 2^26 channels of 32 bits, the frame size overflowed u32 before.
        let e = parse_stsd(&lpcm_stsd(48_000.0, 1 << 26, 32, 0x4)).unwrap();
        assert_eq!((e.pcm, e.channels), (None, 0));
        let e = parse_stsd(&lpcm_stsd(48_000.0, 64, 32, 0x4)).unwrap();
        assert_eq!(e.pcm, Some(Pcm { sample: "s32", frame: 256 }));
    }
}
