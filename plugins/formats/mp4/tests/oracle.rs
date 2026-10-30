//! Cross-checks `Mp4Reader` sample enumeration against ffprobe over the committed fixtures.
//! Skips if ffprobe is absent.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;

use pf_mp4::Mp4Reader;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

fn head_through_moov(file: &[u8]) -> Vec<u8> {
    let mut at = 0usize;
    while at + 8 <= file.len() {
        let size =
            u32::from_be_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]) as usize;
        let kind = &file[at + 4..at + 8];
        if kind == b"mdat" {
            return file[..at].to_vec();
        }
        let advance = if size == 0 {
            file.len() - at
        } else if size == 1 {
            u64::from_be_bytes(file[at + 8..at + 16].try_into().unwrap()) as usize
        } else {
            size
        };
        if advance == 0 {
            break;
        }
        at += advance;
    }
    file.to_vec()
}

#[derive(Debug, PartialEq, Eq)]
struct OraclePacket {
    stream_index: u32,
    size: usize,
    offset: u64,
    pts: i64,
    dts: i64,
    keyframe: bool,
}

fn ffprobe_lines(name: &str, args: &[&str]) -> Option<Vec<String>> {
    let ffprobe = which("ffprobe")?;
    let out = std::process::Command::new(ffprobe)
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-of", "compact"])
        .args(args)
        .arg(fixture(name))
        .output()
        .expect("run ffprobe");
    assert!(out.status.success(), "ffprobe failed on {name}: {:?}", out.status);
    Some(String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
}

fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split('|')
        .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        .unwrap_or_else(|| panic!("no {key} in {line:?}"))
}

fn ffprobe_enumerate(name: &str) -> Option<(Vec<OraclePacket>, Vec<u32>)> {
    let streams = ffprobe_lines(name, &["-show_entries", "stream=index,time_base"])?;
    let timescales = streams
        .iter()
        .map(|l| {
            field(l, "time_base").split_once('/').expect("a ratio").1.parse().expect("a timescale")
        })
        .collect();
    let lines =
        ffprobe_lines(name, &["-show_entries", "packet=stream_index,pts,dts,size,pos,flags"])?;
    let mut packets: Vec<OraclePacket> = lines
        .iter()
        .map(|l| OraclePacket {
            stream_index: field(l, "stream_index").parse().expect("an index"),
            size: field(l, "size").parse().expect("a size"),
            offset: field(l, "pos").parse().expect("a position"),
            pts: field(l, "pts").parse().expect("mp4 sample has a pts"),
            dts: field(l, "dts").parse().expect("mp4 sample has a dts"),
            keyframe: field(l, "flags").starts_with('K'),
        })
        .collect();
    packets.sort_by_key(|p| p.offset);
    Some((packets, timescales))
}

fn assert_agrees_with_ffmpeg(name: &str) {
    let Some((oracle, oracle_timescales)) = ffprobe_enumerate(name) else {
        eprintln!("ffprobe absent, skipping the {name} oracle");
        return;
    };
    let file = fixture_bytes(name);
    let head = head_through_moov(&file);

    let ours = Mp4Reader::new(&head).unwrap_or_else(|e| panic!("our resolve {name}: {e:?}"));
    let our_timescales: Vec<u32> = ours.tracks().iter().map(|t| t.timescale).collect();

    assert_eq!(
        ours.samples().len(),
        oracle.len(),
        "{name}: sample count must match ffmpeg ({} vs {})",
        ours.samples().len(),
        oracle.len()
    );
    assert_eq!(
        our_timescales, oracle_timescales,
        "{name}: per-track media timescale must match ffmpeg"
    );

    for (i, (o, x)) in ours.samples().iter().zip(oracle.iter()).enumerate() {
        assert_eq!(o.track_index as u32, x.stream_index, "{name}: sample {i} track/stream index");
        assert_eq!(o.size as usize, x.size, "{name}: sample {i} size (mdat slice length)");
        assert_eq!(o.offset, x.offset, "{name}: sample {i} file offset");
        assert_eq!(o.pts, x.pts, "{name}: sample {i} pts (dts + ctts - edit_shift, ticks)");
        assert_eq!(o.dts, x.dts, "{name}: sample {i} dts (stts sum - edit_shift, ticks)");
        assert_eq!(o.sync, x.keyframe, "{name}: sample {i} sync/keyframe flag");
    }
}

#[test]
fn tiny_h264_matches_ffmpeg() {
    assert_agrees_with_ffmpeg("tiny_h264.mp4");
    let file = fixture_bytes("tiny_h264.mp4");
    let ours = Mp4Reader::new(&head_through_moov(&file)).unwrap();
    assert!(
        ours.samples().iter().all(|s| s.pts == s.dts),
        "no ctts box → pts == dts for every sample"
    );
    let syncs = ours.samples().iter().filter(|s| s.sync).count();
    assert_eq!(syncs, 1, "one keyframe (the single IDR)");
    assert_eq!(ours.samples().len(), 15, "15 frames");
}

#[test]
fn bframes_h264_matches_ffmpeg() {
    assert_agrees_with_ffmpeg("bframes_h264.mp4");
    let file = fixture_bytes("bframes_h264.mp4");
    let ours = Mp4Reader::new(&head_through_moov(&file)).unwrap();
    assert!(
        ours.samples().iter().any(|s| s.pts != s.dts),
        "ctts box present → some sample has pts != dts (B-frame reorder)"
    );
}

#[test]
fn tiny_av_matches_ffmpeg() {
    assert_agrees_with_ffmpeg("tiny_av.mp4");
}

#[test]
fn mp4v_matches_ffmpeg() {
    for name in ["mp4v_mpeg4.mp4", "mp4v_mpeg2.mp4", "mp4v_mpeg1.mp4"] {
        assert_agrees_with_ffmpeg(name);
    }
}

#[test]
fn ffprobe_second_opinion() {
    let Some(ffprobe) = which("ffprobe") else {
        eprintln!("ffprobe absent, skipping the live-oracle second opinion");
        return;
    };
    for name in ["tiny_h264.mp4", "bframes_h264.mp4"] {
        let path = fixture(name);
        let out = std::process::Command::new(&ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "frame=key_frame",
                "-of",
                "csv=p=0",
            ])
            .arg(&path)
            .output()
            .expect("run ffprobe");
        assert!(out.status.success(), "ffprobe failed on {name}: {:?}", out.status);
        let text = String::from_utf8_lossy(&out.stdout);
        // ffprobe 8.x may append a stray comma to the first field, so take the leading digit.
        let flags: Vec<char> = text
            .lines()
            .filter_map(|l| l.trim().chars().next())
            .filter(|c| c.is_ascii_digit())
            .collect();
        let ff_frames = flags.len();
        let ff_keys = flags.iter().filter(|&&c| c == '1').count();

        let file = fixture_bytes(name);
        let ours = Mp4Reader::new(&head_through_moov(&file)).unwrap();
        assert_eq!(ours.samples().len(), ff_frames, "{name}: frame count vs ffprobe");
        let our_keys = ours.samples().iter().filter(|s| s.sync).count();
        assert_eq!(our_keys, ff_keys, "{name}: keyframe count vs ffprobe");
        eprintln!("ffprobe agrees on {name}: {ff_frames} frames, {ff_keys} keyframes");
    }
}

fn which(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(exe);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}
