//! pf-mp3 readers on untrusted bytes, and the tag readers against tags written here.
//! Knob bits 0-2 head scan piece size, 3 known length, 4-5 probe audio length, 6-7 ID3v2 mode.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::cell::RefCell;

use libfuzzer_sys::fuzz_target;
use pf_mp3::frame::Header;
use pf_mp3::id3;
use pf_mp3::index::{HeadScan, WINDOW, index_of};
use pf_mp3::{TAG_FRAME_PROBE, TagFrame, XingToc, probe_props, tag_frame};
use profluens_core::event::TagSink;
use profluens_core::pipeline::Index;
use profluens_core::scratch::Arena;

const PIECES: [usize; 8] = [1, 3, 7, 10, 64, 1000, 4096, usize::MAX];
const RATES: [u32; 9] = [44_100, 48_000, 32_000, 22_050, 24_000, 16_000, 11_025, 12_000, 8_000];
/// MPEG-2 layer II at 160 kbit/s and 8 kHz, padded.
const MAX_FRAME: usize = 144 * 160_000 / 8_000 + 1;
const IGNORED: u32 = 0x0000_013F;

thread_local! {
    static ARENA: RefCell<Arena> = RefCell::new(Arena::default());
}

#[derive(Default, Debug, PartialEq)]
struct Got {
    text: Vec<(String, String)>,
    pics: Vec<(String, usize)>,
}

impl TagSink for Got {
    fn text(&mut self, key: &str, value: &str) {
        self.text.push((key.to_string(), value.to_string()));
    }
    fn picture(&mut self, mime: &str, data: &[u8]) {
        self.pics.push((mime.to_string(), data.len()));
    }
}

impl Got {
    fn bounded(&self, input: usize, grow: usize, key_max: usize) {
        let entries = self.text.len() + self.pics.len();
        assert!(entries <= input, "{entries} tags out of {input} bytes");
        let values = self.text.iter().map(|(_, v)| v.len()).sum::<usize>()
            + self.pics.iter().map(|(_, n)| n).sum::<usize>();
        assert!(values <= grow * input, "{values} value bytes out of {input}");
        for (k, v) in &self.text {
            assert!(!k.is_empty() && !v.is_empty(), "an empty key or value");
            assert!(k.len() <= key_max, "a key of {} bytes", k.len());
            assert!(!k.bytes().any(|b| b.is_ascii_lowercase()), "a lowercase key {k:?}");
        }
        assert!(self.pics.iter().all(|(_, n)| *n > 0), "an empty picture");
    }
}

fn with_arena<R>(f: impl FnOnce(&Arena) -> R) -> R {
    ARENA.with(|a| {
        let mut a = a.borrow_mut();
        a.reset();
        f(&a)
    })
}

fuzz_target!(|data: &[u8]| {
    let Some((&knob, bytes)) = data.split_first() else {
        return;
    };
    header(bytes);
    probe(bytes, knob);
    vbr_tag_frame(bytes);
    head_scan(bytes, knob);
    readers(bytes);
    id3v2_round_trip(bytes, knob >> 6);
    ape_round_trip(bytes);
    id3v1_round_trip(bytes);
});

fn header(b: &[u8]) {
    let Some(h) = Header::parse(b) else {
        return;
    };
    assert_eq!(Header::parse(&b[..4]), Some(h), "a header read past its four bytes");
    assert!((1..=3).contains(&h.layer) && RATES.contains(&h.sample_rate_hz));
    assert!(matches!(h.samples_per_frame(), 384 | 576 | 1152));
    assert!(matches!(h.channel_count(), 1 | 2));
    assert!(h.bitrate_kbps.is_none_or(|k| (8..=448).contains(&k)));
    assert_eq!(h.frame_len().is_some(), h.bitrate_kbps.is_some());
    assert!(
        h.frame_len().is_none_or(|n| (20..=MAX_FRAME).contains(&n)),
        "frame {:?}",
        h.frame_len()
    );
    let raw = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    for flip in [IGNORED, 0x0000_0001, 0x0000_0030, 0x0000_0100] {
        assert_eq!(
            Header::parse(&(raw ^ flip).to_be_bytes()),
            Some(h),
            "bits {flip:#x} changed the header"
        );
    }
    if !h.mono {
        let mode = (raw >> 6) & 3;
        for m in (0..3).filter(|&m| m != mode) {
            let other = Header::parse(&((raw & !0xC0) | (m << 6)).to_be_bytes());
            assert_eq!(other, Some(h), "mode {m} changed the header");
        }
    }
}

fn probe(audio: &[u8], knob: u8) {
    let len = audio.len() as u64;
    let audio_len = [len, 0, u64::MAX, len.saturating_mul(1000)][usize::from(knob >> 4 & 3)];
    let Some(p) = probe_props(audio, audio_len) else {
        assert!(probe_props(audio, len / 2).is_none(), "the audio length found a frame");
        return;
    };
    assert_eq!(probe_props(audio, audio_len), Some(p), "the probe is not deterministic");
    let h = audio
        .get(p.first_frame_offset..)
        .and_then(Header::parse)
        .expect("no header at the first frame");
    assert_eq!(
        (p.sample_rate, p.channels, p.bitrate_kbps),
        (h.sample_rate_hz, u32::from(h.channel_count()), h.bitrate_kbps)
    );
    assert!(p.toc.is_none() || p.exact, "a seek table without a VBR header");
    assert!(p.stream_bytes.is_none() || p.exact, "a byte count without a VBR header");
    if p.exact {
        let most = u128::from(u32::MAX) * u128::from(h.samples_per_frame()) * 1_000_000_000
            / u128::from(h.sample_rate_hz);
        assert!(
            p.duration_ns.is_some_and(|d| u128::from(d) <= most),
            "a frame count duration past its bound"
        );
    } else {
        let cbr = h.bitrate_kbps.map(|k| {
            (u128::from(audio_len) * 8_000_000 / u128::from(k)).min(u128::from(u64::MAX)) as u64
        });
        assert_eq!(p.duration_ns, cbr, "a CBR estimate off the bitrate");
    }
    if let (Some(toc), Some(d)) = (p.toc, p.duration_ns) {
        let bytes = p.stream_bytes.map_or(len, u64::from);
        let mut last = 0;
        for i in 0..XingToc::LEN {
            let t = toc.time_at(i, d).expect("a point in the table");
            assert!(t >= last && (d == 0 || t < d), "seek point {i} at {t} of {d}");
            last = t;
            assert!(
                toc.byte_at(i, bytes).is_some_and(|b| b <= bytes),
                "seek point {i} past the stream"
            );
        }
        assert!(
            toc.time_at(XingToc::LEN, d).is_none() && toc.byte_at(XingToc::LEN, bytes).is_none()
        );
    }
}

fn vbr_tag_frame(b: &[u8]) {
    let open = tag_frame(b, false);
    let ended = tag_frame(b, true);
    if b.len() < TAG_FRAME_PROBE {
        assert_eq!(open, TagFrame::Need);
        let mut padded = [0u8; TAG_FRAME_PROBE];
        padded[..b.len()].copy_from_slice(b);
        assert_eq!(ended, tag_frame(&padded, false), "the end is not zeros");
    } else {
        assert_eq!(open, ended);
        assert_eq!(open, tag_frame(&b[..TAG_FRAME_PROBE], false), "read past the probe");
    }
    if let TagFrame::Frame(n) = ended {
        assert!((21..=MAX_FRAME).contains(&n), "a tag frame of {n} bytes");
    }
}

fn index_ok(ix: &Index, len: Option<u64>) {
    assert_eq!(ix.file_len, len);
    assert!(ix.entries.len() <= XingToc::LEN);
    assert!(!ix.complete || !ix.entries.is_empty());
    for w in ix.entries.windows(2) {
        assert!(w[0].time_ns < w[1].time_ns && w[0].byte <= w[1].byte, "entries out of order");
    }
    if let (Some(len), Some(e)) = (len, ix.entries.last()) {
        assert!(e.byte < len, "an entry past the file");
    }
}

fn model(b: &[u8], len: Option<u64>) -> Index {
    let none = Index { file_len: len, ..Index::default() };
    let Some(head) = b.get(..10) else {
        return none;
    };
    let tag = id3::v2_total_len(head).unwrap_or(0);
    match b.get(tag..) {
        Some(audio) => index_of(&audio[..audio.len().min(WINDOW)], tag as u64, len),
        None => none,
    }
}

fn head_scan(b: &[u8], knob: u8) {
    let len = (knob & 8 != 0).then_some(b.len() as u64);
    let want = model(b, len);
    index_ok(&want, len);
    for piece in [PIECES[usize::from(knob & 7)], 1] {
        let mut scan = HeadScan::new(len);
        let mut got = None;
        for chunk in b.chunks(piece.min(b.len()).max(1)) {
            if let Some(ix) = scan.feed(chunk) {
                assert!(got.is_none(), "a second index");
                got = Some(ix);
            }
            if scan.done() {
                break;
            }
        }
        let ended = scan.end();
        assert!(got.is_none() || ended.is_none(), "an index from both feed and end");
        assert!(scan.done() && scan.end().is_none() && scan.feed(b"\xFF\xFB\x90\x00").is_none());
        assert_eq!(got.or(ended).as_ref(), Some(&want), "pieces of {piece} disagree");
    }
}

fn readers(b: &[u8]) {
    if let Some(n) = id3::v2_total_len(b) {
        assert!(n >= 10);
        let whole = with_arena(|a| collect(|s| id3::parse_v2(b, a, s)));
        let own = with_arena(|a| collect(|s| id3::parse_v2(&b[..n.min(b.len())], a, s)));
        assert_eq!(whole, own, "ID3v2 read past its tag");
        // Latin-1 doubles, UTF-16 makes three of two.
        whole.bounded(b.len(), 2, 2 * b.len().max(11));
    }
    let v1 = with_arena(|a| collect(|s| id3::parse_v1(b, a, s)));
    assert!(v1.text.len() <= 7 && v1.pics.is_empty());
    if let Some(last) = b.len().checked_sub(128) {
        let own = with_arena(|a| collect(|s| id3::parse_v1(&b[last..], a, s)));
        assert_eq!(v1, own, "ID3v1 read before its 128 bytes");
    }
    v1.bounded(b.len(), 2, 11);
    if let Some(n) = id3::ape_tail_len(b) {
        assert!(n >= 32);
        let tag = &b[b.len().saturating_sub(n)..];
        let got = collect(|s| id3::parse_ape(tag, s));
        got.bounded(tag.len(), 1, 255);
        assert!(got.text.iter().all(|(_, v)| !v.contains('\0')), "a NUL inside an APE value");
    }
    let raw = collect(|s| id3::parse_ape(b, s));
    raw.bounded(b.len(), 1, 255);
}

fn collect(f: impl FnOnce(&mut Got)) -> Got {
    let mut g = Got::default();
    f(&mut g);
    g
}

const FRAMES: [(&[u8; 4], &str); 10] = [
    (b"TIT2", "TITLE"),
    (b"TPE1", "ARTIST"),
    (b"TALB", "ALBUM"),
    (b"TPE2", "ALBUMARTIST"),
    (b"TRCK", "TRACKNUMBER"),
    (b"TPOS", "DISCNUMBER"),
    (b"TCON", "GENRE"),
    (b"TCOM", "COMPOSER"),
    (b"COMM", "COMMENT"),
    (b"TXXX", ""),
];

fn encode(enc: u8, s: &str, little: bool, out: &mut Vec<u8>) {
    match enc {
        0 => out.extend(s.chars().map(|c| c as u8)),
        1 | 2 => {
            if enc == 1 {
                out.extend_from_slice(if little { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
            }
            for u in s.encode_utf16() {
                out.extend_from_slice(&if enc == 1 && little {
                    u.to_le_bytes()
                } else {
                    u.to_be_bytes()
                });
            }
        }
        _ => out.extend_from_slice(s.as_bytes()),
    }
}

fn terminator(enc: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(if matches!(enc, 1 | 2) { &[0, 0] } else { &[0] });
}

/// id3v2.4.0-structure section 6.1 stuffing.
fn unsync(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len() * 2);
    for &x in b {
        out.push(x);
        if x == 0xFF {
            out.push(0);
        }
    }
    out
}

fn synchsafe(n: usize) -> [u8; 4] {
    let n = n as u32;
    [(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]
}

fn pieces(b: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    b[..b.len().min(512)]
        .split(|&x| x == b'\n')
        .take(12)
        .filter_map(|p| p.split_first().map(|(&k, v)| (k, v)))
}

fn id3v2_round_trip(b: &[u8], mode: u8) {
    let (major, enc) = [(3u8, 0u8), (4, 3), (3, 1), (4, 2)][usize::from(mode & 3)];
    let flags = b.first().copied().unwrap_or(0);
    let (unsynced, little, pad) = (flags & 1 != 0, flags & 2 != 0, usize::from(flags >> 4));
    let mut body = Vec::new();
    let mut want = Got::default();
    for (kind, raw) in pieces(b) {
        let text: String = match enc {
            0 => raw.iter().filter(|&&c| c != 0).map(|&c| char::from(c)).collect(),
            _ => String::from_utf8_lossy(raw).chars().filter(|&c| c != '\0').collect(),
        };
        let (id, key) = FRAMES[usize::from(kind) % FRAMES.len()];
        let mut data = vec![enc];
        if kind >= 0xF0 && !raw.is_empty() {
            let mut pic = vec![0u8];
            pic.extend_from_slice(b"image/png\0\x03\0");
            pic.extend_from_slice(raw);
            body.extend_from_slice(&frame(major, b"APIC", &pic, unsynced));
            want.pics.push(("image/png".to_string(), raw.len()));
            continue;
        }
        let key = match id {
            b"COMM" => {
                data.extend_from_slice(b"eng");
                encode(enc, "d", little, &mut data);
                terminator(enc, &mut data);
                key.to_string()
            }
            b"TXXX" => {
                let desc = format!("rg_{}", kind % 10);
                encode(enc, &desc, little, &mut data);
                terminator(enc, &mut data);
                desc.to_ascii_uppercase()
            }
            _ => key.to_string(),
        };
        encode(enc, &text, little, &mut data);
        body.extend_from_slice(&frame(major, id, &data, unsynced));
        if !text.is_empty() {
            want.text.push((key, text));
        }
    }
    body.resize(body.len() + pad, 0);
    let tag_unsync = unsynced && major == 3;
    if tag_unsync {
        body = unsync(&body);
    }
    let mut tag = b"ID3".to_vec();
    tag.extend_from_slice(&[major, 0, if tag_unsync { 0x80 } else { 0 }]);
    tag.extend_from_slice(&synchsafe(body.len()));
    tag.extend_from_slice(&body);
    assert_eq!(id3::v2_total_len(&tag), Some(tag.len()));
    let got = with_arena(|a| collect(|s| id3::parse_v2(&tag, a, s)));
    assert_eq!(got, want, "ID3v2.{major} in encoding {enc} read back otherwise");
}

/// A v2.4 frame is stuffed on its own, a v2.3 one with its whole tag.
fn frame(major: u8, id: &[u8; 4], data: &[u8], unsynced: bool) -> Vec<u8> {
    let own = unsynced && major == 4;
    let data = if own { unsync(data) } else { data.to_vec() };
    let mut f = id.to_vec();
    match major {
        4 => f.extend_from_slice(&synchsafe(data.len())),
        _ => f.extend_from_slice(&(data.len() as u32).to_be_bytes()),
    }
    f.extend_from_slice(&[0, if own { 0x02 } else { 0 }]);
    f.extend_from_slice(&data);
    f
}

fn ape_round_trip(b: &[u8]) {
    let items: Vec<(String, String)> = pieces(b)
        .map(|(k, v)| {
            let key: String = v
                .iter()
                .take(usize::from(k % 12) + 2)
                .map(|&c| char::from(0x20 + c % 95))
                .collect();
            let key = if key.trim().is_empty() { "Title".to_string() } else { key };
            (key, String::from_utf8_lossy(v).into_owned())
        })
        .collect();
    let refs: Vec<(&str, &str)> = items.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let tag = pf_tags::fixture::ape_tag(&refs);
    assert_eq!(id3::ape_tail_len(&tag), Some(tag.len()));
    let mut want = Got::default();
    for (k, v) in &items {
        let k = k.to_ascii_uppercase();
        let k = match k.as_str() {
            "YEAR" => "DATE".to_string(),
            "TRACK" => "TRACKNUMBER".to_string(),
            "DISC" => "DISCNUMBER".to_string(),
            "ALBUM ARTIST" => "ALBUMARTIST".to_string(),
            _ => k,
        };
        want.text
            .extend(v.split('\0').filter(|s| !s.is_empty()).map(|s| (k.clone(), s.to_string())));
    }
    assert_eq!(collect(|s| id3::parse_ape(&tag, s)), want, "APEv2 read back otherwise");
}

fn id3v1_round_trip(b: &[u8]) {
    let field = |at: usize, n: usize| -> String {
        let s = b.get(at..).unwrap_or(&[]);
        let n = s.first().map_or(0, |&l| usize::from(l) % (n + 1));
        s.iter()
            .skip(1)
            .take(n)
            .map(|&c| {
                char::from(
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
                        [usize::from(c) % 62],
                )
            })
            .collect()
    };
    let (title, artist, album, year) = (field(0, 30), field(8, 30), field(16, 30), field(24, 4));
    let track = b.get(3).copied().unwrap_or(0);
    let tag = pf_tags::fixture::id3v1(&title, &artist, &album, &year, track);
    let mut want = Got::default();
    for (k, v) in [("TITLE", &title), ("ARTIST", &artist), ("ALBUM", &album), ("DATE", &year)] {
        if !v.is_empty() {
            want.text.push((k.to_string(), v.clone()));
        }
    }
    if track != 0 {
        want.text.push(("TRACKNUMBER".to_string(), track.to_string()));
    }
    let got = with_arena(|a| collect(|s| id3::parse_v1(&tag, a, s)));
    assert_eq!(got, want, "ID3v1 read back otherwise");
}
