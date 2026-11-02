// Tests own their staging Vecs.
#![allow(clippy::disallowed_methods)]

use super::*;

#[test]
fn annex_b_loses_its_sets_and_delimiters() {
    let au = [
        0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 0xAA, 0xBB,
    ];
    let mut out = Vec::new();
    length_prefixed(NalCodec::H264, &au, &mut out);
    assert_eq!(out, vec![0, 0, 0, 3, 0x65, 0xAA, 0xBB]);
    assert!(has_picture(NalCodec::H264, &au));
    assert!(!has_picture(NalCodec::H264, &au[..19]), "sets alone are a head");
}

#[test]
fn families_name_their_nal_codec() {
    assert_eq!(nal_codec("h264/annexb"), Some(NalCodec::H264));
    assert_eq!(nal_codec("h265/annexb"), Some(NalCodec::H265));
    assert_eq!(nal_codec("vp9"), None);
}

#[test]
fn a_head_without_sets_is_no_entry() {
    let pps = [0, 0, 0, 1, 0x68, 3];
    assert_eq!(nal_entry(NalCodec::H264, &pps), Err(EntryError::ParamSets(ConfigError::NoSps)));
}

#[test]
fn codec_names_follow_rfc_6381() {
    assert_eq!(
        codecs(&Entry::Avc(vec![1, 0x64, 0x00, 0x28, 0xFF])).as_deref(),
        Some("avc1.640028")
    );
    assert_eq!(codecs(&Entry::Aac(vec![0x11, 0x90])).as_deref(), Some("mp4a.40.2"));
    assert_eq!(codecs(&Entry::Opus(vec![])).as_deref(), Some("opus"));
    assert_eq!(codecs(&Entry::Tx3g), None);
    // Main, compatibility 0x60000000, level 3.1, progressive and frame only flags
    let mut r = vec![1, 0x01, 0x60, 0, 0, 0, 0xB0, 0, 0, 0, 0, 0, 93];
    r.extend_from_slice(&[0; 10]);
    let hevc = |inband| codecs(&Entry::Hevc { record: r.clone(), inband });
    assert_eq!(hevc(false).as_deref(), Some("hvc1.1.6.L93.B0"));
    assert_eq!(hevc(true).as_deref(), Some("hev1.1.6.L93.B0"));
}

/// Fields of (value, width) packed big endian, padded with zero bits.
fn pack(fields: &[(u32, usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    for &(v, n) in fields {
        for i in (0..n).rev() {
            if at % 8 == 0 {
                out.push(0);
            }
            let last = out.len() - 1;
            out[last] |= ((v >> i & 1) as u8) << (7 - at % 8);
            at += 1;
        }
    }
    out.extend_from_slice(&[0; 4]);
    out
}

/// A keyframe header up to its colour config, profile split into its two bits.
fn key(profile: u32, colour: &[(u32, usize)]) -> Vec<u8> {
    let mut f = vec![(2, 2), (profile & 1, 1), (profile >> 1, 1)];
    if profile == 3 {
        f.push((0, 1));
    }
    f.extend_from_slice(&[(0, 1), (0, 1), (1, 1), (0, 1), (0x49_8342, 24)]);
    f.extend_from_slice(colour);
    pack(&f)
}

#[test]
fn vp9_keyframes_give_profile_depth_chroma_and_range() {
    let h = vp9_head(&key(0, &[(2, 3), (0, 1)])).expect("profile 0");
    assert_eq!(h, Vp9Head { profile: 0, depth: 8, chroma: 1, full: false, space: 2 });
    let h = vp9_head(&key(2, &[(1, 1), (5, 3), (1, 1)])).expect("profile 2");
    assert_eq!((h.profile, h.depth, h.chroma, h.full), (2, 12, 1, true));
    let h = vp9_head(&key(1, &[(1, 3), (0, 1), (1, 1), (0, 1), (0, 1)])).expect("4:2:2");
    assert_eq!((h.profile, h.chroma), (1, 2));
    let h = vp9_head(&key(3, &[(0, 1), (7, 3), (0, 1)])).expect("RGB");
    assert_eq!((h.depth, h.chroma, h.full, h.space), (10, 3, true, 7));
    // an inter frame, frame_type 1
    let inter = pack(&[(2, 2), (0, 2), (0, 1), (1, 1), (1, 1), (0, 1)]);
    assert_eq!(vp9_head(&inter), None);
    assert_eq!(vp9_head(&[]), None);
}

#[test]
fn a_superframe_finds_its_keyframe_behind_a_hidden_frame() {
    let hidden = pack(&[(2, 2), (0, 2), (0, 1), (1, 1), (0, 1), (0, 1)]);
    let k = key(0, &[(1, 3), (0, 1)]);
    let mut sf = hidden.clone();
    sf.extend_from_slice(&k);
    sf.extend_from_slice(&[0xC1, hidden.len() as u8, k.len() as u8, 0xC1]);
    let h = vp9_head(&sf).expect("the second frame");
    assert_eq!(h.space, 1);
    // sizes past the data are no frames at all
    let mut bad = k.clone();
    bad.extend_from_slice(&[0xC1, 200, 200, 0xC1]);
    assert_eq!(vp9_head(&bad), None);
}

#[test]
fn vp9_levels_follow_size_and_rate() {
    assert_eq!(vp9_level(320, 180, 25_000), 11);
    assert_eq!(vp9_level(1920, 1080, 23_976), 40);
    assert_eq!(vp9_level(3840, 2160, 60_000), 51);
    assert_eq!(vp9_level(1920, 1080, 0), 40, "size alone without a rate");
    assert_eq!(vp9_level(0, 0, 25_000), 0);
}

#[test]
fn a_vp9_entry_takes_the_matrix_from_the_bitstream_when_unspecified() {
    let m = Vp9Meta { width: 1920, height: 1080, fps_milli: 24_000, colour: [2, 2, 2, 0] };
    let e = vp9_entry(&key(0, &[(2, 3), (0, 1)]), &m).expect("entry");
    assert_eq!(e, Entry::Vp9(vec![0, 40, 0x82, 2, 2, 1, 0, 0]));
    assert_eq!(codecs(&e).as_deref(), Some("vp09.00.40.08"));
    let hdr = Vp9Meta { colour: [9, 9, 16, 1], ..m };
    let e = vp9_entry(&key(2, &[(0, 1), (5, 3), (0, 1)]), &hdr).expect("entry");
    assert_eq!(codecs(&e).as_deref(), Some("vp09.02.40.10.01.09.16.09.00"));
    assert_eq!(vp9_entry(&[0x80, 0, 0], &m), Err(EntryError::Vp9Head));
}

#[test]
fn av1_takes_the_matroska_record_and_drops_the_delimiter() {
    // profile 0, level 4.0, main tier, 10 bit 4:2:0
    let rec = [0x81, 0x08, 0x4C, 0x00, 0x0A, 0x0B, 0, 0, 0, 0x0A];
    let e = av1_entry(&rec, &[]).expect("entry");
    assert_eq!(e, Entry::Av1(rec.to_vec()));
    assert_eq!(codecs(&e).as_deref(), Some("av01.0.08M.10"));
    assert_eq!(av1_entry(&[], &[0x12, 0x00]), Err(EntryError::Av1(ConfigError::NoSequenceHeader)));
    let tu = [0x12, 0x00, 0x32, 0x01, 0xAA];
    assert_eq!(av1_sample(&tu), &tu[2..]);
    assert_eq!(av1_sample(&tu[2..]), &tu[2..], "no delimiter, as it is");
    assert_eq!(av1_sample(&[0x12, 0x00]), &[0x12, 0x00], "a lone delimiter stays");
}
