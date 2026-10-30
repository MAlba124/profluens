//! The pf-tags scanner over an in-memory reactor, on raw bytes or on corrupted built files.
//! Input is a knob byte (prefix, tail, stat length, faults), a build byte (below 0x80 the rest
//! is the file, else a format), then for a built file a title, a picture length and XOR triples.

#![no_main]
// Fuzz target, not an element. Writes the file stat reads.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use pf_tags::{Format, Props, ScanConfig, ScanError, ScanOutcome, Scanner, fixture as fx};
use profluens_core::id::ElementId;
use profluens_core::io::{
    Completion, Handle, IoError, IoResult, Op, Reactor, ReactorFactory, Submission,
};
use profluens_core::memory::{Memory, Pool};

const PREFIX: [usize; 4] = [96, 512, 4096, 128 << 10];
const TAIL: [usize; 4] = [32, 300, 4096, 64 << 10];
/// `MAX_TEXT_TAGS` and `MAX_PICTURES` in plugins/tags/src/store.rs.
const MAX_TEXT: usize = 65_536;
const MAX_PICS: usize = 1_024;
const BATCH: usize = 3;
/// Off, since a read fills its buffer and a parser then sees bytes past the planned extent.
const STRICT: bool = false;
const TAIL_ALL: usize = TAIL[3];
const ALNUM: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    Short,
    Failing,
}

struct Feed {
    data: Vec<u8>,
    claimed: u64,
    fault: Fault,
    ops: u64,
    served: usize,
}

static FEED: Mutex<Feed> =
    Mutex::new(Feed { data: Vec::new(), claimed: 0, fault: Fault::None, ops: 0, served: 0 });

fn feed() -> MutexGuard<'static, Feed> {
    FEED.lock().unwrap_or_else(|e| e.into_inner())
}

impl Feed {
    fn read(&mut self, mem: &mut Memory, offset: u64, at: usize, len: usize) -> IoResult {
        self.ops += 1;
        let fate = match self.fault {
            Fault::None => 15,
            Fault::Short => 0,
            Fault::Failing => self.ops.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60,
        };
        let src = usize::try_from(offset).ok().and_then(|o| self.data.get(o..)).unwrap_or(&[]);
        let full = mem.as_mut_full();
        let at = at.min(full.len());
        let can = len.min(full.len() - at).min(src.len());
        let (n, result) = match fate {
            0..=3 if can > 1 => (can / 2, IoResult::Ok(can / 2)),
            4 | 5 => (0, IoResult::Ok(0)),
            6 => (0, IoResult::Err(IoError::Unsupported)),
            7 => (0, IoResult::Cancelled),
            _ => (can, IoResult::Ok(can)),
        };
        full[at..at + n].copy_from_slice(&src[..n]);
        mem.set_len(at + n);
        self.served += n;
        result
    }
}

#[derive(Default)]
struct Mem {
    done: Vec<(ElementId, Completion)>,
    files: u32,
}

impl Reactor for Mem {
    fn submit(&mut self, subs: &mut Vec<Submission>, _names: &[u8]) {
        let mut feed = feed();
        for s in subs.drain(..) {
            let mut buf = s.buf;
            let result = match s.kind {
                Op::Open { .. } => {
                    self.files = self.files.wrapping_add(1);
                    IoResult::File { handle: Handle::new(self.files, 1), len: feed.claimed }
                }
                Op::Read { offset, at, len, .. } => match buf.as_mut() {
                    Some(b) => feed.read(&mut b.memory, offset, at as usize, len as usize),
                    None => IoResult::Err(IoError::Unsupported),
                },
                Op::Close { .. } => IoResult::Ok(0),
                Op::Cancel { .. } => continue,
                _ => IoResult::Err(IoError::Unsupported),
            };
            self.done.push((s.element, Completion { op: s.op, user: s.user, result, buf }));
        }
    }

    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        out.clear();
        out.append(&mut self.done);
    }

    fn due_in(&self) -> Option<Duration> {
        None
    }

    fn is_idle(&self) -> bool {
        self.done.is_empty()
    }
}

/// Unlinked at once and named by its descriptor, so a run leaves nothing behind.
struct Stat {
    file: File,
    path: PathBuf,
}

fn stat() -> &'static Stat {
    static STAT: OnceLock<Stat> = OnceLock::new();
    STAT.get_or_init(|| {
        let made = std::env::temp_dir().join(format!("pf-fuzz-tags-{}.bin", std::process::id()));
        let file = File::create(&made).expect("create the stat file");
        std::fs::remove_file(&made).expect("unlink the stat file");
        let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        Stat { file, path }
    })
}

fn factory() -> &'static ReactorFactory {
    static FACTORY: OnceLock<ReactorFactory> = OnceLock::new();
    FACTORY.get_or_init(|| Arc::new(|| Ok(Box::new(Mem::default()) as Box<dyn Reactor>)))
}

#[derive(Clone, Debug, PartialEq)]
struct Row {
    result: Result<(), ScanError>,
    format: Format,
    len: u64,
    props: Props,
    text: Vec<(String, String)>,
    pics: Vec<(String, usize)>,
    stored: usize,
}

fn row(out: &ScanOutcome<'_>) -> Row {
    match &out.result {
        Ok(r) => Row {
            result: Ok(()),
            format: r.format,
            len: r.file.len,
            props: r.props,
            text: r.tags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            pics: r.tags.pictures().iter().map(|p| (p.mime.to_string(), p.data.len())).collect(),
            stored: stored(r.tags.iter()),
        },
        Err(e) => Row {
            result: Err(*e),
            format: Format::Unknown,
            len: 0,
            props: Props::default(),
            text: Vec::new(),
            pics: Vec::new(),
            stored: 0,
        },
    }
}

fn stored<'a>(text: impl Iterator<Item = (&'a str, &'a str)>) -> usize {
    let mut last = core::ptr::null();
    text.map(|(k, v)| {
        let key = if k.as_ptr() == last { 0 } else { k.len() };
        last = k.as_ptr();
        key + v.len()
    })
    .sum()
}

fn scan(sc: &mut Scanner, path: &Path, copies: usize, fault: Fault) -> (Vec<Row>, usize) {
    {
        let mut f = feed();
        (f.fault, f.ops, f.served) = (fault, 0, 0);
    }
    let mut rows = Vec::with_capacity(copies);
    sc.scan(std::iter::repeat_n(path, copies), |out| rows.push(row(&out)));
    assert_eq!(rows.len(), copies, "a path reported {} times of {copies}", rows.len());
    (rows, feed().served)
}

fn bounded(r: &Row, claimed: u64, served: usize) {
    if r.result.is_err() {
        return;
    }
    assert_eq!(r.len, claimed, "a length other than the stat one");
    let entries = r.text.len() + r.pics.len();
    let bytes = r.stored + r.pics.iter().map(|(m, n)| m.len() + n).sum::<usize>();
    assert!(r.text.len() <= MAX_TEXT && r.pics.len() <= MAX_PICS, "past the store caps");
    assert!(entries <= served, "{entries} tags out of {served} bytes read");
    assert!(bytes <= 2 * served + 64 * entries, "{bytes} tag bytes out of {served} read");
    if r.format == Format::Unknown {
        assert!(entries == 0 && r.props == Props::default(), "an unknown file with tags");
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&[knob, build], rest)) = data.split_first_chunk::<2>() else {
        return;
    };
    let (file, want) = if build < 0x80 { (rest.to_vec(), None) } else { synth(build & 0x7F, rest) };
    // `PF_FUZZ_DUMP=path` writes the file scanned, to replay it outside the rig.
    if let Some(path) = std::env::var_os("PF_FUZZ_DUMP") {
        std::fs::write(path, &file).expect("dump the file");
    }
    let len = file.len() as u64;
    let lie = (knob >> 4) & 3;
    let claimed = [len, len / 2, len + 129, len + 70_000][usize::from(lie)];
    let fault = [Fault::None, Fault::None, Fault::Short, Fault::Failing][usize::from(knob >> 6)];
    let st = stat();
    if st.file.set_len(claimed).is_err() {
        return;
    }
    {
        let mut f = feed();
        (f.data, f.claimed) = (file, claimed);
    }

    let prefix = PREFIX[usize::from(knob & 3)];
    let tail_len = TAIL[usize::from((knob >> 2) & 3)];
    let cfg = ScanConfig { in_flight: 1, tail_len };
    let mut one = Scanner::with_reactor(cfg, Pool::bounded(prefix, 4), factory());
    let (base, served) = scan(&mut one, &st.path, 1, Fault::None);
    let base = &base[0];
    assert!(base.result.is_ok(), "a clean read failed with {:?}", base.result);
    bounded(base, claimed, served);
    if claimed == 0 {
        assert_eq!(base.format, Format::Unknown);
    }
    if let (Some((format, _, _)), 0) = (&want, lie) {
        assert_eq!(base.format, *format, "a built file sniffed as another format");
    }
    // Matroska plans from a SeekHead the prefix must hold, which 96 bytes do not.
    let seekhead = base.format != Format::Mkv || prefix > PREFIX[0];
    if let (Some((_, tags, pics)), 0, TAIL_ALL, true) = (&want, lie, tail_len, seekhead) {
        for t in tags {
            assert!(base.text.contains(t), "{t:?} written, not read back, got {:?}", base.text);
        }
        for p in pics {
            assert!(base.pics.contains(p), "picture {p:?} written, not read back");
        }
    }

    let (again, _) = scan(&mut one, &st.path, 1, Fault::None);
    assert_eq!(&again[0], base, "a rescan on the same scanner differs");

    let cfg = ScanConfig { in_flight: 2, tail_len };
    let mut two = Scanner::with_reactor(cfg, Pool::bounded(prefix, 16), factory());
    let (rows, _) = scan(&mut two, &st.path, BATCH, Fault::None);
    for r in &rows {
        assert_eq!(r, base, "a batch differs from one file alone");
    }

    let mut starved = Scanner::with_reactor(cfg, Pool::bounded(prefix, 3), factory());
    match fault {
        Fault::None => {
            let (rows, served) = scan(&mut starved, &st.path, BATCH, Fault::None);
            for r in &rows {
                bounded(r, claimed, served);
                assert!(!STRICT || r == base, "a starved pool reports another file");
            }
        }
        Fault::Short => {
            let (rows, served) = scan(&mut one, &st.path, 1, Fault::Short);
            let r = &rows[0];
            bounded(r, claimed, served);
            if claimed >= len {
                assert_eq!(r.format, base.format, "short reads sniff another format");
                assert!(!STRICT || r == base, "short reads report another file");
            }
        }
        Fault::Failing => {
            let (rows, served) = scan(&mut starved, &st.path, BATCH, Fault::Failing);
            for r in &rows {
                bounded(r, claimed, served);
            }
        }
    }
});

type Want = (Format, Vec<(String, String)>, Vec<(String, usize)>);

fn synth(sel: u8, rest: &[u8]) -> (Vec<u8>, Option<Want>) {
    let (&n, rest) = rest.split_first().unwrap_or((&0, &[]));
    let n = usize::from(n) % 30 + 1;
    let title: String =
        rest.iter().take(n).map(|&b| char::from(ALNUM[usize::from(b) % 62])).collect();
    let title = if title.is_empty() { "T".to_string() } else { title };
    let rest = rest.get(n..).unwrap_or(&[]);
    let (&p, rest) = rest.split_first().unwrap_or((&0, &[]));
    let art: Vec<u8> = (0..usize::from(p) * 16).map(|i| (i * 31 % 251) as u8).collect();
    let pic = (!art.is_empty()).then_some(("image/png", art.as_slice()));
    let artist: String = title.chars().rev().chain("A".chars()).collect();
    let lines = [format!("TITLE={title}"), format!("ARTIST={artist}")];
    let comments: Vec<&str> = lines.iter().map(String::as_str).collect();
    let t = title.as_str();
    let tag = |k: &str, v: &str| (k.to_string(), v.to_string());
    let vorbis = vec![tag("TITLE", t), tag("ARTIST", &artist)];
    let titled = vec![tag("TITLE", t)];
    let pics = if art.is_empty() { Vec::new() } else { vec![("image/png".to_string(), art.len())] };
    let ape = fx::ape_tag(&[("Title", t)]);
    let v1 = fx::id3v1(t, "V1", "V1", "1996", 2);
    let none = Vec::new;

    let (file, format, tags, pictures) = match sel % 20 {
        0 => (fx::wav(8_000, 1, 8, 300, &[("INAM", t)], false), Format::Wav, titled, none()),
        1 => (fx::wav(44_100, 2, 16, 400, &[("INAM", t)], true), Format::Wav, titled, none()),
        2 => (
            fx::aiff(b"AIFF", 44_100, 2, 16, 150, &[("NAME", t)], None),
            Format::Aiff,
            titled,
            none(),
        ),
        3 => {
            let f = fx::aiff(b"AIFC", 22_050, 1, 16, 150, &[("NAME", t)], Some(*b"sowt"));
            (f, Format::Aiff, titled, none())
        }
        s @ (4 | 5) => {
            let f = fx::aiff_id3(44_100, 2, 300, &[("NAME", t)], &[("TIT2", &artist)], s == 5);
            (f, Format::Aiff, vec![tag("TITLE", t), tag("TITLE", &artist)], none())
        }
        6 => (fx::flac(48_000, 2, 24, 96_000, &comments, pic), Format::Flac, vorbis, pics),
        7 => {
            let head = fx::id3v2(4, &[("TIT2", "Prefixed")], None);
            let f = [head, fx::flac(44_100, 2, 16, 44_100, &comments, None)].concat();
            (f, Format::Flac, vorbis, none())
        }
        s @ (8 | 9) => {
            let v1 = Some((t, "V1", "V1", "1998", 9));
            let f = fx::mp3(44_100, 128, 3, &[("TIT2", t)], pic, s == 9, v1, &[("Title", t)]);
            (f, Format::Mp3, vec![tag("TITLE", t); 3], pics)
        }
        s @ (10 | 11) => {
            let f = fx::m4a(44_100, 2, 88_200, &[("\u{a9}nam", t)], Some(3), None, s == 10);
            (f, Format::Mp4, titled, none())
        }
        12 => {
            let f = fx::ogg_opus(2, 312, 44_100, &comments, pic, 48_000 * 5 + 312);
            (f, Format::OggOpus, vorbis, pics)
        }
        13 => (fx::ogg_vorbis(2, 44_100, &comments, 44_100 * 4), Format::OggVorbis, vorbis, none()),
        14 => (
            fx::ogg_speex(16_000, 1, &comments, None, 16_000 * 3),
            Format::OggSpeex,
            vorbis,
            none(),
        ),
        15 => (fx::ogg_flac(48_000, 2, 288_000, &comments, pic), Format::OggFlac, vorbis, pics),
        16 => (fx::mkv(48_000.0, 2, 3_000.0, &[("TITLE", t)], pic), Format::Mkv, titled, pics),
        17 => {
            let f = fx::mkv_trailing(44_100.0, 1, 1_250.0, &[("TITLE", t)], pic, 1500);
            (f, Format::Mkv, titled, pics)
        }
        18 => {
            let f = [fx::ape_file(3990, 44_100, 2, 73_728, 3, 4_644), vec![0x71; 300], ape, v1]
                .concat();
            (f, Format::Ape, vec![tag("TITLE", t); 2], none())
        }
        _ => {
            let (f, format) = match rest.first().map_or(0, |b| b % 3) {
                0 => {
                    let wv = fx::wavpack(44_100, 2, 88_200, &[fx::wv_sub_block(0x0A, &[0x33; 64])]);
                    ([wv, ape].concat(), Format::WavPack)
                }
                1 => (
                    [fx::mpc_sv8(0, 2, 88_200, 1_024, &[]), vec![0x2E; 300], ape].concat(),
                    Format::Musepack,
                ),
                _ => ([fx::mpc_sv7(0, 40, true, 500), ape].concat(), Format::Musepack),
            };
            (f, format, titled, none())
        }
    };
    let mut file = file;
    let corrupt = rest.get(1..).unwrap_or(&[]);
    if corrupt.is_empty() {
        return (file, Some((format, tags, pictures)));
    }
    let (triples, rest) = corrupt.as_chunks::<3>();
    for &[lo, hi, x] in triples.iter().take(256) {
        let at = usize::from(u16::from_le_bytes([lo, hi])) % file.len().max(1);
        if let Some(b) = file.get_mut(at) {
            *b ^= x;
        }
    }
    if let [cut] = rest {
        file.truncate(file.len() * usize::from(*cut) / 256);
    }
    (file, None)
}
