pub type FourCc = [u8; 4];

/// One FourCC a family and one family a FourCC. ISO BMFF sample entry codes where MP4RA registers
/// one, the rest are pf's own and marked.
pub static FOURCCS: [(&str, FourCc); 25] = [
    ("h264/annexb", *b"avc1"),
    ("h265/annexb", *b"hvc1"),
    ("av1", *b"av01"),
    ("vp9", *b"vp09"),
    ("vp8", *b"vp08"),
    ("mpeg4/asp", *b"mp4v"),
    ("opus", *b"Opus"),
    ("aac", *b"mp4a"),
    ("flac", *b"fLaC"),
    ("ac3", *b"ac-3"),
    ("eac3", *b"ec-3"),
    ("dts", *b"dtsc"),
    ("truehd", *b"mlpa"),
    ("alac", *b"alac"),
    // QuickTime's, ISO puts MP3 under mp4a.
    ("mp3", *b".mp3"),
    ("subtitle/vtt", *b"wvtt"),
    ("subtitle/ttml", *b"stpp"),
    // Not registered, pf's.
    ("mlp", *b"mlp "),
    ("vorbis", *b"vorb"),
    ("subtitle/srt", *b"srt "),
    ("subtitle/ass", *b"ass "),
    ("subtitle/pgs", *b"pgs "),
    ("subtitle/vobsub", *b"vobs"),
    ("subtitle/dvb", *b"dvbs"),
    ("subtitle/cue-ir", *b"cuir"),
];

pub fn fourcc_of(family: &str) -> Option<FourCc> {
    FOURCCS.iter().find(|e| e.0 == family).map(|e| e.1)
}

pub fn family_of(cc: FourCc) -> Option<&'static str> {
    FOURCCS.iter().find(|e| e.1 == cc).map(|e| e.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_round_trips_and_none_repeats() {
        for (i, &(family, cc)) in FOURCCS.iter().enumerate() {
            assert_eq!(fourcc_of(family), Some(cc), "{family}");
            assert_eq!(family_of(cc), Some(family), "{cc:?}");
            for &(f, c) in &FOURCCS[i + 1..] {
                assert_ne!(f, family);
                assert_ne!(c, cc, "{family} and {f}");
            }
        }
    }

    #[test]
    fn unknown_names_have_no_code() {
        assert_eq!(fourcc_of("bytes"), None);
        assert_eq!(fourcc_of("audio/raw"), None);
        assert_eq!(family_of(*b"avc3"), None);
        assert_eq!(fourcc_of("opus"), Some(*b"Opus"));
        assert_eq!(family_of(*b"fLaC"), Some("flac"));
    }
}
