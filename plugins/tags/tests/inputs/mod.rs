//! Inputs shared by the parser-level and end-to-end corruption gates.

// test inputs
#![allow(dead_code, clippy::disallowed_methods)]

use super::Format;
use super::fixture as fx;

pub struct Fixture {
    pub name: &'static str,
    pub format: Format,
    pub bytes: Vec<u8>,
    pub fields: Vec<usize>,
}

/// Magics and ids. Each marks a size field next to it and is a token the gate splices in.
pub const MAGICS: &[&[u8]] = &[
    b"RIFF",
    b"WAVE",
    b"fmt ",
    b"data",
    b"LIST",
    b"INFO",
    b"INAM",
    b"IART",
    b"FORM",
    b"AIFF",
    b"AIFC",
    b"COMM",
    b"SSND",
    b"ID3 ",
    b"NAME",
    b"ANNO",
    b"fLaC",
    b"ID3\x03\x00",
    b"ID3\x04\x00",
    b"TAG",
    b"APETAGEX",
    b"TIT2",
    b"TXXX",
    b"APIC",
    b"Xing",
    b"VBRI",
    b"ftyp",
    b"moov",
    b"mdat",
    b"mvhd",
    b"trak",
    b"mdia",
    b"mdhd",
    b"stsd",
    b"udta",
    b"meta",
    b"ilst",
    b"covr",
    b"trkn",
    b"----",
    b"mean",
    b"name",
    b"OggS",
    b"OpusHead",
    b"OpusTags",
    b"\x01vorbis",
    b"\x03vorbis",
    b"\x7fFLAC",
    b"Speex   ",
    b"METADATA_BLOCK_PICTURE=",
    &[0x1A, 0x45, 0xDF, 0xA3],
    &[0x18, 0x53, 0x80, 0x67],
    &[0x11, 0x4D, 0x9B, 0x74],
    &[0x15, 0x49, 0xA9, 0x66],
    &[0x16, 0x54, 0xAE, 0x6B],
    &[0x12, 0x54, 0xC3, 0x67],
    &[0x19, 0x41, 0xA4, 0x69],
    &[0x4D, 0xBB],
    &[0x53, 0xAB],
    &[0x53, 0xAC],
    &[0x1F, 0x43, 0xB6, 0x75],
    b"MAC ",
    b"MACF",
    b"wvpk",
    b"MPCK",
    b"MP+\x07",
    b"SH",
    b"RG",
    b"SE",
];

pub const EXTRA_TOKENS: &[&[u8]] = &[
    &[0, 0, 0, 1],
    &[0, 0, 0, 0],
    &[0, 0, 0, 7],
    &[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    &[0x01, 0, 0, 0, 0, 0, 0, 0],
    &[0x80],
    &[0x80; 9],
    &[0xFF; 16],
    &[0x00; 16],
    &[0xFF, 0xFB, 0x90, 0x00],
    &[0x7F, 0x7F, 0x7F, 0x7F],
    &[0xC0, 0xAF],
    &[0xFF, 0xFE],
    &[0xED, 0xA0, 0x80],
    b"====",
    b"=",
];

pub fn tokens() -> Vec<&'static [u8]> {
    MAGICS.iter().chain(EXTRA_TOKENS).copied().collect()
}

pub fn fields(bytes: &[u8]) -> Vec<usize> {
    let len = bytes.len();
    let mut at: Vec<usize> = (0..64).step_by(4).collect();
    at.extend([6, 10, 18, 22, 26, 27]);
    for magic in MAGICS.iter().filter(|m| m.len() >= 3) {
        let hits = bytes.windows(magic.len()).enumerate().filter(|(_, w)| w == magic);
        for (i, _) in hits.take(4) {
            at.extend([i.saturating_sub(4), i, i + magic.len(), i + magic.len() + 4]);
            at.extend(inner_fields(magic).iter().map(|field| i + field));
        }
    }
    // ID3v1 is the last 128 bytes. An APEv2 footer is the 32 before it or the last 32.
    for back in [128, 160, 148, 144, 32, 20, 16, 1] {
        at.push(len.saturating_sub(back));
    }
    at.retain(|&a| a < len);
    at.sort_unstable();
    at.dedup();
    at
}

fn inner_fields(magic: &[u8]) -> &'static [usize] {
    match magic {
        b"OggS" => &[5, 6, 14, 22, 26, 27],
        b"APETAGEX" => &[8, 12, 16, 20],
        b"wvpk" => &[4, 8, 12, 16, 20, 24, 32, 33],
        b"MAC " => &[8, 12, 52, 56, 60, 64, 68, 70, 72],
        b"COMM" => &[8, 10, 14, 16, 18, 26],
        b"OpusHead" => &[9, 10, 12, 18],
        b"OpusTags" => &[8, 12],
        b"\x7fFLAC" => &[7, 9, 13, 14],
        _ => &[],
    }
}

pub fn fix_ogg_crcs(bytes: &mut [u8]) {
    let mut at = 0;
    while at + 27 <= bytes.len() {
        if &bytes[at..at + 4] != b"OggS" {
            at += 1;
            continue;
        }
        let segments = bytes[at + 26] as usize;
        let Some(table) = bytes.get(at + 27..at + 27 + segments) else {
            break;
        };
        let end = at + 27 + segments + table.iter().map(|&b| b as usize).sum::<usize>();
        if end > bytes.len() {
            at += 4;
            continue;
        }
        let crc = pf_ogg::page::page_crc(&bytes[at..end]);
        bytes[at + 22..at + 26].copy_from_slice(&crc.to_le_bytes());
        at = end;
    }
}

pub fn ogg_paged(packets: &[&[u8]], per_page: usize, last_granule: u64) -> Vec<u8> {
    use pf_ogg::page::{GRANULE_NONE, flags, write_page};
    let mut out = Vec::new();
    let mut sequence = 0u32;
    for (n, packet) in packets.iter().enumerate() {
        let mut lacing = vec![255u8; packet.len() / 255];
        lacing.push((packet.len() % 255) as u8);
        let pages = lacing.len().div_ceil(per_page);
        let mut at = 0;
        for (i, table) in lacing.chunks(per_page).enumerate() {
            let size: usize = table.iter().map(|&b| b as usize).sum();
            let mut kind = if i > 0 { flags::CONTINUED } else { 0 };
            if n == 0 && i == 0 {
                kind |= flags::BOS;
            }
            let last = i + 1 == pages;
            if last && n + 1 == packets.len() {
                kind |= flags::EOS;
            }
            let granule = match (last, n + 1 == packets.len()) {
                (true, true) => last_granule,
                (true, false) => 0,
                _ => GRANULE_NONE,
            };
            write_page(
                &mut out,
                kind,
                granule,
                0x0DD5_E41A,
                sequence,
                table,
                &packet[at..at + size],
            );
            sequence += 1;
            at += size;
        }
    }
    out
}

fn opus_packets(comments: &[&str]) -> (Vec<u8>, Vec<u8>) {
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2]);
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&fx::vorbis_comment(comments));
    (head, tags)
}

fn picture(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

pub fn fixtures() -> Vec<Fixture> {
    let info = [("INAM", "Inline Title"), ("IART", "Bj\u{f6}rk"), ("ITRK", "7"), ("ICMT", "odd")];
    let v2 = [
        ("TIT2", "Head Title"),
        ("TPE1", "Head Artist"),
        ("TXXX", "replaygain_track_gain=-7.25 dB"),
    ];
    let ape = [("Title", "Ape Title"), ("Replaygain_Track_Gain", "-6.75 dB")];
    let v1 = fx::id3v1("V1 Title", "V1 Artist", "V1 Album", "1996", 2);
    let art = picture(700);
    let comments = ["TITLE=Comment Title", "ARTIST=Comment Artist", "TRACKNUMBER=3"];
    let (opus_head, opus_tags) = opus_packets(&comments);
    let long_value = format!("COMMENT={}", "x".repeat(70 * 255));
    let (_, opus_long) = opus_packets(&["TITLE=Dropped", &long_value]);
    let (_, opus_spanning) = opus_packets(&["TITLE=Spanning", &long_value[..30 * 255]]);
    let id3_prefix = fx::id3v2(4, &[("TIT2", "Prefixed")], None);

    let set: Vec<(&'static str, Format, Vec<u8>)> = vec![
        ("wav_inline_info", Format::Wav, fx::wav(8_000, 1, 8, 300, &info, false)),
        ("wav_tail_info", Format::Wav, fx::wav(44_100, 2, 16, 400, &info, true)),
        (
            "aiff_text",
            Format::Aiff,
            fx::aiff(
                b"AIFF",
                44_100,
                2,
                16,
                150,
                &[("NAME", "Text Title"), ("AUTH", "Author"), ("ANNO", "note")],
                None,
            ),
        ),
        (
            "aifc_sowt",
            Format::Aiff,
            fx::aiff(b"AIFC", 22_050, 1, 16, 150, &[("NAME", "Compressed")], Some(*b"sowt")),
        ),
        (
            "aiff_tail_id3",
            Format::Aiff,
            fx::aiff_id3(
                44_100,
                2,
                300,
                &[("NAME", "Text Title")],
                &[("TIT2", "Tag Title"), ("TRCK", "5")],
                true,
            ),
        ),
        ("flac_plain", Format::Flac, fx::flac(48_000, 2, 24, 96_000, &comments, None)),
        (
            "flac_art",
            Format::Flac,
            fx::flac(44_100, 1, 16, 22_050, &["TITLE=Art"], Some(("image/jpeg", &art))),
        ),
        (
            "flac_behind_id3",
            Format::Flac,
            [id3_prefix.clone(), fx::flac(44_100, 2, 16, 44_100, &comments, None)].concat(),
        ),
        (
            "mp3_all_tags",
            Format::Mp3,
            fx::mp3(
                44_100,
                128,
                3,
                &v2,
                None,
                true,
                Some(("V1 Title", "V1 Artist", "V1 Album", "1998", 9)),
                &ape,
            ),
        ),
        (
            "mp3_art",
            Format::Mp3,
            fx::mp3(
                44_100,
                128,
                2,
                &[("TIT2", "Art")],
                Some(("image/png", &art)),
                false,
                None,
                &[],
            ),
        ),
        ("mp3_bare", Format::Mp3, fx::mp3(32_000, 64, 3, &[], None, false, None, &[])),
        (
            "m4a_faststart",
            Format::Mp4,
            fx::m4a(
                44_100,
                2,
                88_200,
                &[("\u{a9}nam", "Fast Title"), ("\u{a9}ART", "Fast Artist")],
                Some(3),
                Some(("image/png", &art)),
                true,
            ),
        ),
        (
            "m4a_tail_moov",
            Format::Mp4,
            fx::m4a(48_000, 1, 144_000, &[("\u{a9}nam", "Tail Moov")], None, None, false),
        ),
        (
            "ogg_opus",
            Format::OggOpus,
            ogg_paged(&[&opus_head, &opus_tags, &[0xFC; 160]], 255, 48_000 * 5),
        ),
        (
            "ogg_opus_art",
            Format::OggOpus,
            fx::ogg_opus(2, 312, 44_100, &comments, Some(("image/jpeg", &art)), 48_000 * 5 + 312),
        ),
        (
            "ogg_opus_spanning",
            Format::OggOpus,
            ogg_paged(&[&opus_head, &opus_spanning, &[0xFC; 60]], 1, 48_000),
        ),
        (
            "ogg_opus_dropped",
            Format::OggOpus,
            ogg_paged(&[&opus_head, &opus_long, &[0xFC; 60]], 1, 48_000),
        ),
        (
            "ogg_opus_two_per_page",
            Format::OggOpus,
            ogg_paged(&[&opus_head, &opus_tags, &[0xFC; 600]], 2, 48_000),
        ),
        ("ogg_vorbis", Format::OggVorbis, fx::ogg_vorbis(2, 44_100, &comments, 44_100 * 4)),
        ("ogg_speex", Format::OggSpeex, fx::ogg_speex(16_000, 1, &comments, None, 16_000 * 3)),
        (
            "ogg_flac_art",
            Format::OggFlac,
            fx::ogg_flac(48_000, 2, 288_000, &comments, Some(("image/jpeg", &art))),
        ),
        (
            "mkv_front",
            Format::Mkv,
            fx::mkv(
                48_000.0,
                2,
                3_000.0,
                &[("TITLE", "WebM Title"), ("PART_NUMBER", "9")],
                Some(("image/png", &art)),
            ),
        ),
        (
            "mkv_trailing",
            Format::Mkv,
            fx::mkv_trailing(
                44_100.0,
                1,
                1_250.0,
                &[("TITLE", "Trailing"), ("ALBUM", "Album")],
                Some(("image/jpeg", &art)),
                1500,
            ),
        ),
        (
            "ape_modern",
            Format::Ape,
            [
                fx::ape_file(3990, 44_100, 2, 73_728, 3, 4_644),
                vec![0x71; 300],
                fx::ape_tag(&ape),
                v1.clone(),
            ]
            .concat(),
        ),
        (
            "ape_old",
            Format::Ape,
            [fx::ape_file_old(3950, 2000, 0, 44_100, 2, 2, 294_912), fx::ape_tag(&ape)].concat(),
        ),
        (
            "wavpack",
            Format::WavPack,
            [
                fx::wavpack_block(0x410, 0, 0, 0, 0, &[fx::wv_sub_block(0x21, &[1, 2, 3])]),
                fx::wavpack(
                    12_345,
                    6,
                    88_200,
                    &[
                        fx::wv_sub_block(0x27, &12_345u32.to_le_bytes()[..3]),
                        fx::wv_sub_block(0x0D, &[6, 0x3F, 0, 0]),
                        fx::wv_sub_block(0x0A, &[0x33; 600]),
                    ],
                ),
                fx::ape_tag(&ape),
                v1.clone(),
            ]
            .concat(),
        ),
        (
            "mpc_sv8",
            Format::Musepack,
            [
                fx::mpc_sv8(0, 2, 88_200, 1_024, &[(*b"RG", vec![1; 9])]),
                vec![0x2E; 300],
                fx::ape_tag(&ape),
            ]
            .concat(),
        ),
        (
            "mpc_sv7",
            Format::Musepack,
            [fx::mpc_sv7(1, 100, true, 500), vec![0x2E; 200], fx::ape_tag(&ape), v1].concat(),
        ),
        (
            "mpc_behind_id3",
            Format::Musepack,
            [id3_prefix, fx::mpc_sv8(1, 2, 48_000, 0, &[]), fx::ape_tag(&ape)].concat(),
        ),
    ];
    set.into_iter()
        .map(|(name, format, bytes)| Fixture { name, format, fields: fields(&bytes), bytes })
        .collect()
}

pub fn of<'a>(all: &'a [Fixture], formats: &[Format]) -> Vec<&'a Fixture> {
    all.iter().filter(|f| formats.contains(&f.format)).collect()
}
