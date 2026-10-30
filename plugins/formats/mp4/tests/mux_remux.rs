//! Remux through `mp4mux` and `mkvmuxn` from ffmpeg-made files, checked by ffmpeg.
//! Skipped where ffmpeg is absent. It runs without the devshell `LD_LIBRARY_PATH`, which breaks it.

// Tests own their fixtures, temp files and staging Vecs.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pf_mkv::{MatroskaReader, MkvDemux, MkvMux, MkvMuxN};
use pf_mp4::mux::{Mode, Mp4Mux};
use pf_mp4::{Mp4Demux, Mp4Reader};
use profluens_core::id::SinkPad;
use profluens_elements::io::{FileSink, FileSrc};

fn dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mp4mux");
    std::fs::create_dir_all(&dir).expect("tmp dir");
    dir
}

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn ffmpeg(name: &str, args: &[&str]) -> PathBuf {
    let path = dir().join(name);
    let status = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .arg(&path)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed to build {name}");
    path
}

fn source(name: &str, codecs: &[&str]) -> PathBuf {
    let mut args = vec!["-f", "lavfi", "-i", "testsrc2=size=160x120:rate=25:duration=2"];
    args.extend(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=2"]);
    args.extend(["-map", "0", "-map", "1"]);
    args.extend(codecs);
    ffmpeg(name, &args)
}

fn probe(path: &Path) -> (Vec<String>, String) {
    let out = Command::new("ffprobe")
        .env_remove("LD_LIBRARY_PATH")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-show_entries",
            "stream=codec_type,codec_name,nb_read_frames",
        ])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    let lines = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    (lines, String::from_utf8_lossy(&out.stderr).into_owned())
}

fn durations(path: &Path) -> Vec<f64> {
    let out = Command::new("ffprobe")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-show_entries", "stream=duration", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect()
}

fn md5s(path: &Path, map: &str, copy: bool) -> Vec<(i64, String)> {
    let mut cmd = Command::new("ffmpeg");
    cmd.env_remove("LD_LIBRARY_PATH").args(["-v", "error", "-i"]).arg(path).args(["-map", map]);
    if copy {
        cmd.args(["-c", "copy"]);
    }
    let out = cmd.args(["-f", "framemd5", "-"]).output().expect("run ffmpeg");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut tb = (1i64, 1000i64);
    let mut rows = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("#tb 0: ") {
            let (n, d) = rest.split_once('/').expect("a time base");
            tb = (n.trim().parse().unwrap(), d.trim().parse().unwrap());
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        let pts: i64 = cols[2].parse().unwrap();
        rows.push(((pts * tb.0 * 1000 + tb.1 / 2).div_euclid(tb.1), cols[5].to_string()));
    }
    rows
}

fn mkv_head(file: &[u8]) -> Vec<u8> {
    let at = file.windows(4).position(|w| w == [0x1F, 0x43, 0xB6, 0x75]).expect("a Cluster");
    file[..at].to_vec()
}

fn mp4_head(file: &[u8]) -> Vec<u8> {
    let mut at = 0usize;
    let (mut mdat, mut moov) = (None, None);
    while at + 8 <= file.len() {
        let size = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
        let size = match size {
            0 => file.len() - at,
            1 => u64::from_be_bytes(file[at + 8..at + 16].try_into().unwrap()) as usize,
            n => n,
        };
        match &file[at + 4..at + 8] {
            b"mdat" if mdat.is_none() => mdat = Some(at),
            b"moov" => moov = Some((at, size)),
            _ => {}
        }
        if size < 8 {
            break;
        }
        at += size;
    }
    let (m, len) = moov.expect("a moov");
    let prefix = mdat.unwrap_or(file.len()).min(m + len);
    let mut head = file[..prefix].to_vec();
    if m >= prefix {
        head.extend_from_slice(&file[m..m + len]);
    }
    head
}

fn mkv_to_mp4(input: &Path, output: &Path, mode: Mode) {
    let file = std::fs::read(input).expect("read source");
    let head = mkv_head(&file);
    let mut r = MatroskaReader::new();
    r.push(&head).expect("header parses");
    let names: Vec<String> = r
        .tracks()
        .iter()
        .filter(|t| {
            t.codec_id.starts_with("V_")
                || t.codec_id.starts_with("A_")
                || t.codec_id == "S_TEXT/UTF8"
        })
        .map(|t| format!("src_track{}", t.track_number))
        .collect();
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(input));
    let demux = p.add(MkvDemux::new(head));
    p.link(src.src(FileSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");
    let added = p.preroll().expect("preroll").srcs;
    let mux = p.add(Mp4Mux::tracks(mode, names.len()));
    let sink = p.add(FileSink::new(output));
    for (i, name) in names.iter().enumerate() {
        let ap = added.iter().find(|a| &a.name == name).expect("a pad per track");
        p.link(ap.element.src(ap.pad), mux.sink(Mp4Mux::INS[i])).expect("demux -> mux");
    }
    p.link(mux.src(Mp4Mux::OUT), sink.sink(FileSink::IN)).expect("mux -> sink");
    run_within(p, output);
}

fn run_within(mut p: profluens_core::pipeline::Pipeline, output: &Path) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(p.run().map(|_| ()).map_err(|e| format!("{e:?}")));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(60)) {
        Ok(r) => r.expect("remux runs"),
        Err(_) => panic!("remux to {} did not finish in a minute", output.display()),
    }
}

fn mkv_to_mkv(input: &Path, output: &Path) {
    let file = std::fs::read(input).expect("read source");
    let head = mkv_head(&file);
    let mut r = MatroskaReader::new();
    r.push(&head).expect("header parses");
    let names: Vec<String> =
        r.tracks().iter().map(|t| format!("src_track{}", t.track_number)).collect();
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(input));
    let demux = p.add(MkvDemux::new(head));
    p.link(src.src(FileSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");
    let added = p.preroll().expect("preroll").srcs;
    let mux = p.add(MkvMux::multi(names.len()));
    let sink = p.add(FileSink::new(output));
    for (i, name) in names.iter().enumerate() {
        let ap = added.iter().find(|a| &a.name == name).expect("a pad per track");
        p.link(ap.element.src(ap.pad), mux.sink(MkvMuxN::INS[i])).expect("demux -> mux");
    }
    p.link(mux.src(MkvMuxN::OUT), sink.sink(FileSink::IN)).expect("mux -> sink");
    run_within(p, output);
}

fn mp4_to(input: &Path, output: &Path, mkv: bool, mode: Mode) {
    let file = std::fs::read(input).expect("read source");
    let head = mp4_head(&file);
    let tables = Mp4Reader::new(&head).expect("tables").tables();
    let n = tables.tracks().len();
    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(input));
    let demux = p.add(Mp4Demux::new(head));
    let pool = tables.remux_pool(&(0..n).collect::<Vec<_>>());
    p.set_element_pool_bytes(demux, 64 << 10, 512, pool.bytes);
    p.set_element_reserve(demux, pool.largest as usize, pool.held);
    p.link(src.src(FileSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src -> demux");
    let added = p.preroll().expect("preroll").srcs;
    assert_eq!(added.len(), n);
    let sink = p.add(FileSink::new(output));
    let (mux, pads, out): (_, [SinkPad; 8], _) = match mkv {
        true => {
            let m = p.add(MkvMux::multi(n));
            (m, MkvMuxN::INS, m.src(MkvMuxN::OUT))
        }
        false => {
            let m = p.add(Mp4Mux::tracks(mode, n));
            (m, Mp4Mux::INS, m.src(Mp4Mux::OUT))
        }
    };
    for (i, ap) in added.iter().enumerate() {
        p.link(ap.element.src(ap.pad), mux.sink(pads[i])).expect("demux -> mux");
    }
    p.link(out, sink.sink(FileSink::IN)).expect("mux -> sink");
    run_within(p, output);
}

fn same_media(src: &Path, out: &Path, kinds: &[&str]) {
    let (want, _) = probe(src);
    let (got, errors) = probe(out);
    assert!(errors.trim().is_empty(), "ffprobe errors on {}: {errors}", out.display());
    assert_eq!(got.len(), want.len(), "streams of {}", out.display());
    assert_eq!(got[..kinds.len()], want[..kinds.len()], "streams of {}", out.display());
    for (k, kind) in kinds.iter().enumerate() {
        let map = format!("0:{k}");
        match *kind {
            "video" => {
                let (a, b) = (md5s(src, &map, false), md5s(out, &map, false));
                assert!(!a.is_empty());
                assert_eq!(b.len(), a.len(), "pictures of {}", out.display());
                for (x, y) in a.iter().zip(&b) {
                    assert_eq!(x.1, y.1, "picture at {} ms of {}", x.0, out.display());
                    assert!((x.0 - y.0).abs() <= 1, "picture at {} ms came out at {} ms", x.0, y.0);
                }
            }
            _ => {
                let (a, b) = (md5s(src, &map, true), md5s(out, &map, true));
                assert!(!a.is_empty());
                assert_eq!(
                    b.iter().map(|r| &r.1).collect::<Vec<_>>(),
                    a.iter().map(|r| &r.1).collect::<Vec<_>>()
                );
            }
        }
    }
    for (a, b) in durations(src).iter().zip(durations(out)) {
        assert!((a - b).abs() < 0.1, "duration {b} s against {a} s");
    }
}

fn mkv_case(name: &str, codecs: &[&str], kinds: &[&str]) {
    if skip() {
        return;
    }
    let src = source(&format!("{name}.mkv"), codecs);
    for mode in [Mode::Progressive, Mode::Fragmented] {
        let out = dir().join(format!("{name}_{mode:?}.mp4"));
        mkv_to_mp4(&src, &out, mode);
        same_media(&src, &out, kinds);
    }
    let out = dir().join(format!("{name}_again.mkv"));
    mkv_to_mkv(&src, &out);
    same_media(&src, &out, kinds);
}

#[test]
fn h264_with_b_frames_and_aac_from_matroska() {
    let codecs = ["-c:v", "libx264", "-preset", "ultrafast", "-bf", "2", "-g", "25", "-c:a", "aac"];
    mkv_case(
        "h264_aac",
        &["-tune", "film"].iter().chain(&codecs).copied().collect::<Vec<_>>(),
        &["video", "audio"],
    );
    let src = dir().join("h264_aac.mkv");
    if src.exists() {
        let out = dir().join("h264_aac_Progressive.mp4");
        let r = Mp4Reader::new(&mp4_head(&std::fs::read(&out).unwrap())).unwrap();
        assert_eq!(r.tracks()[1].media_time, 1024, "the AAC priming is the audio edit");
    }
}

#[test]
fn h265_from_matroska() {
    let codecs = ["-c:v", "libx265", "-x265-params", "log-level=error:bframes=3", "-c:a", "aac"];
    mkv_case("h265", &codecs, &["video", "audio"]);
}

#[test]
fn av1_and_opus_from_matroska() {
    let codecs =
        ["-c:v", "libsvtav1", "-preset", "12", "-svtav1-params", "verbosity=0", "-c:a", "libopus"];
    mkv_case("av1_opus", &codecs, &["video", "audio"]);
}

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn skip() -> bool {
    let absent = !have_ffmpeg();
    if absent {
        eprintln!("skipped: no ffmpeg");
    }
    absent
}

#[test]
fn committed_mp4_to_matroska() {
    if skip() {
        return;
    }
    let mp4 = fixture("tests/fixtures/tiny_av.mp4");
    let out = dir().join("tiny_av.mkv");
    mp4_to(&mp4, &out, true, Mode::Progressive);
    same_media(&mp4, &out, &["video", "audio"]);
    let bframes = fixture("tests/fixtures/bframes_h264.mp4");
    let out = dir().join("bframes.mkv");
    mp4_to(&bframes, &out, true, Mode::Progressive);
    same_media(&bframes, &out, &["video"]);
}

#[test]
fn committed_mp4_to_mp4() {
    if skip() {
        return;
    }
    let moov_last = fixture("tests/fixtures/moov_last.mp4");
    for mode in [Mode::Progressive, Mode::Fragmented] {
        let out = dir().join(format!("moov_last_{mode:?}.mp4"));
        mp4_to(&moov_last, &out, false, mode);
        same_media(&moov_last, &out, &["video", "audio"]);
    }
}

#[test]
fn committed_matroska_to_mp4() {
    if skip() {
        return;
    }
    let mkv = fixture("tests/fixtures/cues_last.mkv");
    for mode in [Mode::Progressive, Mode::Fragmented] {
        let out = dir().join(format!("cues_last_{mode:?}.mp4"));
        mkv_to_mp4(&mkv, &out, mode);
        same_media(&mkv, &out, &["video", "audio"]);
    }
}

fn srt_of(path: &Path) -> String {
    let out = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:s", "-f", "srt", "-"])
        .output()
        .expect("run ffmpeg");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn sparse_subtitles_from_matroska() {
    if skip() {
        return;
    }
    let srt = dir().join("cues.srt");
    std::fs::write(
        &srt,
        "1\n00:00:00,300 --> 00:00:00,800\nfirst\n\n2\n00:00:01,500 --> 00:00:01,900\nsecond\n\n",
    )
    .unwrap();
    let mut args = vec!["-f", "lavfi", "-i", "testsrc2=size=160x120:rate=25:duration=2"];
    args.extend(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=2"]);
    let srt_arg = srt.to_str().unwrap().to_string();
    args.extend(["-i", &srt_arg, "-map", "0", "-map", "1", "-map", "2"]);
    args.extend(["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-c:s", "srt"]);
    let src = ffmpeg("subs.mkv", &args);
    for mode in [Mode::Progressive, Mode::Fragmented] {
        let out = dir().join(format!("subs_{mode:?}.mp4"));
        mkv_to_mp4(&src, &out, mode);
        let (streams, errors) = probe(&out);
        assert!(errors.trim().is_empty(), "{mode:?} ffprobe errors: {errors}");
        assert_eq!(streams.len(), 3, "{mode:?} {streams:?}");
        assert!(streams[2].starts_with("mov_text,subtitle"), "{mode:?} {streams:?}");
        assert_eq!(srt_of(&out), srt_of(&src), "{mode:?} cues");
        same_media(&src, &out, &["video", "audio"]);
    }
}
