mod at;
mod avi;
mod bare;
mod flv;
mod mkv;
mod mp4;
mod ogg;
#[cfg(feature = "pipeline")]
pub mod pipeline;
mod sps;

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

pub use at::OpenAt;
pub use pf_probe::Kind;
use pf_probe::head::{AviHeadError, MkvHeadError, Mp4HeadError};
use profluens_video::color::{HdrStatic, StreamColor};

/// What an open reads past the tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Want {
    /// The Matroska Cues, the AVI idx1 and the MP4 sample tables, for demuxers that then read
    /// or fold none.
    pub index: bool,
    /// The tags and pictures, for `pf_tags::of`.
    pub meta: bool,
}

impl Want {
    pub const ALL: Want = Want { index: true, meta: true };
    pub const TRACKS: Want = Want { index: false, meta: false };
}

/// The most an open reads of each part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// The first read, the sniff and every head that fits in it come from it.
    pub window: usize,
    /// A Matroska head past it once Tracks is read ends before the element that crosses it, as it
    /// ends before the Attachments however large, an AVI head is refused.
    pub max_head: u64,
    /// Matroska Cues past it are not read.
    pub max_cues: u64,
    /// A Matroska Tags element past it is left out, an Ogg comment packet grows the head to it.
    pub max_tags: u64,
    /// A larger attached picture is left out.
    pub max_picture: u64,
    /// Matroska Cues and Tags in the last this many bytes of the file come in one read.
    pub max_tail: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            window: 256 << 10,
            max_head: 64 << 20,
            max_cues: 32 << 20,
            max_tags: 4 << 20,
            max_picture: 16 << 20,
            max_tail: 4 << 20,
        }
    }
}

/// Bytes of [`Opened::arena`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub at: u32,
    pub len: u32,
}

impl Span {
    pub fn range(self) -> Range<usize> {
        self.at as usize..self.at as usize + self.len as usize
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Text,
    #[default]
    Other,
}

/// A track as its container states it, the bitstream filling in what the container leaves out.
/// Zero is unknown for every number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackDesc {
    /// The container's number, the Matroska TrackNumber, the MP4 track_ID, the AVI stream
    /// ordinal plus one, the Ogg bos ordinal plus one, FLV 1 for video and 2 for audio, 1 for a
    /// bare file.
    pub id: u32,
    /// What the demuxer names it by, the Ogg serial, the AVI stream ordinal, else the id.
    pub stream: u32,
    pub kind: TrackKind,
    /// The family the demuxer sends it as.
    pub family: &'static str,
    /// The codec head, Annex B parameter sets for H.264 and H.265, OpusHead for Opus.
    pub head: Span,
    pub language: Span,
    pub title: Span,
    pub default: bool,
    pub forced: bool,
    pub channels: u16,
    pub rate: u32,
    pub depth: u8,
    pub width: u32,
    pub height: u32,
    /// Pixel aspect ratio, 1 to 1 where the container states none.
    pub par: (u32, u32),
    /// Degrees clockwise the picture turns to stand upright.
    pub rotation: u16,
    /// A frame's duration, the Matroska DefaultDuration or the AVI sample duration.
    pub frame_ns: u64,
    /// Bits a second the container states or its sizes tell.
    pub bitrate: u64,
    /// The stamps run this far ahead of presentation, CodecDelay, pre-skip or the edit's priming.
    pub delay_ns: u64,
    /// Decode this much before a seek target.
    pub seek_preroll_ns: u64,
    pub colour: StreamColor,
    /// The NAL length prefix size of the stored units, 0 for Annex B or codecs without NALs.
    pub nal_length: u8,
    pub duration_ns: u64,
}

impl TrackDesc {
    fn new(id: u32, kind: TrackKind, family: &'static str) -> TrackDesc {
        TrackDesc {
            id,
            stream: id,
            kind,
            family,
            head: Span::default(),
            language: Span::default(),
            title: Span::default(),
            default: false,
            forced: false,
            channels: 0,
            rate: 0,
            depth: 0,
            width: 0,
            height: 0,
            // nothing stated yet, `Desc::push` makes it square when nothing does
            par: (0, 0),
            rotation: 0,
            frame_ns: 0,
            bitrate: 0,
            delay_ns: 0,
            seek_preroll_ns: 0,
            colour: StreamColor::default(),
            nal_length: 0,
            duration_ns: 0,
        }
    }
}

/// Why a track has no [`TrackDesc`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipWhy {
    /// Its codec head does not parse.
    BadHead,
    /// Its number is past `u32`.
    TooLarge,
    /// The container names a codec no demuxer sends.
    NoFamily,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub id: u64,
    pub family: &'static str,
    pub why: SkipWhy,
}

/// What each run's demuxer takes instead of reading it again.
#[derive(Clone, Default)]
pub enum Index {
    #[default]
    None,
    /// The head parsed, its tracks and where the rest lies, with every track's CuePoints, empty
    /// when not read or absent. The demuxer of each run parses none of it again.
    Mkv(Arc<pf_mkv::Parsed>),
    Avi(Arc<pf_avi::AviIndex>),
    Bare(Arc<pf_bare::BareHead>),
    /// The MP4 tracks and sample index, the demuxer of each run reads these.
    Mp4(Arc<pf_mp4::Tables>),
}

impl fmt::Debug for Index {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Index::None => f.write_str("None"),
            Index::Mkv(p) => f
                .debug_struct("Mkv")
                .field("first_cluster", &p.first_cluster)
                .field("cues", &p.cues.len())
                .field("at", &p.cued_at)
                .finish(),
            Index::Avi(i) => f.debug_struct("Avi").field("whole", &i.whole).finish(),
            Index::Bare(h) => f.debug_tuple("Bare").field(h).finish(),
            Index::Mp4(t) => f.debug_tuple("Mp4").field(t).finish(),
        }
    }
}

/// The tag bytes an open read, for `pf_tags::of`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Meta {
    #[default]
    None,
    Mkv(pf_mkv::walk::Meta),
    /// A FLAC file's blocks or an MPEG audio or ADTS file's ID3 and APE tags.
    Bare {
        raw: pf_bare::RawTags,
        flac: bool,
    },
    /// The `moov` box in [`Opened::head`].
    Moov(Range<usize>),
    /// The Vorbis comment body of the first stream that has one, in [`Opened::arena`].
    Ogg(Span),
}

/// A file opened, its tracks and what every run over it shares.
#[derive(Clone, Debug)]
pub struct Opened {
    pub kind: Kind,
    pub len: u64,
    pub duration_ns: u64,
    /// The bytes the demuxer starts from, empty for a bare file. A Matroska demuxer starts from
    /// the head parsed in [`Index::Mkv`], the head ending before any Attachments behind Tracks.
    pub head: Arc<[u8]>,
    pub tracks: Vec<TrackDesc>,
    /// The codec heads, languages and titles the spans of the tracks name.
    pub arena: Vec<u8>,
    pub index: Index,
    pub meta: Meta,
    pub skipped: Vec<Skipped>,
}

impl Opened {
    /// [`Brand::of`] its head.
    pub fn brand(&self) -> Brand {
        Brand::of(self.kind, &self.head)
    }

    pub fn bytes(&self, s: Span) -> &[u8] {
        self.arena.get(s.range()).unwrap_or_default()
    }

    /// Spans of strings are copied from strings, a span that is none reads empty.
    pub fn text(&self, s: Span) -> &str {
        std::str::from_utf8(self.bytes(s)).unwrap_or_default()
    }

    pub fn codec_head(&self, t: &TrackDesc) -> &[u8] {
        self.bytes(t.head)
    }

    pub fn language(&self, t: &TrackDesc) -> &str {
        self.text(t.language)
    }

    pub fn title(&self, t: &TrackDesc) -> &str {
        self.text(t.title)
    }

    pub fn track(&self, id: u32) -> Option<&TrackDesc> {
        self.tracks.iter().find(|t| t.id == id)
    }

    /// The Cluster a frame of Matroska `track` at `t_ns` is read from, the last of its cues at or
    /// before it, the first Cluster before any. Cues of every track when `track` has none. None
    /// for other containers.
    pub fn cue_before(&self, track: u64, t_ns: u64) -> Option<u64> {
        let Index::Mkv(p) = &self.index else { return None };
        let (first_cluster, cues) = (&p.first_cluster, &p.cues);
        let own = cues.iter().any(|c| c.track == track);
        let at = cues
            .iter()
            .filter(|c| (!own || c.track == track) && c.time_ns <= t_ns)
            .map(|c| (c.time_ns, c.byte))
            .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        Some(at.map_or(*first_cluster, |c| c.1))
    }
}

/// The flavour a container states of itself, by the Matroska DocType or the MP4 `ftyp` brands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Brand {
    /// A container without flavours.
    #[default]
    None,
    Matroska,
    WebM,
    /// ISO base media, `isom`, `mp41`, `mp42` and the rest.
    Mp4,
    /// Apple QuickTime, `qt  `.
    QuickTime,
    /// MPEG-4 audio, `M4A `, `M4B ` or `M4P `.
    M4a,
}

impl Brand {
    /// The flavour of a file of `kind` whose first bytes are `head`. A Matroska DocType other
    /// than `webm` is Matroska, an MP4 whose brands name no class is Mp4.
    pub fn of(kind: Kind, head: &[u8]) -> Brand {
        match kind {
            Kind::Mkv => match mkv::doc_type(head) {
                Some(b"webm") => Brand::WebM,
                _ => Brand::Matroska,
            },
            Kind::Mp4 => mp4::brand(head),
            _ => Brand::None,
        }
    }
}

/// What [`OpenAt`] refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// No supported container or codec leads the file.
    Unknown(pf_probe::ProbeError),
    /// A container pf-open does not open.
    Unsupported(Kind),
    /// No track in the head.
    NoStreams,
    TooLarge,
    Malformed {
        at: u64,
    },
    MkvHead(MkvHeadError),
    Mp4Head(Mp4HeadError),
    AviHead(AviHeadError),
    Mp4(pf_mp4::Mp4Error),
    Bare(pf_bare::BareError),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Unknown(e) => e.fmt(f),
            OpenError::Unsupported(k) => write!(f, "{} files are not opened here", k.label()),
            OpenError::NoStreams => f.write_str("no tracks"),
            OpenError::TooLarge => f.write_str("too large"),
            OpenError::Malformed { at } => write!(f, "malformed at byte {at}"),
            OpenError::MkvHead(e) => e.fmt(f),
            OpenError::Mp4Head(e) => e.fmt(f),
            OpenError::AviHead(e) => e.fmt(f),
            OpenError::Mp4(e) => e.fmt(f),
            OpenError::Bare(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {}

/// `c` as H.273 code points, matrix, primaries and transfer, and the Matroska range, 2 and 0
/// where it states none.
pub fn codes(c: &StreamColor) -> [u8; 4] {
    [
        c.matrix.map_or(2, |m| m.to_h273()),
        c.primaries.map_or(2, |p| p.to_h273()),
        c.transfer.map_or(2, |t| t.to_h273()),
        c.range.map_or(0, |r| r.to_mkv()),
    ]
}

/// The mastering display colour volume SEI payload of H.265 D.2.28 then the content light level
/// payload of D.2.35, big endian, None when nothing is stated.
pub fn hdr_meta(h: &HdrStatic) -> Option<[u8; 28]> {
    if h.is_empty() {
        return None;
    }
    let m = &h.mastering;
    let mut out = [0u8; 28];
    let mut at = 0;
    let mut put = |b: &[u8]| {
        out[at..at + b.len()].copy_from_slice(b);
        at += b.len();
    };
    // green, blue, red on the wire, red, green, blue in the struct
    for c in [1, 2, 0] {
        put(&m.primaries[c][0].to_be_bytes());
        put(&m.primaries[c][1].to_be_bytes());
    }
    put(&m.white[0].to_be_bytes());
    put(&m.white[1].to_be_bytes());
    put(&m.max_luminance.to_be_bytes());
    put(&m.min_luminance.to_be_bytes());
    put(&h.light.max_cll.to_be_bytes());
    put(&h.light.max_fall.to_be_bytes());
    Some(out)
}

/// Tracks, their bytes and what was left out, as each container's description fills them.
#[derive(Debug, Default)]
pub(crate) struct Desc {
    pub tracks: Vec<TrackDesc>,
    pub arena: Vec<u8>,
    pub skipped: Vec<Skipped>,
}

impl Desc {
    /// `b` appended to the arena.
    pub fn put(&mut self, b: &[u8]) -> Span {
        let at = self.arena.len();
        self.arena.extend_from_slice(b);
        Span { at: at as u32, len: b.len() as u32 }
    }

    pub fn skip(&mut self, id: u64, family: &'static str, why: SkipWhy) {
        self.skipped.push(Skipped { id, family, why });
    }

    /// The track with the colour, size, depth and pixel aspect its first SPS states where the
    /// container leaves them out.
    pub fn push(&mut self, mut t: TrackDesc) {
        let head = t.head;
        sps::fill(&mut t, self.arena.get(head.range()).unwrap_or_default());
        if t.par.0 == 0 || t.par.1 == 0 {
            t.par = (1, 1);
        }
        self.tracks.push(t);
    }
}

/// `id` as a track id, None past `u32`.
pub(crate) fn id_of(id: u64) -> Option<u32> {
    u32::try_from(id).ok()
}

/// The ratio `a` to `b` in lowest terms, 1 to 1 for a zero.
pub(crate) fn ratio(a: u64, b: u64) -> (u32, u32) {
    if a == 0 || b == 0 {
        return (1, 1);
    }
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    let (a, b) = (a / x, b / x);
    match (u32::try_from(a), u32::try_from(b)) {
        (Ok(a), Ok(b)) => (a, b),
        _ => (1, 1),
    }
}

#[cfg(test)]
// Test fixtures allocate and read the disk freely.
#[allow(clippy::disallowed_methods, clippy::unwrap_used)]
mod tests {
    use super::*;
    use profluens_core::fetch::drive_slice;
    use profluens_video::color::{ContentLight, Mastering};

    /// The VPS, SPS and PPS of a 10 bit PQ BT.2020 HEVC stream whose VUI states its colour.
    const PQ_SETS: [u8; 86] = [
        0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x02, 0x20, 0x00, 0x00, 0x03,
        0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x1e, 0xba, 0x02, 0x40, 0x00, 0x00,
        0x00, 0x01, 0x42, 0x01, 0x01, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03,
        0x00, 0x00, 0x03, 0x00, 0x1e, 0xa0, 0x10, 0x20, 0x20, 0x4d, 0x96, 0xe9, 0x29, 0x30, 0xbc,
        0x05, 0xa8, 0x48, 0x80, 0x48, 0x20, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x03, 0x03,
        0x21, 0x00, 0x00, 0x00, 0x01, 0x44, 0x01, 0xc0, 0x73, 0xc1, 0x89,
    ];

    fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let mut b = ((16 + 4 * compatible.len()) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(b"ftyp");
        b.extend_from_slice(major);
        b.extend_from_slice(&[0, 0, 2, 0]);
        compatible.iter().for_each(|c| b.extend_from_slice(*c));
        b.extend_from_slice(b"\0\0\0\x08qt  ");
        b
    }

    fn ebml(doc_type: Option<&[u8]>) -> Vec<u8> {
        let mut body = vec![0x42, 0x86, 0x81, 0x01];
        if let Some(d) = doc_type {
            body.extend_from_slice(&[0x42, 0x82, 0x80 | d.len() as u8]);
            body.extend_from_slice(d);
        }
        body.extend_from_slice(&[0x42, 0x87, 0x81, 0x04]);
        let mut b = vec![0x1A, 0x45, 0xDF, 0xA3, 0x80 | body.len() as u8];
        b.extend_from_slice(&body);
        b.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
        b
    }

    #[test]
    fn the_brand_is_the_doctype_or_the_ftyp_class() {
        assert_eq!(Brand::of(Kind::Mkv, &ebml(Some(b"webm"))), Brand::WebM);
        assert_eq!(Brand::of(Kind::Mkv, &ebml(Some(b"webm\0\0"))), Brand::WebM, "NUL padded");
        assert_eq!(Brand::of(Kind::Mkv, &ebml(Some(b"matroska"))), Brand::Matroska);
        assert_eq!(Brand::of(Kind::Mkv, &ebml(None)), Brand::Matroska, "none stated");
        assert_eq!(Brand::of(Kind::Mkv, &ebml(Some(b"webm"))[..12]), Brand::Matroska, "cut");
        assert_eq!(Brand::of(Kind::Mp4, &ftyp(b"qt  ", &[b"qt  "])), Brand::QuickTime);
        assert_eq!(Brand::of(Kind::Mp4, &ftyp(b"M4A ", &[b"M4A ", b"mp42", b"isom"])), Brand::M4a);
        assert_eq!(Brand::of(Kind::Mp4, &ftyp(b"isom", &[b"isom", b"iso2", b"avc1"])), Brand::Mp4);
        assert_eq!(
            Brand::of(Kind::Mp4, &ftyp(b"CAEP", &[b"CAEP", b"qt  "])),
            Brand::QuickTime,
            "a compatible one"
        );
        assert_eq!(Brand::of(Kind::Mp4, &ftyp(b"qt  ", &[])[..10]), Brand::Mp4, "cut");
        // the box that follows is no compatible brand
        assert_eq!(Brand::of(Kind::Mp4, &ftyp(b"XYZW", &[])), Brand::Mp4);
        assert_eq!(Brand::of(Kind::Ogg, b"OggS"), Brand::None);
        assert_eq!(Brand::of(Kind::Mp4, b""), Brand::Mp4);
    }

    /// A track that leaves its colour to the bitstream is PQ by its SPS. A VP9 track keeps the
    /// container's.
    #[test]
    fn the_sps_fills_what_the_container_leaves_out() {
        let mut d = Desc::default();
        let mut t = TrackDesc::new(1, TrackKind::Video, "h265/annexb");
        t.head = d.put(&PQ_SETS);
        d.push(t);
        let t = d.tracks[0];
        assert_eq!(codes(&t.colour), [9, 9, 16, 1]);
        assert_eq!((t.width, t.height, t.depth), (128, 128, 10));
        let mut vp9 = TrackDesc::new(2, TrackKind::Video, "vp9");
        vp9.head = d.put(&PQ_SETS);
        vp9.colour.matrix = profluens_video::color::Matrix::from_h273(1);
        d.push(vp9);
        assert_eq!(codes(&d.tracks[1].colour), [1, 2, 2, 0]);
        assert_eq!(d.tracks[1].depth, 0);
        assert_eq!(codes(&StreamColor::default()), [2, 2, 2, 0]);
    }

    #[test]
    fn hdr_meta_is_the_two_sei_payloads_or_nothing() {
        assert_eq!(hdr_meta(&HdrStatic::default()), None);
        let m = Mastering {
            primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
            white: [15635, 16450],
            max_luminance: 10_000_000,
            min_luminance: 50,
        };
        let h = HdrStatic { mastering: m, light: ContentLight { max_cll: 1000, max_fall: 400 } };
        let b = hdr_meta(&h).unwrap();
        // green first on the wire, red in the struct
        assert_eq!(&b[..4], &[0x21, 0x34, 0x9b, 0xaa]);
        assert_eq!(Mastering::from_st2086(&b[..24]), Some(m), "the mastering payload reads back");
        assert_eq!(&b[24..], &[0x03, 0xe8, 0x01, 0x90]);
    }

    #[test]
    fn ratios_are_in_lowest_terms() {
        assert_eq!(ratio(1280 * 9, 720 * 16), (1, 1));
        assert_eq!(ratio(16 * 480, 9 * 720), (32, 27));
        assert_eq!((ratio(0, 5), ratio(5, 0)), ((1, 1), (1, 1)));
        assert_eq!(ratio(u64::MAX, 1), (1, 1), "past u32");
        assert_eq!((id_of(7), id_of(1 << 40)), (Some(7), None));
    }

    fn fixture(rel: &str) -> Vec<u8> {
        std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel))
            .unwrap()
    }

    /// The heads found in a window are the ones the head machines read.
    #[test]
    fn a_head_in_the_window_is_the_machines() {
        for rel in [
            "plugins/formats/mp4/tests/fixtures/cues_last.mkv",
            "lib/open/tests/fixtures/tagged.mkv",
        ] {
            let b = fixture(rel);
            let want = drive_slice(&mut pf_probe::head::MkvHeadAt::new(64 << 20), &b).unwrap();
            let passed = mkv::Passed { tags: want.tags, attachments: want.attachments };
            let at = (want.head.len(), want.first_cluster, passed);
            assert_eq!(mkv::head_in(&b, 64 << 20), Some(at), "{rel}");
            match want.attachments {
                Some(a) => assert_eq!(a, want.head.len() as u64, "{rel} ends at the Attachments"),
                None => assert_eq!(want.first_cluster, want.head.len() as u64, "{rel}"),
            }
            assert_eq!(mkv::head_in(&b, 10), None, "past the cap");
            assert_eq!(mkv::head_in(&b[..want.head.len()], 64 << 20), None, "the Cluster unseen");
        }
        let b = fixture("plugins/formats/mp4/tests/fixtures/tiny_av.mp4");
        let want =
            drive_slice(&mut pf_probe::head::Mp4HeadAt::new(&b, b.len() as u64), &b).unwrap();
        assert_eq!(mp4::head_in(&b, b.len() as u64), Some(want.len()));
        let last = fixture("plugins/formats/mp4/tests/fixtures/moov_last.mp4");
        assert_eq!(mp4::head_in(&last, last.len() as u64), None, "the moov behind the mdat");
        assert_eq!(mp4::head_in(&b[..100], b.len() as u64), None, "the moov cut");
        let avi = fixture("plugins/formats/avi/tests/fixtures/tiny.avi");
        let want = drive_slice(&mut pf_probe::head::AviHeadAt::new(16 << 20), &avi).unwrap();
        assert_eq!(avi::movi_in(&avi, 16 << 20), Some(want.len()));
        assert_eq!(avi::movi_in(&avi, 100), None);
    }

    #[test]
    fn spans_read_back_and_a_bad_one_is_empty() {
        let mut d = Desc::default();
        let s = d.put(b"eng");
        let o = Opened {
            kind: Kind::Mkv,
            len: 0,
            duration_ns: 0,
            head: Arc::from(&[][..]),
            tracks: Vec::new(),
            arena: d.arena,
            index: Index::None,
            meta: Meta::None,
            skipped: Vec::new(),
        };
        assert_eq!((o.text(s), o.bytes(s)), ("eng", &b"eng"[..]));
        assert_eq!(o.text(Span { at: 2, len: 9 }), "");
        assert_eq!(o.cue_before(1, 0), None, "no Cues outside Matroska");
        assert!(Span::default().is_empty());
    }
}
