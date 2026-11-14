//! The packed Cues tables hold what the plain parse reads, for every Matroska fixture checked in
//! and for the Cues the writer makes.

// Test code. The allocation and file bans are for element hot paths.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};

use pf_mkv::ebml::{id, read_element_header};
use pf_mkv::{Cue, Cues, MatroskaWriter, TrackConfig, parse_cue_tracks, parse_seek_head};

/// The directories of the repository that hold Matroska fixtures, from this crate.
const FIXTURES: [&str; 5] = [
    "../../../lib/open/tests/fixtures",
    "../../audio/opus/tests/fixtures",
    "../../video/hdr/tests/fixtures",
    "../bare/tests/fixtures",
    "../mp4/tests/fixtures",
];

fn matroska(dir: &Path) -> Vec<PathBuf> {
    let is = |p: &PathBuf| {
        let ext = p.extension().and_then(|e| e.to_str());
        matches!(ext, Some("mkv" | "webm" | "mka"))
    };
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    let mut files: Vec<PathBuf> = entries.map(|e| e.expect("an entry").path()).filter(is).collect();
    files.sort();
    files
}

/// The Cues element of `file` with what its SeekHead says of the Segment, none without them.
fn cues_of(file: &[u8]) -> Option<(&[u8], u64, u64)> {
    let cluster = file.windows(4).position(|w| w == id::CLUSTER).unwrap_or(file.len());
    let info = parse_seek_head(&file[..cluster])?;
    let at = usize::try_from(info.segment_data_start + info.cues_pos?).ok()?;
    let h = read_element_header(file, at).ok().filter(|h| h.id == id::CUES)?;
    let end = h.data_end().ok()??.min(file.len());
    Some((&file[at..end], info.segment_data_start, info.timestamp_scale))
}

/// The cues the tables of `bytes` hold against the plain parse, by time then track. The count
/// of cues.
fn same(bytes: &[u8], start: u64, scale: u64, name: &str) -> usize {
    let mut want = parse_cue_tracks(bytes, start, scale);
    for c in &mut want {
        c.relative = c.relative.filter(|&r| r < u64::from(u32::MAX));
    }
    want.sort_by_key(|c| (c.time_ns, c.track));
    let got = Cues::parse(bytes, start, scale);
    assert_eq!(got.iter().collect::<Vec<Cue>>(), want, "{name}");
    assert_eq!(got.len(), want.len(), "{name}");
    for v in got.tracks() {
        let own: Vec<Cue> = want.iter().filter(|c| c.track == v.track()).copied().collect();
        assert_eq!(v.iter().collect::<Vec<Cue>>(), own, "{name} track {}", v.track());
    }
    assert!(got.bytes() <= want.len() * 24 + got.tracks().count() * 24, "{name}");
    want.len()
}

#[test]
fn every_fixtures_cues_pack_to_what_the_plain_parse_reads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (mut files, mut cued) = (0, 0);
    for path in FIXTURES.iter().flat_map(|dir| matroska(&root.join(dir))) {
        let file = std::fs::read(&path).expect("a fixture");
        let name = path.display().to_string();
        files += 1;
        if let Some((bytes, start, scale)) = cues_of(&file) {
            cued += usize::from(same(bytes, start, scale, &name) > 0);
        }
    }
    assert!(files >= 10 && cued >= 5, "{cued} of {files} fixtures cued");
}

/// A film of the writer's, video keys and a caption track with every block placed.
#[test]
fn the_writers_cues_pack_to_what_the_plain_parse_reads() {
    const MS: u64 = 1_000_000;
    let text = TrackConfig::audio(2, "S_TEXT/UTF8", Vec::new(), 0.0, 0, 0);
    let mut w = MatroskaWriter::new(vec![TrackConfig::vp8(1, 320, 240), text]);
    w.enable_cues();
    w.set_duration_ns(16_000 * MS);
    let mut out = Vec::new();
    w.write_header(&mut out).expect("header");
    for t in (0..16_000u64).step_by(40) {
        w.write_frame(&mut out, 1, t * MS, &[7u8; 300], t % 1_000 == 0).expect("a picture");
        if t % 1_200 == 0 {
            w.write_frame(&mut out, 2, t * MS, b"a line", true).expect("a caption");
        }
    }
    w.finalize(&mut out);
    let (bytes, start, scale) = cues_of(&out).expect("the writer's Cues");
    let n = same(bytes, start, scale, "written");
    assert_eq!(n, 16 + 14, "a key a second and a caption every 1.2 s");
    let cues = Cues::parse(bytes, start, scale);
    let captions = cues.track(2).expect("the captions are cued");
    assert!(captions.placed(), "every caption block is placed");
    assert_eq!(captions.len(), 14);
    let keys = cues.track(1).expect("the keys are cued");
    assert_eq!((keys.len(), keys.duration_ns(0)), (16, None));
    assert!(cues.bytes() < cues.len() * size_of::<Cue>() / 2, "{} octets", cues.bytes());
}
