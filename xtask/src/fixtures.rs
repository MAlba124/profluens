//! `fixtures`, the pfplay corpus in fixtures/out. Existing outputs stay unless FORCE is set,
//! a missing encoder skips and only a failed encode exits nonzero.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::Error;
use crate::util;

const VSRC: &str = "testsrc2=size=320x180:rate=25:duration=5";
const ASRC: &str = "sine=frequency=440:sample_rate=48000:duration=5";

const AV: &[&str] = &["-f", "lavfi", "-i", VSRC, "-f", "lavfi", "-i", ASRC];
const V: &[&str] = &["-f", "lavfi", "-i", VSRC];
const A: &[&str] = &["-f", "lavfi", "-i", ASRC];
const A44: &[&str] = &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=5"];
/// Speex codes 8, 16 and 32 kHz, the wideband mode here.
const A16: &[&str] = &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000:duration=2"];
const VA_TINY: &[&str] = &[
    "-f",
    "lavfi",
    "-i",
    "testsrc2=size=160x120:rate=15:duration=2",
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440:sample_rate=16000:duration=2",
];

/// Three chapters, the last in German, as ffmpeg's ffmetadata states them. No start's seconds
/// round up, ffmpeg's Vorbis comment writer rounds the seconds and keeps the milliseconds.
const CHAPTERS: &str = ";FFMETADATA1
title=Chaptered

[CHAPTER]
TIMEBASE=1/1000
START=0
END=1250
title=Intro

[CHAPTER]
TIMEBASE=1/1000
START=1250
END=3250
title=Mittelstück

[CHAPTER]
TIMEBASE=1/1000
START=3250
END=5000
title=Ende
language=ger
";

const SVTAV1: &[&str] = &["-c:v", "libsvtav1", "-preset", "12", "-svtav1-params", "fast-decode=1"];
const AOMAV1: &[&str] = &["-c:v", "libaom-av1", "-cpu-used", "8", "-usage", "realtime"];

#[derive(Clone, Copy)]
enum Build {
    Plain {
        input: &'static [&'static str],
        args: &'static [&'static str],
    },
    /// [`CHAPTERS`] mapped in as a second input.
    Chapters {
        input: &'static [&'static str],
        args: &'static [&'static str],
    },
    Av1,
    Vp8Resize,
}

struct Fixture {
    name: &'static str,
    needs: &'static [&'static str],
    build: Build,
}

const fn plain(
    name: &'static str,
    needs: &'static [&'static str],
    input: &'static [&'static str],
    args: &'static [&'static str],
) -> Fixture {
    Fixture { name, needs, build: Build::Plain { input, args } }
}

const FIXTURES: &[Fixture] = &[
    // XviD style, packed B-frames, the legacy idx1 at the end.
    plain(
        "avi_mpeg4_ac3.avi",
        &["mpeg4", "ac3"],
        AV,
        &[
            "-c:v",
            "mpeg4",
            "-vtag",
            "XVID",
            "-bf",
            "2",
            "-g",
            "25",
            "-b:v",
            "400k",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "ac3",
            "-ac",
            "2",
            "-b:a",
            "96k",
            "-shortest",
            "-f",
            "avi",
        ],
    ),
    plain(
        "mkv_h264_aac.mkv",
        &["libx264", "aac"],
        AV,
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-shortest",
            "-f",
            "matroska",
        ],
    ),
    plain(
        "mkv_h265_aac.mkv",
        &["libx265", "aac"],
        AV,
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-tag:v",
            "hvc1",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-shortest",
            "-f",
            "matroska",
        ],
    ),
    plain(
        "webm_vp8_vorbis.webm",
        &["libvpx", "libvorbis"],
        AV,
        &[
            "-c:v",
            "libvpx",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-b:v",
            "400k",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "libvorbis",
            "-b:a",
            "96k",
            "-shortest",
            "-f",
            "webm",
        ],
    ),
    // Turns portrait halfway, the keyframes carry the new size.
    Fixture {
        name: "webm_vp8_resize.webm",
        needs: &["libvpx", "libvorbis"],
        build: Build::Vp8Resize,
    },
    plain(
        "webm_vp9.webm",
        &["libvpx-vp9"],
        V,
        &[
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-b:v",
            "400k",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "webm",
        ],
    ),
    Fixture { name: "mkv_av1.mkv", needs: &[], build: Build::Av1 },
    plain(
        "mp4_h264_aac.mp4",
        &["libx264", "aac"],
        AV,
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-shortest",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
        ],
    ),
    plain(
        "mp4_h265.mp4",
        &["libx265"],
        V,
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-tag:v",
            "hvc1",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
        ],
    ),
    // moov after mdat, Mp4Demux must reject it cleanly.
    plain(
        "mp4_h264_nofaststart.mp4",
        &["libx264", "aac"],
        AV,
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-shortest",
            "-movflags",
            "-faststart",
            "-f",
            "mp4",
        ],
    ),
    plain("ogg_flac.oga", &["flac"], A, &["-c:a", "flac", "-f", "oga"]),
    plain("ogg_vorbis.ogg", &["libvorbis"], A, &["-c:a", "libvorbis", "-b:a", "96k", "-f", "ogg"]),
    plain("ogg_speex.spx", &["libspeex"], A16, &["-c:a", "libspeex", "-f", "ogg"]),
    plain(
        "ogg_theora_speex.ogv",
        &["libtheora", "libspeex"],
        VA_TINY,
        &["-c:v", "libtheora", "-q:v", "5", "-c:a", "libspeex", "-f", "ogg"],
    ),
    plain(
        "audio.mp3",
        &["libmp3lame"],
        A44,
        &[
            "-af",
            "aeval=val(0)|val(0):c=stereo",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "128k",
            "-ar",
            "44100",
            "-ac",
            "2",
        ],
    ),
    plain("audio.flac", &["flac"], A, &["-c:a", "flac"]),
    plain("audio.wav", &["pcm_s16le"], A, &["-c:a", "pcm_s16le", "-f", "wav"]),
    plain("audio.aac", &["aac"], A, &["-c:a", "aac", "-b:a", "128k", "-f", "adts"]),
    plain("audio_f32.wav", &["pcm_f32le"], A, &["-c:a", "pcm_f32le", "-f", "wav"]),
    plain("audio.ac3", &["ac3"], A, &["-c:a", "ac3", "-b:a", "192k", "-f", "ac3"]),
    plain("audio_51.ac3", &["ac3"], A, &["-c:a", "ac3", "-ac", "6", "-b:a", "448k", "-f", "ac3"]),
    plain("audio.eac3", &["eac3"], A, &["-c:a", "eac3", "-b:a", "192k", "-f", "eac3"]),
    // ffmpeg's DTS encoder is marked experimental
    plain(
        "audio.dts",
        &["dca"],
        A,
        &["-c:a", "dca", "-strict", "-2", "-ac", "2", "-b:a", "768k", "-f", "dts"],
    ),
    plain(
        "ts_h264_aac.ts",
        &["libx264", "aac"],
        AV,
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-shortest",
            "-f",
            "mpegts",
        ],
    ),
    plain(
        "ts_h265_ac3.ts",
        &["libx265", "ac3"],
        AV,
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "ac3",
            "-b:a",
            "192k",
            "-shortest",
            "-f",
            "mpegts",
        ],
    ),
    // MPEG-1 system stream, as a VCD carries it
    plain(
        "ps_mpeg1_mp2.mpg",
        &["mpeg1video", "mp2"],
        AV,
        &[
            "-c:v",
            "mpeg1video",
            "-b:v",
            "800k",
            "-c:a",
            "mp2",
            "-b:a",
            "192k",
            "-shortest",
            "-f",
            "mpeg",
        ],
    ),
    // DVD packs, AC-3 in private stream 1
    plain(
        "ps_mpeg2_ac3.vob",
        &["mpeg2video", "ac3"],
        AV,
        &[
            "-c:v",
            "mpeg2video",
            "-b:v",
            "1500k",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "ac3",
            "-b:a",
            "192k",
            "-shortest",
            "-f",
            "vob",
        ],
    ),
    chapters("chapters.mkv", &["aac"], &["-c:a", "aac", "-b:a", "32k", "-f", "matroska"]),
    // a chpl and a chapter track both
    chapters(
        "chapters.mp4",
        &["aac"],
        &["-c:a", "aac", "-b:a", "32k", "-movflags", "+faststart", "-f", "mp4"],
    ),
    // the chapter track alone, behind the mdat
    chapters(
        "chapters.mov",
        &["aac"],
        &["-c:a", "aac", "-b:a", "32k", "-movflags", "+disable_chpl", "-f", "mov"],
    ),
    chapters("chapters.opus", &["libopus"], &["-c:a", "libopus", "-b:a", "24k", "-f", "ogg"]),
    // the same chapters as comments, ffmpeg's FLAC muxer writes none of its own
    plain(
        "chapters.flac",
        &["flac"],
        A,
        &[
            "-metadata",
            "CHAPTER000=00:00:00.000",
            "-metadata",
            "CHAPTER000NAME=Intro",
            "-metadata",
            "CHAPTER001=00:00:01.250",
            "-metadata",
            "CHAPTER001NAME=Mittelstück",
            "-metadata",
            "CHAPTER002=00:00:03.250",
            "-metadata",
            "CHAPTER002NAME=Ende",
            "-c:a",
            "flac",
            "-f",
            "flac",
        ],
    ),
];

const fn chapters(
    name: &'static str,
    needs: &'static [&'static str],
    args: &'static [&'static str],
) -> Fixture {
    Fixture { name, needs, build: Build::Chapters { input: A, args } }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Exists,
    Ok,
    Skip,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Exists => "EXISTS",
            Status::Ok => "OK",
            Status::Skip => "SKIP",
            Status::Fail => "FAIL",
        }
    }
}

struct Tools {
    ffmpeg: OsString,
    ffprobe: OsString,
    encoders: String,
    av1: Option<(&'static str, &'static [&'static str])>,
}

fn find_tools() -> Result<Tools, Error> {
    let wanted = util::env_nonempty("FFMPEG").filter(|w| util::which(w).is_some());
    let ffmpeg = if let Some(wanted) = wanted {
        wanted
    } else {
        util::which(OsStr::new("ffmpeg"))
            .ok_or_else(|| Error::Missing("ffmpeg not found (set FFMPEG=/path/to/ffmpeg)".into()))?
            .into_os_string()
    };
    let ffprobe = util::env_nonempty("FFPROBE").unwrap_or_else(|| sibling_ffprobe(&ffmpeg));
    let ffprobe = if util::which(&ffprobe).is_some() {
        ffprobe
    } else {
        util::which(OsStr::new("ffprobe")).map_or_else(|| "ffprobe".into(), PathBuf::into_os_string)
    };
    let out = Command::new(&ffmpeg)
        .env_remove("LD_LIBRARY_PATH")
        .args(["-hide_banner", "-encoders"])
        .stderr(Stdio::null())
        .output();
    let encoders = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let av1 = if has_encoder(&encoders, "libsvtav1") {
        Some(("libsvtav1", SVTAV1))
    } else if has_encoder(&encoders, "libaom-av1") {
        Some(("libaom-av1", AOMAV1))
    } else {
        None
    };
    Ok(Tools { ffmpeg, ffprobe, encoders, av1 })
}

fn sibling_ffprobe(ffmpeg: &OsStr) -> OsString {
    let bytes = ffmpeg.as_bytes();
    let mut out = bytes.strip_suffix(b"ffmpeg").unwrap_or(bytes).to_vec();
    out.extend_from_slice(b"ffprobe");
    OsString::from_vec(out)
}

fn has_encoder(encoders: &str, name: &str) -> bool {
    encoders.lines().any(|line| {
        let line = line.trim_start();
        let flags_end =
            line.find(|c: char| !(c.is_ascii_uppercase() || c == '.')).unwrap_or(line.len());
        if flags_end == 0 {
            return false;
        }
        let rest = &line[flags_end..];
        let trimmed = rest.trim_start();
        if trimmed.len() == rest.len() {
            return false;
        }
        trimmed.strip_prefix(name).is_some_and(|after| {
            !after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        })
    })
}

fn ffmpeg(tools: &Tools) -> Command {
    let mut cmd = Command::new(&tools.ffmpeg);
    cmd.env_remove("LD_LIBRARY_PATH").args(["-hide_banner", "-loglevel", "error", "-y"]);
    cmd
}

/// `Ok(false)` when a needed encoder is missing.
fn build(tools: &Tools, fixture: &Fixture, out: &Path) -> Result<bool, Error> {
    if !fixture.needs.iter().all(|e| has_encoder(&tools.encoders, e)) {
        return Ok(false);
    }
    match fixture.build {
        Build::Plain { input, args } => {
            util::run("ffmpeg", ffmpeg(tools).args(input).args(args).arg(out))?;
        }
        Build::Chapters { input, args } => {
            let tmp = util::temp_dir("chapters")?;
            let meta = tmp.path.join("chapters.txt");
            util::write(&meta, CHAPTERS.as_bytes())?;
            let mut cmd = ffmpeg(tools);
            cmd.args(input).arg("-i").arg(&meta);
            cmd.args(["-map", "0", "-map_metadata", "1", "-map_chapters", "1"]);
            util::run("ffmpeg", cmd.args(args).arg(out))?;
        }
        Build::Av1 => {
            let Some((_, av1)) = tools.av1 else {
                return Ok(false);
            };
            let mut cmd = ffmpeg(tools);
            cmd.args(V).args(av1).args(["-pix_fmt", "yuv420p", "-f", "matroska"]).arg(out);
            util::run("ffmpeg", &mut cmd)?;
        }
        Build::Vp8Resize => vp8_resize(tools, out)?,
    }
    Ok(true)
}

fn vp8_resize(tools: &Tools, out: &Path) -> Result<(), Error> {
    let tmp = util::temp_dir("vp8-resize")?;
    let parts = [("320x180", "440", "a.webm"), ("180x320", "660", "b.webm")];
    for (size, freq, name) in parts {
        let mut cmd = ffmpeg(tools);
        cmd.args(["-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={size}:rate=25:duration=2"))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("sine=frequency={freq}:sample_rate=48000:duration=2"))
            .args(["-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8", "-b:v", "120k"])
            .args(["-pix_fmt", "yuv420p", "-c:a", "libvorbis", "-b:a", "64k", "-shortest"])
            .args(["-f", "webm"])
            .arg(tmp.path.join(name));
        util::run("ffmpeg", &mut cmd)?;
    }
    let mut list = Vec::new();
    for (_, _, name) in parts {
        list.extend_from_slice(b"file '");
        list.extend_from_slice(tmp.path.join(name).as_os_str().as_bytes());
        list.extend_from_slice(b"'\n");
    }
    let list_path = tmp.path.join("list.txt");
    util::write(&list_path, &list)?;
    let mut cmd = ffmpeg(tools);
    cmd.args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list_path)
        .args(["-c", "copy", "-f", "webm"]);
    util::run("ffmpeg", cmd.arg(out))
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| m.len())
}

fn emit(tools: &Tools, fixture: &Fixture, dir: &Path, force: bool) -> Status {
    let path = dir.join(fixture.name);
    let name = fixture.name;
    if !force && file_size(&path) > 0 {
        println!("  exists: {name}");
        return Status::Exists;
    }
    let _ = fs::remove_file(&path);
    let status = match build(tools, fixture, &path) {
        Ok(true) if file_size(&path) > 0 => {
            println!("  wrote:  {name}");
            return Status::Ok;
        }
        Ok(true) => {
            println!("  FAIL:   {name} (empty output)");
            Status::Fail
        }
        Ok(false) => {
            println!("  skip:   {name} (encoder missing)");
            Status::Skip
        }
        Err(e) => {
            println!("  FAIL:   {name} ({e})");
            Status::Fail
        }
    };
    let _ = fs::remove_file(&path);
    status
}

fn stream_summary(csv: &str) -> String {
    let parts: Vec<String> = csv
        .lines()
        .filter_map(|line| line.split_once(','))
        .filter(|(_, kind)| !kind.is_empty())
        .map(|(name, kind)| format!("{}:{name}", kind.replace(',', "|")))
        .collect();
    if parts.is_empty() { "UNREADABLE".to_string() } else { parts.join(" ") }
}

fn probe_streams(tools: &Tools, path: &Path) -> String {
    let out = Command::new(&tools.ffprobe)
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-show_entries", "stream=codec_name,codec_type", "-of", "csv=p=0"])
        .arg(path)
        .stderr(Stdio::null())
        .output();
    stream_summary(
        &out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default(),
    )
}

fn du(path: &Path) -> String {
    let out = Command::new("du").arg("-h").arg(path).stderr(Stdio::null()).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    text.split('\t').next().unwrap_or("").trim().to_string()
}

fn summary(tools: &Tools, dir: &Path, log: &[(&str, Status)]) -> usize {
    println!();
    println!("== ffprobe sanity check ==");
    println!("{:<26} {:<10} {:<8}  STREAMS (ffprobe)", "FILE", "STATUS", "SIZE");
    println!("{:<26} {:<10} {:<8}  -----------------", "----", "------", "----");
    for &(name, status) in log {
        let path = dir.join(name);
        if file_size(&path) > 0 {
            let streams = probe_streams(tools, &path);
            println!("{name:<26} {:<10} {:<8}  {streams}", status.label(), du(&path));
        } else {
            println!("{name:<26} {:<10} {:<8}  (not produced)", status.label(), "-");
        }
    }
    let count = |want: &[Status]| log.iter().filter(|(_, s)| want.contains(s)).count();
    let failed = count(&[Status::Fail]);
    println!();
    println!(
        "done: {} present, {} skipped (encoder missing), {failed} failed",
        count(&[Status::Ok, Status::Exists]),
        count(&[Status::Skip])
    );
    failed
}

pub fn corpus(root: &Path) -> Result<(), Error> {
    let dir = root.join("fixtures/out");
    util::mkdir_p(&dir)?;
    let tools = find_tools()?;
    let force = util::env_nonempty("FORCE").is_some();
    println!("== generating fixtures into {} ==", dir.display());
    println!("   ffmpeg: {}", tools.ffmpeg.to_string_lossy());
    println!("   av1 encoder: {}", tools.av1.map_or("<none available>", |(name, _)| name));
    println!();
    let log: Vec<(&str, Status)> =
        FIXTURES.iter().map(|f| (f.name, emit(&tools, f, &dir, force))).collect();
    match summary(&tools, &dir, &log) {
        0 => Ok(()),
        n => Err(Error::Failed(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCODERS: &str = "Encoders:
 V..... = Video
 ------
 V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC (codec h264)
 V....D libvpx-vp9           libvpx VP9 (codec vp9)
 A....D aac                  AAC (Advanced Audio Coding)
 A....D libmp3lame_x         made up
";

    #[test]
    fn encoder_lines_match_on_a_word_boundary() {
        assert!(has_encoder(ENCODERS, "libx264"));
        assert!(has_encoder(ENCODERS, "aac"));
        assert!(!has_encoder(ENCODERS, "libx26"));
        assert!(!has_encoder(ENCODERS, "libmp3lame"));
        assert!(!has_encoder(ENCODERS, "Video"));
        // `\b` sits between x and the hyphen, so libvpx matches libvpx-vp9 as grep does.
        assert!(has_encoder(ENCODERS, "libvpx"));
        assert!(has_encoder(ENCODERS, "libvpx-vp9"));
    }

    #[test]
    fn ffprobe_sits_beside_ffmpeg() {
        assert_eq!(sibling_ffprobe(OsStr::new("/usr/bin/ffmpeg")), "/usr/bin/ffprobe");
        assert_eq!(sibling_ffprobe(OsStr::new("ffmpeg")), "ffprobe");
        assert_eq!(sibling_ffprobe(OsStr::new("/opt/ff")), "/opt/ffffprobe");
    }

    #[test]
    fn streams_are_summarised_type_first() {
        assert_eq!(stream_summary("h264,video\naac,audio\n"), "video:h264 audio:aac");
        assert_eq!(stream_summary(""), "UNREADABLE");
        assert_eq!(stream_summary("h264,\n"), "UNREADABLE");
    }

    #[test]
    fn every_fixture_name_is_unique() {
        let mut names: Vec<&str> = FIXTURES.iter().map(|f| f.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), FIXTURES.len());
        assert_eq!(FIXTURES.len(), 32);
    }
}
