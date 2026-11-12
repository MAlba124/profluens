// Test tables collected once.
#![allow(clippy::disallowed_methods)]

use pf_registry::lookup::is_coded;
use pf_registry::{Elements, Raw, decoder_for, decoders_for, encoder_for, raw_of};
use profluens_core::element::ElementDesc;

/// Software first, so the answers do not hang on the machine's GPU.
static SETS: [Elements; 15] = [
    pf_opus::ELEMENTS,
    pf_libflac::ELEMENTS,
    pf_av1::ELEMENTS,
    pf_libav::ELEMENTS,
    pf_vaapi::ELEMENTS,
    pf_text::ELEMENTS,
    pf_subparse::ELEMENTS,
    pf_mkv::ELEMENTS,
    pf_mp4::ELEMENTS,
    pf_avi::ELEMENTS,
    pf_flv::ELEMENTS,
    pf_ogg::ELEMENTS,
    pf_ps::ELEMENTS,
    pf_tsdemux::ELEMENTS,
    pf_bare::ELEMENTS,
];

static DEMUXERS: [(Elements, &[&str]); 8] = [
    (pf_mkv::ELEMENTS, pf_mkv::EMITS),
    (pf_mp4::ELEMENTS, pf_mp4::EMITS),
    (pf_avi::ELEMENTS, pf_avi::EMITS),
    (pf_flv::ELEMENTS, pf_flv::EMITS),
    (pf_ogg::ELEMENTS, pf_ogg::EMITS),
    (pf_ps::ELEMENTS, pf_ps::EMITS),
    (pf_tsdemux::ELEMENTS, pf_tsdemux::EMITS),
    (pf_bare::ELEMENTS, &[]),
];

/// What a demuxer emits that pf does not decode to raw media, and why.
static UNDECODED: [(&str, &str); 11] = [
    ("subtitle/srt", "text cues"),
    ("subtitle/ass", "text cues"),
    ("subtitle/vtt", "text cues"),
    ("subtitle/ttml", "text cues"),
    ("subtitle/pgs", "pgsdec puts out subtitle/bitmap"),
    ("subtitle/vobsub", "vobsubdec puts out subtitle/bitmap"),
    ("subtitle/dvb", "dvbsubdec puts out subtitle/bitmap"),
    ("vfw", "a VFW fourcc pf does not map"),
    ("pcm", "PCM of a depth audio/raw does not name"),
    ("encrypted", "refused, no keys"),
    ("compressed", "refused, a ContentCompAlgo pf does not undo"),
];

fn emitted() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for (elements, emits) in &DEMUXERS {
        let pads = elements.iter().filter(|d| takes_bytes(d)).flat_map(|d| d.srcs);
        let offered = pads.flat_map(|p| p.offers).map(|o| o.family);
        out.extend(emits.iter().copied().chain(offered).filter(|f| is_coded(f)));
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn takes_bytes(desc: &ElementDesc) -> bool {
    desc.sinks.iter().any(|p| p.offers.iter().any(|o| o.family == pf_registry::BYTES))
}

fn undecoded(family: &str) -> bool {
    UNDECODED.iter().any(|e| e.0 == family)
}

#[test]
fn every_family_a_demuxer_emits_decodes_or_is_listed() {
    let families = emitted();
    assert!(families.len() > 30, "{families:?}");
    for family in &families {
        let dec = decoder_for(&SETS, family).map(|d| d.name);
        match (dec, undecoded(family)) {
            (Some(_), false) | (None, true) => {}
            (None, false) => panic!("{family} has no decoder, add one or list it in UNDECODED"),
            (Some(d), true) => panic!("{family} decodes through {d}, take it out of UNDECODED"),
        }
    }
    for (family, _) in &UNDECODED {
        assert!(families.contains(family), "{family} is listed but no demuxer emits it");
    }
}

#[test]
fn decoders_by_family() {
    let cases = [
        ("opus", "opusdec"),
        ("flac", "flacdec"),
        ("flac/ogg", "oggflacdec"),
        ("aac", "libavaacdec"),
        ("ac3", "libavac3dec"),
        ("eac3", "libavac3dec"),
        ("mp3", "libavmp3dec"),
        ("vorbis", "libavvorbisdec"),
        ("dts", "libavdtsdec"),
        ("truehd", "libavtruehddec"),
        ("alac", "libavalacdec"),
        ("speex", "libavspeexdec"),
        ("av1", "av1dec"),
        ("h264/annexb", "libavh264dec"),
        ("h265/annexb", "libavh265dec"),
        ("vp9", "libavvp9dec"),
        ("mpeg1", "libavmpeg2dec"),
        ("vp6f", "libavvp6fdec"),
        ("theora", "libavtheoradec"),
    ];
    for (family, name) in cases {
        assert_eq!(decoder_for(&SETS, family).map(|d| d.name), Some(name), "{family}");
    }
    assert_eq!(raw_of(&SETS, "opus"), Some(Raw::Audio));
    assert_eq!(raw_of(&SETS, "flac/ogg"), Some(Raw::Audio));
    assert_eq!(raw_of(&SETS, "vp9"), Some(Raw::Video));
    assert_eq!(raw_of(&SETS, "speex"), Some(Raw::Audio));
    assert_eq!(raw_of(&SETS, "theora"), Some(Raw::Video));
    assert!(decoder_for(&SETS, "bytes").is_none());
    assert!(decoder_for(&SETS, "audio/raw").is_none());
}

#[test]
fn a_set_put_first_is_preferred() {
    let hw_first = [pf_vaapi::ELEMENTS, pf_libav::ELEMENTS, pf_av1::ELEMENTS];
    assert_eq!(decoder_for(&hw_first, "h264/annexb").map(|d| d.name), Some("vaapih264dec"));
    assert_eq!(decoder_for(&hw_first, "av1").map(|d| d.name), Some("vaapiav1dec"));
    let all: Vec<&str> = decoders_for(&hw_first, "av1").map(|d| d.name).collect();
    assert_eq!(all, ["vaapiav1dec", "av1dec"]);
}

#[test]
fn encoders_by_family() {
    let cases = [
        (Raw::Audio, "opus", "opusenc"),
        (Raw::Audio, "aac", "libavaacenc"),
        (Raw::Video, "h264/annexb", "vaapih264enc"),
        (Raw::Video, "h265/annexb", "vaapih265enc"),
        (Raw::Video, "vp8", "vaapivp8enc"),
        (Raw::Video, "vp9", "vaapivp9enc"),
        (Raw::Video, "av1", "vaapiav1enc"),
    ];
    for (raw, family, name) in cases {
        assert_eq!(encoder_for(&SETS, raw, family).map(|d| d.name), Some(name), "{family}");
    }
    assert!(encoder_for(&SETS, Raw::Video, "opus").is_none());
    // flacenc writes a native FLAC file, so its src offers bytes.
    assert!(encoder_for(&SETS, Raw::Audio, "flac").is_none());
}

#[test]
fn raw_names_are_the_media_crates() {
    assert_eq!(Raw::Audio.family(), profluens_audio::FAMILY);
    assert_eq!(Raw::Video.family(), profluens_video::format::FAMILY);
}

#[test]
fn registered_names_are_unique_across_sets() {
    let mut names: Vec<&str> = SETS.iter().flat_map(|s| s.iter().map(|d| d.name)).collect();
    names.sort_unstable();
    let len = names.len();
    names.dedup();
    assert_eq!(names.len(), len, "{names:?}");
}
