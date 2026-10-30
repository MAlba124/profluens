//! `probe_props` over hand-built MPEG audio frames with Xing, VBRI or no VBR header.

// Test fixtures allocate freely.
#![allow(clippy::disallowed_methods)]

use pf_mp3::probe_props;

const MPEG1: u8 = 0xFB;
const MPEG1_CRC: u8 = 0xFA;
const MPEG2: u8 = 0xF3;

const STEREO: u8 = 0b00;
const MONO: u8 = 0b11;

fn header(version_byte: u8, bitrate_index: u8, samplerate_index: u8, mode: u8) -> [u8; 4] {
    [0xFF, version_byte, (bitrate_index << 4) | (samplerate_index << 2), mode << 6]
}

fn frame(head: [u8; 4], len: usize, tag_at: usize, tag: &[u8]) -> Vec<u8> {
    let mut f = head.to_vec();
    f.resize(len, 0);
    f[tag_at..tag_at + tag.len()].copy_from_slice(tag);
    f
}

fn xing(frames: u32) -> Vec<u8> {
    let mut b = b"Xing".to_vec();
    b.extend_from_slice(&1u32.to_be_bytes());
    b.extend_from_slice(&frames.to_be_bytes());
    b
}

fn lame_ext(writer: &[u8], delay: u32, padding: u32) -> [u8; 36] {
    let mut e = [0u8; 36];
    e[..writer.len()].copy_from_slice(writer);
    let v = (delay << 12) | padding;
    e[0x15..0x18].copy_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8]);
    e
}

fn xing_full(frames: u32, ext: &[u8]) -> Vec<u8> {
    let mut b = b"Xing".to_vec();
    b.extend_from_slice(&0xFu32.to_be_bytes());
    b.extend_from_slice(&frames.to_be_bytes());
    b.extend_from_slice(&999_999u32.to_be_bytes()); // byte count
    b.extend_from_slice(&[0u8; 100]); // seek TOC
    b.extend_from_slice(&57u32.to_be_bytes()); // quality
    b.extend_from_slice(ext);
    b
}

fn vbri(frames: u32) -> Vec<u8> {
    let mut b = b"VBRI".to_vec();
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&frames.to_be_bytes());
    b
}

fn duration_ns(frames: u64, samples_per_frame: u64, rate: u64) -> u64 {
    let ns = u128::from(frames) * u128::from(samples_per_frame) * 1_000_000_000 / u128::from(rate);
    ns.min(u128::from(u64::MAX)) as u64
}

fn samples_ns(samples: u64, rate: u64) -> u64 {
    (u128::from(samples) * 1_000_000_000 / u128::from(rate)) as u64
}

const MPEG1_128_LEN: usize = 417;
const MPEG2_64_LEN: usize = 208;

#[test]
fn xing_frame_count_gives_an_exact_duration() {
    // MPEG-1 stereo side info is 32 bytes, so Xing begins at 4 + 32.
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing(100));
    let p = probe_props(&f, MPEG1_128_LEN as u64).expect("first frame must be found");

    assert_eq!(p.sample_rate, 44_100);
    assert_eq!(p.channels, 2);
    assert_eq!(p.bitrate_kbps, Some(128));
    assert!(p.exact, "a frame count is exact, not an estimate");
    assert_eq!(p.duration_ns, Some(duration_ns(100, 1152, 44_100)));
    assert_eq!(p.duration_ns, Some(2_612_244_897));
}

#[test]
fn xing_offset_follows_the_side_info_width() {
    // Mono MPEG-1 side info is 17 bytes, so Xing moves to 4 + 17.
    let f = frame(header(MPEG1, 9, 0, MONO), MPEG1_128_LEN, 21, &xing(20));
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();
    assert_eq!(p.channels, 1);
    assert!(p.exact);
    assert_eq!(p.duration_ns, Some(duration_ns(20, 1152, 44_100)));

    let f = frame(header(MPEG2, 8, 0, STEREO), MPEG2_64_LEN, 21, &xing(50));
    let p = probe_props(&f, MPEG2_64_LEN as u64).unwrap();
    assert_eq!(p.sample_rate, 22_050);
    assert_eq!(p.channels, 2);
    assert_eq!(p.bitrate_kbps, Some(64));
    assert!(p.exact);
    assert_eq!(p.duration_ns, Some(duration_ns(50, 576, 22_050)));
}

#[test]
fn a_crc_protected_first_frame_shifts_the_xing_offset_by_two() {
    // The CRC word sits between the header and the side info.
    let f = frame(header(MPEG1_CRC, 9, 0, STEREO), MPEG1_128_LEN, 38, &xing(7));
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();
    assert!(p.exact);
    assert_eq!(p.duration_ns, Some(duration_ns(7, 1152, 44_100)));

    let f = frame(header(MPEG1_CRC, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing(7));
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();
    assert!(!p.exact);
}

#[test]
fn a_lame_extension_trims_the_encoder_delay_and_padding() {
    let ext = lame_ext(b"LAME3.99r", 576, 1000);
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing_full(100, &ext));
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();

    assert!(p.exact, "the encoder stated both figures; nothing here is estimated");
    assert_eq!(p.duration_ns, Some(samples_ns(100 * 1152 - 576 - 1000, 44_100)));
    assert_eq!(p.duration_ns, Some(2_576_507_936));
    assert_eq!(duration_ns(100, 1152, 44_100) - p.duration_ns.unwrap(), 35_736_961);

    let ext = lame_ext(b"Lavc60.31", 4095, 4095);
    let f = frame(header(MPEG2, 8, 0, STEREO), MPEG2_64_LEN, 21, &xing_full(1000, &ext));
    let p = probe_props(&f, MPEG2_64_LEN as u64).unwrap();
    assert_eq!(p.duration_ns, Some(samples_ns(1000 * 576 - 8190, 22_050)));
}

#[test]
fn the_lame_extension_sits_after_whatever_the_flags_announce() {
    let mut x = xing(100);
    x.extend_from_slice(&lame_ext(b"Lavf61.7.", 1105, 288));
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &x);
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();
    assert_eq!(p.duration_ns, Some(samples_ns(100 * 1152 - 1105 - 288, 44_100)));

    let mut x = xing(100);
    x.resize(120, 0);
    x.extend_from_slice(&lame_ext(b"Lavf61.7.", 1105, 288));
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &x);
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();
    assert_eq!(p.duration_ns, Some(duration_ns(100, 1152, 44_100)));
}

#[test]
fn a_xing_without_a_usable_lame_extension_is_left_untrimmed() {
    let coded = Some(duration_ns(100, 1152, 44_100));

    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing_full(100, &[]));
    assert_eq!(probe_props(&f, MPEG1_128_LEN as u64).unwrap().duration_ns, coded);

    let junk: [&[u8]; 3] =
        [b"\0\0\0\0\0\0\0\0\0", b"Xingified", b"\xff\xff\xff\xff\xff\xff\xff\xff"];
    for writer in junk {
        let ext = lame_ext(writer, 4095, 4095);
        let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing_full(100, &ext));
        let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();
        assert_eq!(p.duration_ns, coded, "trimmed on writer {writer:?}");
        assert!(p.exact);
    }

    let ext = lame_ext(b"LAME3.99r", 4095, 4095);
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing_full(2, &ext));
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();
    assert_eq!(p.duration_ns, Some(duration_ns(2, 1152, 44_100)));

    let ext = lame_ext(b"LAME3.99r", 1000, 152);
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing_full(1, &ext));
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();
    assert_eq!(p.duration_ns, Some(duration_ns(1, 1152, 44_100)));
}

#[test]
fn vbri_frame_count_gives_an_exact_duration() {
    let f = frame(header(MPEG1, 9, 0, MONO), MPEG1_128_LEN, 36, &vbri(365));
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();

    assert_eq!(p.channels, 1);
    assert!(p.exact);
    assert_eq!(p.duration_ns, Some(duration_ns(365, 1152, 44_100)));
}

#[test]
fn a_plain_cbr_stream_estimates_from_bitrate_and_byte_count() {
    let mut audio = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 4, &[]);
    audio.extend_from_slice(&frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 4, &[]));
    assert_eq!(audio.len(), 834);

    let p = probe_props(&audio, audio.len() as u64).unwrap();
    assert_eq!(p.sample_rate, 44_100);
    assert_eq!(p.channels, 2);
    assert_eq!(p.bitrate_kbps, Some(128));
    assert!(!p.exact, "a bitrate estimate is not exact");
    assert_eq!(p.duration_ns, Some(52_125_000));

    let p = probe_props(&audio, 5_000_000).unwrap();
    assert_eq!(p.duration_ns, Some(312_500_000_000)); // 5 MB at 128 kbps is about 312.5 s
}

#[test]
fn a_window_too_short_for_the_whole_frame_still_yields_format_and_an_estimate() {
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing(100));
    let p = probe_props(&f[..64], 5_000_000).unwrap();

    assert_eq!(p.sample_rate, 44_100);
    assert_eq!(p.channels, 2);
    assert_eq!(p.bitrate_kbps, Some(128));
    assert!(!p.exact);
    assert_eq!(p.duration_ns, Some(312_500_000_000));
}

#[test]
fn leading_junk_before_the_first_sync_is_skipped() {
    let mut audio = vec![0x00, 0x11, 0x22, 0xFF, 0x13, 0x44];
    audio.extend_from_slice(&frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing(100)));

    let p = probe_props(&audio, audio.len() as u64).unwrap();
    assert!(p.exact);
    assert_eq!(p.duration_ns, Some(duration_ns(100, 1152, 44_100)));
}

#[test]
fn audio_without_a_frame_sync_probes_to_none() {
    assert!(probe_props(&[], 0).is_none());
    assert!(probe_props(b"not an mpeg stream at all", 25).is_none());
    assert!(probe_props(&[0xFF, 0xFB, 0xFC, 0x00, 0x00, 0x00], 6).is_none());
}

#[test]
fn truncated_and_corrupt_windows_never_panic() {
    let lame = xing_full(100, &lame_ext(b"LAME3.99r", 576, 1000));
    let fixtures = [
        frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing(100)),
        frame(header(MPEG1, 9, 0, MONO), MPEG1_128_LEN, 36, &vbri(365)),
        frame(header(MPEG2, 8, 0, STEREO), MPEG2_64_LEN, 21, &xing(50)),
        frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &lame),
    ];
    for f in &fixtures {
        for n in 0..=f.len() {
            probe_props(&f[..n], f.len() as u64);
            probe_props(&f[..n], u64::MAX); // the estimate must not overflow either
        }
        for i in 0..f.len() {
            for byte in [0x00u8, 0x7F, 0x80, 0xFF] {
                let mut corrupt = f.clone();
                corrupt[i] = byte;
                probe_props(&corrupt, f.len() as u64);
            }
        }
    }
    // `frames * 1152 * 10^9` overflows a `u64`, so the multiply must be wide.
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing(u32::MAX));
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();
    assert_eq!(p.duration_ns, Some(duration_ns(u64::from(u32::MAX), 1152, 44_100)));

    let cbr = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 4, &[]);
    assert_eq!(probe_props(&cbr, u64::MAX).unwrap().duration_ns, Some(u64::MAX));
}

fn xing_fields(
    flags: u32,
    frames: u32,
    bytes: u32,
    toc: Option<&[u8; 100]>,
    ext: &[u8],
) -> Vec<u8> {
    let mut b = b"Xing".to_vec();
    b.extend_from_slice(&flags.to_be_bytes());
    if flags & 0x1 != 0 {
        b.extend_from_slice(&frames.to_be_bytes());
    }
    if flags & 0x2 != 0 {
        b.extend_from_slice(&bytes.to_be_bytes());
    }
    if flags & 0x4 != 0 {
        b.extend_from_slice(toc.expect("flags announced a TOC"));
    }
    if flags & 0x8 != 0 {
        b.extend_from_slice(&57u32.to_be_bytes());
    }
    b.extend_from_slice(ext);
    b
}

fn linear_toc() -> [u8; 100] {
    let mut t = [0u8; 100];
    for (i, e) in t.iter_mut().enumerate() {
        *e = (i * 256 / 100) as u8;
    }
    t
}

#[test]
fn xing_toc_is_read_with_the_byte_count_in_front_of_it() {
    let toc = linear_toc();
    let x = xing_fields(0xF, 1000, 4_000_000, Some(&toc), &lame_ext(b"LAME3.99r", 576, 1000));
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &x);
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();

    assert_eq!(p.toc.expect("flags bit 2 announced a TOC").points, toc);
    assert_eq!(p.stream_bytes, Some(4_000_000), "flags bit 1's byte count");
    assert!(p.exact);
    assert_eq!(p.duration_ns, Some(samples_ns(1000 * 1152 - 1576, 44_100)));
}

#[test]
fn the_toc_walk_follows_the_flags_not_a_fixed_offset() {
    let toc = linear_toc();
    // Without a byte count the table begins four bytes earlier.
    let x = xing_fields(0x5, 500, 0, Some(&toc), &[]);
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &x);
    let p = probe_props(&f, MPEG1_128_LEN as u64).unwrap();
    assert_eq!(p.toc.expect("TOC present").points, toc);
    assert_eq!(p.stream_bytes, None, "no byte count was written");

    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &xing(500));
    assert_eq!(probe_props(&f, MPEG1_128_LEN as u64).unwrap().toc, None);
}

#[test]
fn toc_entries_map_time_hundredths_to_byte_two_hundred_fifty_sixths() {
    let toc = pf_mp3::XingToc { points: linear_toc() };
    const STREAM: u64 = 4_000_000;
    const DURATION: u64 = 200_000_000_000; // 200 s

    assert_eq!(toc.byte_at(0, STREAM), Some(0));
    assert_eq!(toc.time_at(0, DURATION), Some(0));
    assert_eq!(toc.time_at(50, DURATION), Some(100_000_000_000));
    assert_eq!(toc.byte_at(50, STREAM), Some(STREAM / 2));
    assert_eq!(toc.time_at(99, DURATION), Some(198_000_000_000));
    assert_eq!(toc.byte_at(99, STREAM), Some(253 * STREAM / 256));
    assert_eq!(toc.byte_at(100, STREAM), None);
    assert_eq!(toc.time_at(100, DURATION), None);
    assert_eq!(toc.byte_at(usize::MAX, STREAM), None);
    assert_eq!(toc.time_at(usize::MAX, DURATION), None);
    assert_eq!(toc.byte_at(99, 0), Some(0));
    assert_eq!(toc.time_at(99, 0), Some(0));
    assert_eq!(
        pf_mp3::XingToc { points: [255; 100] }.byte_at(0, u64::MAX),
        Some(u64::MAX / 256 * 255 + 255 * 255 / 256)
    );
    assert!(toc.time_at(99, u64::MAX).is_some());
}

#[test]
fn first_frame_offset_anchors_the_toc_to_the_file() {
    let toc = linear_toc();
    let x = xing_fields(0xF, 1000, 4_000_000, Some(&toc), &[]);
    let mut audio = vec![0x00, 0x11, 0x22, 0x33];
    audio.extend_from_slice(&frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &x));

    let p = probe_props(&audio, audio.len() as u64).unwrap();
    assert_eq!(p.first_frame_offset, 4);
    assert!(p.toc.is_some());
    let clean = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &x);
    assert_eq!(probe_props(&clean, clean.len() as u64).unwrap().first_frame_offset, 0);
}

#[test]
fn a_toc_the_frame_is_too_short_to_hold_is_absent_not_partial() {
    // A 104 byte frame whose Xing flags claim 137 bytes of fields. The table must be missing.
    let x = xing_fields(0xF, 1000, 4_000_000, Some(&linear_toc()), &[]);
    let mut audio = header(MPEG1, 1, 0, MONO).to_vec();
    audio.resize(21, 0); // MPEG-1 mono side info is 17 bytes, so Xing begins at 4 + 17
    audio.extend_from_slice(&x);
    audio.truncate(104); // the frame ends here, mid-table
    audio.extend_from_slice(&header(MPEG1, 1, 0, MONO)); // the next syncword confirms it
    audio.resize(208, 0);

    let p = probe_props(&audio, audio.len() as u64).expect("the first frame is confirmed");
    assert_eq!(p.toc, None, "a table the frame is too short to hold is not read");
    assert_eq!(p.stream_bytes, Some(4_000_000), "the fields in front of it still are");
    assert!(p.exact);
    assert_eq!(p.duration_ns, Some(duration_ns(1000, 1152, 44_100)));

    let toc = linear_toc();
    let full = frame(
        header(MPEG1, 9, 0, STEREO),
        MPEG1_128_LEN,
        36,
        &xing_fields(0xF, 1000, 4_000_000, Some(&toc), &[]),
    );
    for cut in 0..full.len() {
        if let Some(p) = probe_props(&full[..cut], full.len() as u64) {
            assert_eq!(p.toc, None, "a partial window at cut {cut} must not be believed");
        }
    }
    assert_eq!(probe_props(&full, full.len() as u64).unwrap().toc.map(|t| t.points), Some(toc));
}

#[test]
fn toc_reading_never_panics_on_a_corrupt_block() {
    let x = xing_fields(
        0xF,
        1000,
        4_000_000,
        Some(&linear_toc()),
        &lame_ext(b"Lavf61.7.100", 1105, 940),
    );
    let f = frame(header(MPEG1, 9, 0, STEREO), MPEG1_128_LEN, 36, &x);
    for n in 0..=f.len() {
        if let Some(p) = probe_props(&f[..n], f.len() as u64)
            && let Some(t) = p.toc
        {
            for i in 0..=101 {
                let _ = t.byte_at(i, p.stream_bytes.unwrap_or(0) as u64);
                let _ = t.time_at(i, p.duration_ns.unwrap_or(0));
            }
        }
    }
    for i in 0..f.len() {
        for byte in [0x00u8, 0x7F, 0x80, 0xFF] {
            let mut corrupt = f.clone();
            corrupt[i] = byte;
            let _ = probe_props(&corrupt, f.len() as u64);
        }
    }
}

#[test]
fn a_vbr_tag_frame_is_found_as_ffmpeg_skips_it() {
    use pf_mp3::{TAG_FRAME_PROBE, TagFrame, tag_frame};
    let stereo = header(MPEG1, 9, 0, STEREO);
    assert_eq!(tag_frame(&frame(stereo, 417, 36, &xing(100)), false), TagFrame::Frame(417));
    let mut info = xing(0);
    info[..4].copy_from_slice(b"Info");
    assert_eq!(
        tag_frame(&frame(stereo, 417, 36, &info), false),
        TagFrame::Not,
        "no count and no size"
    );
    let mut sized = b"Info".to_vec();
    sized.extend_from_slice(&2u32.to_be_bytes());
    sized.extend_from_slice(&5000u32.to_be_bytes());
    assert_eq!(tag_frame(&frame(stereo, 417, 36, &sized), false), TagFrame::Frame(417));
    assert_eq!(
        tag_frame(&frame(header(MPEG1, 9, 0, MONO), 417, 21, &xing(9)), false),
        TagFrame::Frame(417)
    );
    assert_eq!(
        tag_frame(&frame(header(MPEG2, 8, 1, STEREO), 192, 21, &xing(9)), false),
        TagFrame::Frame(192)
    );
    assert_eq!(
        tag_frame(&frame(header(MPEG2, 8, 1, MONO), 192, 13, &xing(9)), false),
        TagFrame::Frame(192)
    );
    assert_eq!(
        tag_frame(&frame(header(MPEG1_CRC, 9, 0, STEREO), 417, 38, &xing(100)), false),
        TagFrame::Not
    );
    let mut vbri = b"VBRI\x00\x01\x00\x00\x00\x00".to_vec();
    vbri.extend_from_slice(&0u32.to_be_bytes());
    vbri.extend_from_slice(&77u32.to_be_bytes());
    assert_eq!(tag_frame(&frame(stereo, 417, 36, &vbri), false), TagFrame::Frame(417));
    vbri[5] = 2;
    assert_eq!(
        tag_frame(&frame(stereo, 417, 36, &vbri), false),
        TagFrame::Not,
        "another VBRI version"
    );
    assert_eq!(tag_frame(&frame(stereo, 417, 0, &[]), false), TagFrame::Not);
    assert_eq!(
        tag_frame(&frame([0xFF, 0xFD, 0x90, 0], 417, 36, &xing(100)), false),
        TagFrame::Not,
        "layer II"
    );
    assert_eq!(
        tag_frame(&frame(header(MPEG1, 0, 0, STEREO), 417, 36, &xing(100)), false),
        TagFrame::Not,
        "free format"
    );
    let whole = frame(stereo, 417, 36, &xing(100));
    assert_eq!(tag_frame(&whole[..TAG_FRAME_PROBE - 1], false), TagFrame::Need);
    assert_eq!(
        tag_frame(&whole[..48], true),
        TagFrame::Frame(417),
        "the count read before the end"
    );
}
