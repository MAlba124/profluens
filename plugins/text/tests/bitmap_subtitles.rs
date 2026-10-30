//! The bitmap subtitle decoders against ffmpeg.

// Test code, fixtures and ffmpeg's output live on the heap.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pf_harness::Harness;
use pf_mkv::reader::MatroskaReader;
use pf_text::pgs::{DisplaySet, decode_bitmap};
use pf_text::{DvbSubDec, VobSubDec};
use profluens_core::buffer::BufferFlags;
use profluens_core::element::Element;
use profluens_core::time::Timestamp;

const W: usize = 200;
const H: usize = 40;
const X: usize = 260;
const Y: usize = 480;

/// ffmpeg without the devshell's library path, which breaks the system binary.
fn ffmpeg() -> Option<Command> {
    let runs = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    runs.then(|| {
        let mut c = Command::new("ffmpeg");
        c.env_remove("LD_LIBRARY_PATH").args(["-y", "-hide_banner", "-loglevel", "error"]);
        c
    })
}

fn tmp_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("bitmap_subtitles");
    std::fs::create_dir_all(&dir).expect("tmp dir");
    dir
}

fn sup_segment(kind: u8, pts: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = b"PG".to_vec();
    out.extend_from_slice(&pts.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn rle_line(runs: &[(u8, u16)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(c, n) in runs {
        match (c, n) {
            (c, 1) if c != 0 => out.push(c),
            (0, n) if n < 64 => out.extend_from_slice(&[0, n as u8]),
            (0, n) => out.extend_from_slice(&[0, 0x40 | (n >> 8) as u8, n as u8]),
            (c, n) if n < 64 => out.extend_from_slice(&[0, 0x80 | n as u8, c]),
            (c, n) => out.extend_from_slice(&[0, 0xC0 | (n >> 8) as u8, n as u8, c]),
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

fn sup() -> Vec<u8> {
    let mut rle = Vec::new();
    for y in 0..H {
        let runs: &[(u8, u16)] =
            if (15..25).contains(&y) { &[(1, 50), (2, 100), (1, 50)] } else { &[(1, W as u16)] };
        rle.extend(rle_line(runs));
    }
    let window = |w: u16| {
        let mut p = vec![1, 0];
        for v in [X as u16, Y as u16, w, H as u16] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        p
    };
    let pcs = |number: u16, state: u8, objects: u8| {
        let mut p = Vec::new();
        p.extend_from_slice(&720u16.to_be_bytes());
        p.extend_from_slice(&576u16.to_be_bytes());
        p.push(0x10);
        p.extend_from_slice(&number.to_be_bytes());
        p.extend_from_slice(&[state, 0, 0, objects]);
        if objects > 0 {
            p.extend_from_slice(&[0, 0, 0, 0]);
            p.extend_from_slice(&(X as u16).to_be_bytes());
            p.extend_from_slice(&(Y as u16).to_be_bytes());
        }
        p
    };
    let show = |pts: u32| {
        let pds = [0, 0, 1, 81, 240, 90, 255, 2, 235, 128, 128, 255];
        let mut body = (W as u16).to_be_bytes().to_vec();
        body.extend_from_slice(&(H as u16).to_be_bytes());
        body.extend_from_slice(&rle);
        let mut ods = vec![0, 0, 0, 0xC0];
        ods.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        ods.extend_from_slice(&body);
        [
            sup_segment(0x16, pts, &pcs(0, 0x80, 1)),
            sup_segment(0x17, pts, &window(W as u16)),
            sup_segment(0x14, pts, &pds),
            sup_segment(0x15, pts, &ods),
            sup_segment(0x80, pts, &[]),
        ]
        .concat()
    };
    let clear = |pts: u32| {
        [
            sup_segment(0x16, pts, &pcs(1, 0, 0)),
            sup_segment(0x17, pts, &window(W as u16)),
            sup_segment(0x80, pts, &[]),
        ]
        .concat()
    };
    [show(90_000), clear(270_000), show(360_000), clear(540_000)].concat()
}

fn transcode(codec: &str) -> Option<PathBuf> {
    let mut ff = ffmpeg()?;
    let dir = tmp_dir();
    // One input per test, nextest runs them at once in this shared directory.
    let sup_path = dir.join(format!("cue-{codec}.sup"));
    std::fs::write(&sup_path, sup()).expect("write sup");
    let out = dir.join(format!("{codec}.mkv"));
    let status = ff
        .arg("-fix_sub_duration")
        .arg("-i")
        .arg(&sup_path)
        .args(["-copyts", "-c:s", codec])
        .arg(&out)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg -c:s {codec}");
    Some(out)
}

fn ffmpeg_render(mkv: &Path, stream: &str, secs: f64, w: usize, h: usize) -> Vec<u8> {
    let out = tmp_dir()
        .join(format!("{}.rgb", mkv.file_stem().and_then(|s| s.to_str()).unwrap_or("render")));
    let status = ffmpeg()
        .expect("ffmpeg")
        .args([
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=black:s={w}x{h}:r=25:d={},format=rgba", secs + 2.0),
        ])
        .arg("-i")
        .arg(mkv)
        .args(["-filter_complex", &format!("[0:v][1:{stream}]overlay=format=rgb,format=rgb24")])
        .args(["-ss", &format!("{secs}"), "-frames:v", "1", "-f", "rawvideo"])
        .arg(&out)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg render");
    let rgb = std::fs::read(&out).expect("render");
    assert_eq!(rgb.len(), w * h * 3);
    rgb
}

struct Shown {
    ds: DisplaySet,
    pts: Timestamp,
    duration: Timestamp,
}

fn decode(mkv: &Path, codec_id: &str, dec: impl Element + 'static) -> Vec<Shown> {
    let mut reader = MatroskaReader::new();
    reader.push(&std::fs::read(mkv).expect("read mkv")).expect("parse mkv");
    let track = reader
        .tracks()
        .iter()
        .find(|t| t.codec_id == codec_id)
        .cloned()
        .expect("the subtitle track");
    let mut h = Harness::new(dec);
    let mut out = Vec::new();
    let feed = |h: &mut Harness, data: &[u8], flags, pts: u64, duration: Option<u64>| {
        let mut b = h.alloc(data);
        b.flags = flags;
        b.pts = Timestamp::from_nanos(pts);
        b.duration = duration.map_or(Timestamp::NONE, Timestamp::from_nanos);
        h.push(b).expect("push");
    };
    if !track.codec_private.is_empty() {
        feed(&mut h, &track.codec_private, BufferFlags::HEADER, 0, None);
    }
    while let Some(frame) = reader.next_frame() {
        if frame.track_number == track.track_number {
            feed(&mut h, &frame.data, BufferFlags::empty(), frame.pts_ns, frame.duration_ns);
            out.extend(h.drain_outputs());
        }
    }
    out.extend(h.eos().expect("eos"));
    out.iter()
        .map(|b| Shown {
            ds: decode_bitmap(b.memory.data()).expect("a well formed bitmap"),
            pts: b.pts,
            duration: b.duration,
        })
        .collect()
}

fn composite(ds: &DisplaySet) -> Vec<u8> {
    let (w, h) = (ds.video_width as usize, ds.video_height as usize);
    let mut out = vec![0u8; w * h * 3];
    for y in 0..ds.height as usize {
        for x in 0..ds.width as usize {
            let (fx, fy) = (ds.x as usize + x, ds.y as usize + y);
            if fx >= w || fy >= h {
                continue;
            }
            let s = &ds.rgba[(y * ds.width as usize + x) * 4..][..4];
            let d = &mut out[(fy * w + fx) * 3..][..3];
            for c in 0..3 {
                d[c] = ((u32::from(s[c]) * u32::from(s[3]) + 127) / 255) as u8;
            }
        }
    }
    out
}

fn mismatch(a: &[u8], b: &[u8], tolerance: u8) -> (f64, u8) {
    let (mut off, mut worst) = (0usize, 0u8);
    for (p, q) in a.as_chunks::<3>().0.iter().zip(b.as_chunks::<3>().0) {
        let d = p.iter().zip(q).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        worst = worst.max(d);
        off += usize::from(d > tolerance);
    }
    (off as f64 / (a.len() / 3) as f64, worst)
}

fn rgb_at(frame: &[u8], w: usize, x: usize, y: usize) -> [u8; 3] {
    frame[(y * w + x) * 3..][..3].try_into().expect("three bytes")
}

fn check_against_ffmpeg(mkv: &Path, shown: &[Shown]) {
    let first = shown.iter().find(|s| !s.ds.is_clear()).expect("a bitmap");
    assert_eq!(first.pts, Timestamp::from_secs(1), "the first cue's time");
    assert_eq!((first.ds.video_width, first.ds.video_height), (720, 576), "the authoring grid");
    let ours = composite(&first.ds);
    let theirs = ffmpeg_render(mkv, "s", 2.0, 720, 576);
    for (x, y, what) in [
        (X + 10, Y + 5, "red"),
        (X + 100, Y + 20, "white"),
        (X - 1, Y, "left of it"),
        (X, Y + H, "below it"),
    ] {
        let (a, b) = (rgb_at(&ours, 720, x, y), rgb_at(&theirs, 720, x, y));
        assert!(
            a.iter().zip(&b).all(|(p, q)| p.abs_diff(*q) <= 4),
            "{what} at ({x}, {y}): ours {a:?} ffmpeg {b:?}"
        );
    }
    assert!(rgb_at(&ours, 720, X + 10, Y + 5)[0] > 240, "the box is red");
    assert!(rgb_at(&ours, 720, X + 100, Y + 20).iter().all(|&c| c > 240), "the bar is white");
    let (share, worst) = mismatch(&ours, &theirs, 4);
    assert!(share == 0.0, "{:.4} % of the frame differs, by up to {worst}", share * 100.0);
}

#[test]
fn vobsub_from_ffmpeg_matches_ffmpegs_render() {
    let Some(mkv) = transcode("dvdsub") else {
        eprintln!("skipping, ffmpeg does not run here");
        return;
    };
    let shown = decode(&mkv, "S_VOBSUB", VobSubDec::new());
    assert_eq!(shown.len(), 2, "two cues");
    assert_eq!(shown[0].duration, Timestamp::from_nanos(175 * 1024 * 1_000_000_000 / 90_000));
    assert_eq!(shown[1].pts, Timestamp::from_secs(4));
    assert_eq!(
        (shown[0].ds.x, shown[0].ds.y, shown[0].ds.width, shown[0].ds.height),
        (X as u32, Y as u32, W as u32, H as u32)
    );
    check_against_ffmpeg(&mkv, &shown);
}

#[test]
fn dvb_from_ffmpeg_matches_ffmpegs_render() {
    let Some(mkv) = transcode("dvbsub") else {
        eprintln!("skipping, ffmpeg does not run here");
        return;
    };
    let shown = decode(&mkv, "S_DVBSUB", DvbSubDec::new());
    let shows: Vec<&Shown> = shown.iter().filter(|s| !s.ds.is_clear()).collect();
    assert_eq!(shows.len(), 2, "two cues");
    assert_eq!(shown.len(), 4, "and a clear after each");
    assert_eq!(shows[0].duration, Timestamp::from_secs(2), "the Block's, under the page time out");
    assert_eq!(
        (shows[0].ds.x, shows[0].ds.y, shows[0].ds.width, shows[0].ds.height),
        (X as u32, Y as u32, W as u32, H as u32)
    );
    assert_eq!(shown[1].pts, Timestamp::from_secs(3), "the clear");
    check_against_ffmpeg(&mkv, &shown);
}

#[test]
fn a_real_vobsub_sample_matches_ffmpegs_render() {
    real_sample("PF_VOBSUB_SAMPLE", "S_VOBSUB", VobSubDec::new());
}

#[test]
fn a_real_dvb_sample_matches_ffmpegs_render() {
    real_sample("PF_DVB_SAMPLE", "S_DVBSUB", DvbSubDec::new());
}

fn real_sample(var: &str, codec_id: &str, dec: impl Element + 'static) {
    let (Some(path), Some(_)) = (std::env::var_os(var), ffmpeg()) else {
        eprintln!("skipping, {var} is unset or ffmpeg does not run here");
        return;
    };
    let mkv = PathBuf::from(path);
    let shown = decode(&mkv, codec_id, dec);
    let first = shown.iter().find(|s| !s.ds.is_clear()).expect("a cue");
    let (w, h) = (first.ds.video_width as usize, first.ds.video_height as usize);
    let at = first.pts.nanos().expect("timed") as f64 / 1e9 + 0.1;
    let ours = composite(&first.ds);
    let theirs = ffmpeg_render(&mkv, "s:0", at, w, h);
    let (share, worst) = mismatch(&ours, &theirs, 8);
    let ink = ours.as_chunks::<3>().0.iter().filter(|p| p.iter().any(|&c| c > 0)).count();
    let (x, y) = (first.ds.x, first.ds.y);
    eprintln!(
        "real sample, cue at {at:.3} s, {}x{} at ({x}, {y}), {ink} inked, {:.4} % off, worst {worst}",
        first.ds.width,
        first.ds.height,
        share * 100.0
    );
    assert!(ink > 100, "the cue drew {ink} pixels");
    assert!(share < 0.001, "{:.4} % of the frame differs, by up to {worst}", share * 100.0);
}
