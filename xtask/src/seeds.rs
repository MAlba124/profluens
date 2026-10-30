//! `seeds <out>`, demuxer fuzz seeds, committed fixtures and ffmpeg clips behind control blocks.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::Error;
use crate::util;

mod codec;
mod made;

const FORMATS: [&str; 7] = ["mkv", "mp4", "ts", "avi", "ogg", "flv", "ps"];

/// See Plan::decode. b0 pull, variant, pools. b1 chunk, delay. b2 pads. b3 seek kinds.
/// b4..b11 the two seeks. b12..b15 the seed.
type Control = [u8; 16];

const CONTROLS: [Control; 7] = [
    [
        0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00,
    ],
    [
        0x11, 0x1c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
        0x00,
    ],
    [
        0x08, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x00,
    ],
    [
        0x01, 0x0c, 0x00, 0x01, 0x05, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00,
        0x00,
    ],
    [
        0x10, 0x05, 0x00, 0x0e, 0x14, 0x00, 0x00, 0x40, 0x40, 0x00, 0x00, 0xc0, 0x05, 0x00, 0x00,
        0x00,
    ],
    [
        0x19, 0x14, 0x00, 0x03, 0x0a, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00,
        0x00,
    ],
    [
        0x01, 0x38, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00,
        0x00,
    ],
];

const MP4_CONTROLS: [Control; 2] = [
    [
        0x04, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00,
        0x00,
    ],
    [
        0x07, 0x0c, 0x00, 0x01, 0x08, 0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00,
        0x00,
    ],
];

const VARIANT_CONTROLS: [Control; 3] = [
    [
        0x02, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00,
        0x00,
    ],
    [
        0x05, 0x0c, 0x00, 0x01, 0x08, 0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00,
        0x00,
    ],
    [
        0x0e, 0x05, 0x00, 0x03, 0x04, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x00,
        0x00,
    ],
];

const MAX_RAW: u64 = 1024 * 1024 - 16;

fn extra_controls(format: &str) -> &'static [Control] {
    match format {
        "mkv" => &[],
        "mp4" => &MP4_CONTROLS,
        _ => &VARIANT_CONTROLS,
    }
}

fn frame(control: &Control, body: &[u8], out: &mut impl Write) -> std::io::Result<()> {
    out.write_all(control)?;
    out.write_all(body)
}

fn put(dir: &Path, format: &str, raw: &Path) -> Result<(), Error> {
    let body = util::read(raw)?;
    let name = util::file_name(raw).to_string_lossy();
    let controls = CONTROLS.iter().chain(extra_controls(format));
    for (n, control) in controls.enumerate() {
        let path = dir.join(format!("{name}.{}", n + 1));
        let mut file = File::create(&path).map_err(|e| util::io_err(&path, e))?;
        frame(control, &body, &mut file).map_err(|e| util::io_err(&path, e))?;
    }
    Ok(())
}

fn copy_all(dir: &Path, suffix: &str, to: &Path) -> Result<(), Error> {
    for f in util::glob(dir, suffix) {
        util::copy(&f, &to.join(util::file_name(&f)))?;
    }
    Ok(())
}

fn committed(root: &Path, raw: &Path) -> Result<(), Error> {
    let formats = root.join("plugins/formats");
    let mkv = raw.join("mkv");
    copy_all(&formats.join("mkv/tests/fixtures"), ".mkv", &mkv)?;
    copy_all(&formats.join("mp4/tests/fixtures"), ".mkv", &mkv)?;
    copy_all(&root.join("plugins/audio/opus/tests/fixtures"), ".mka", &mkv)?;
    let mp4_fixtures = formats.join("mp4/tests/fixtures");
    let frag = mp4_fixtures.join("frag");
    for dir in [&mp4_fixtures, &frag] {
        let prefix = util::file_name(dir).to_string_lossy();
        for f in util::glob(dir, ".mp4") {
            let to = format!("{prefix}_{}", util::file_name(&f).to_string_lossy());
            util::copy(&f, &raw.join("mp4").join(to))?;
        }
    }
    // A DASH init with its first two segments.
    for s in 0..3 {
        let init = frag.join(format!("dash_init-stream{s}.m4s"));
        if !init.is_file() {
            continue;
        }
        let mut joined = util::read(&init)?;
        for seg in 1..3 {
            let path = frag.join(format!("dash_stream{s}-0000{seg}.m4s"));
            if path.is_file() {
                joined.extend_from_slice(&util::read(&path)?);
            }
        }
        util::write(&raw.join(format!("mp4/frag_dash{s}.mp4")), &joined)?;
    }
    copy_all(&formats.join("ts/tests/fixtures"), ".ts", &raw.join("ts"))?;
    copy_all(&formats.join("tsdemux/tests/fixtures"), ".ts", &raw.join("ts"))?;
    copy_all(&formats.join("avi/tests/fixtures"), ".avi", &raw.join("avi"))?;
    copy_all(&formats.join("flv/tests/fixtures"), ".flv", &raw.join("flv"))?;
    copy_all(&formats.join("ps/tests/fixtures"), ".vob", &raw.join("ps"))?;
    copy_all(&formats.join("ps/tests/fixtures"), ".mpg", &raw.join("ps"))?;
    copy_all(&formats.join("ogg/tests/fixtures"), "", &raw.join("ogg"))
}

const V: &str = "testsrc2=size=160x90:rate=25:duration=1";
const A: &str = "sine=frequency=440:sample_rate=48000:duration=1";

/// `@srt` and `@chap` in args name the subtitle and chapter files.
struct Clip {
    video: bool,
    dir: &'static str,
    name: &'static str,
    args: &'static [&'static str],
}

const fn av(dir: &'static str, name: &'static str, args: &'static [&'static str]) -> Clip {
    Clip { video: true, dir, name, args }
}

const fn au(dir: &'static str, name: &'static str, args: &'static [&'static str]) -> Clip {
    Clip { video: false, dir, name, args }
}

const CLIPS: &[Clip] = &[
    av(
        "mkv",
        "h264_aac_cues.mkv",
        &["-c:v", "libx264", "-preset", "ultrafast", "-g", "5", "-c:a", "aac", "-b:a", "32k"],
    ),
    av(
        "mkv",
        "h264_bframes_flac.mkv",
        &["-c:v", "libx264", "-preset", "ultrafast", "-bf", "2", "-g", "10", "-c:a", "flac"],
    ),
    av(
        "mkv",
        "hevc_opus.mkv",
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=none",
            "-c:a",
            "libopus",
            "-b:a",
            "24k",
        ],
    ),
    av(
        "mkv",
        "vp9_opus.webm",
        &[
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-b:v",
            "100k",
            "-c:a",
            "libopus",
            "-b:a",
            "24k",
        ],
    ),
    av(
        "mkv",
        "vp8_vorbis.webm",
        &[
            "-c:v",
            "libvpx",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-b:v",
            "100k",
            "-c:a",
            "libvorbis",
        ],
    ),
    av(
        "mkv",
        "vp8_live.webm",
        &[
            "-c:v",
            "libvpx",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-b:v",
            "100k",
            "-c:a",
            "libopus",
            "-live",
            "1",
            "-f",
            "webm",
        ],
    ),
    av(
        "mkv",
        "av1_opus.mkv",
        &["-c:v", "libsvtav1", "-preset", "12", "-c:a", "libopus", "-b:a", "24k"],
    ),
    av("mkv", "mpeg4_mp3.mkv", &["-c:v", "mpeg4", "-q:v", "10", "-c:a", "mp3"]),
    av(
        "mkv",
        "h264_srt.mkv",
        &[
            "-i",
            "@srt",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-c:s",
            "srt",
        ],
    ),
    av(
        "mkv",
        "h264_ass.mkv",
        &[
            "-i",
            "@srt",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-c:s",
            "ass",
        ],
    ),
    av(
        "mkv",
        "chapters_attach.mkv",
        &[
            "-i",
            "@chap",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map_metadata",
            "2",
            "-map_chapters",
            "2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-attach",
            "@srt",
            "-metadata:s:2",
            "mimetype=text/plain",
        ],
    ),
    av(
        "mkv",
        "small_clusters.mkv",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-cluster_time_limit",
            "100",
            "-cluster_size_limit",
            "4096",
        ],
    ),
    av(
        "mkv",
        "index_front.mkv",
        &["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-reserve_index_space", "4096"],
    ),
    av("mkv", "h264_pcm.mkv", &["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "pcm_s16le"]),
    au("mkv", "opus.mka", &["-c:a", "libopus", "-b:a", "24k"]),
    au("mkv", "flac.mka", &["-c:a", "flac"]),
    au("mkv", "vorbis.mka", &["-c:a", "libvorbis"]),
    au("mkv", "ac3.mka", &["-c:a", "ac3"]),
    au("mkv", "aac_live.mka", &["-ac", "2", "-c:a", "aac", "-f", "matroska", "-live", "1"]),
    av(
        "mp4",
        "h264_aac_fast.mp4",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "5",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
            "-movflags",
            "+faststart",
        ],
    ),
    av(
        "mp4",
        "h264_bframes_last.mp4",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-bf",
            "2",
            "-g",
            "10",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
        ],
    ),
    av(
        "mp4",
        "hevc_hvc1.mp4",
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=none",
            "-tag:v",
            "hvc1",
            "-c:a",
            "aac",
        ],
    ),
    av(
        "mp4",
        "hevc_hev1.mp4",
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=none",
            "-tag:v",
            "hev1",
            "-an",
        ],
    ),
    av(
        "mp4",
        "av1_opus.mp4",
        &["-c:v", "libsvtav1", "-preset", "12", "-c:a", "libopus", "-b:a", "24k"],
    ),
    av(
        "mp4",
        "vp9_flac.mp4",
        &[
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-b:v",
            "100k",
            "-c:a",
            "flac",
            "-strict",
            "-2",
        ],
    ),
    av(
        "mp4",
        "frag_empty_moov.mp4",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-movflags",
            "frag_keyframe+empty_moov",
        ],
    ),
    av(
        "mp4",
        "frag_mfra.mp4",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "5",
            "-c:a",
            "aac",
            "-movflags",
            "frag_keyframe",
        ],
    ),
    av(
        "mp4",
        "frag_sidx.mp4",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "5",
            "-c:a",
            "aac",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof+global_sidx",
        ],
    ),
    av(
        "mp4",
        "cmaf.mp4",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "5",
            "-c:a",
            "aac",
            "-movflags",
            "+cmaf+dash+delay_moov",
        ],
    ),
    av(
        "mp4",
        "h264_tx3g.mp4",
        &[
            "-i",
            "@srt",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-c:s",
            "mov_text",
        ],
    ),
    av(
        "mp4",
        "chapters.mp4",
        &[
            "-i",
            "@chap",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map_metadata",
            "2",
            "-map_chapters",
            "2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
        ],
    ),
    av(
        "mp4",
        "editlist.mp4",
        &[
            "-ss",
            "0.3",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-use_editlist",
            "1",
        ],
    ),
    av(
        "mp4",
        "h264_aac.mov",
        &["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-f", "mov"],
    ),
    au("mp4", "aac.m4a", &["-c:a", "aac", "-b:a", "32k"]),
    au("mp4", "alac.m4a", &["-c:a", "alac"]),
    au("mp4", "opus.mp4", &["-c:a", "libopus", "-b:a", "24k"]),
    au("mp4", "mp3.mp4", &["-c:a", "mp3"]),
    au("mp4", "pcm.mov", &["-c:a", "pcm_s16le", "-f", "mov"]),
    av(
        "ts",
        "h264_aac.ts",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "5",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
            "-f",
            "mpegts",
        ],
    ),
    av(
        "ts",
        "h264_bframes_ac3.ts",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-bf",
            "2",
            "-g",
            "10",
            "-c:a",
            "ac3",
            "-f",
            "mpegts",
        ],
    ),
    av(
        "ts",
        "hevc_eac3.ts",
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=none",
            "-c:a",
            "eac3",
            "-f",
            "mpegts",
        ],
    ),
    av(
        "ts",
        "mpeg2_mp2.ts",
        &["-c:v", "mpeg2video", "-b:v", "200k", "-c:a", "mp2", "-f", "mpegts"],
    ),
    av("ts", "mpeg4_mp3.ts", &["-c:v", "mpeg4", "-q:v", "10", "-c:a", "mp3", "-f", "mpegts"]),
    av(
        "ts",
        "h264_opus.ts",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "libopus",
            "-b:a",
            "24k",
            "-f",
            "mpegts",
        ],
    ),
    av(
        "ts",
        "h264_latm.ts",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-mpegts_flags",
            "latm",
            "-f",
            "mpegts",
        ],
    ),
    av(
        "ts",
        "h264_aac.m2ts",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-mpegts_m2ts_mode",
            "1",
            "-f",
            "mpegts",
        ],
    ),
    av(
        "ts",
        "resend.ts",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-mpegts_flags",
            "resend_headers+pat_pmt_at_frames",
            "-pcr_period",
            "20",
            "-f",
            "mpegts",
        ],
    ),
    av(
        "ts",
        "system_b.ts",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-pes_payload_size",
            "0",
            "-mpegts_flags",
            "system_b",
            "-f",
            "mpegts",
        ],
    ),
    av(
        "ts",
        "wrap.ts",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-muxdelay",
            "0",
            "-output_ts_offset",
            "95443",
            "-f",
            "mpegts",
        ],
    ),
    av(
        "ts",
        "two_programs.ts",
        &[
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "1:a",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-program",
            "title=one:st=0:st=1",
            "-program",
            "title=two:st=2",
            "-f",
            "mpegts",
        ],
    ),
    au("ts", "aac.ts", &["-c:a", "aac", "-b:a", "32k", "-f", "mpegts"]),
    au("ts", "ac3.ts", &["-c:a", "ac3", "-f", "mpegts"]),
    av("avi", "mpeg4_mp3.avi", &["-c:v", "mpeg4", "-q:v", "10", "-c:a", "mp3"]),
    av("avi", "mpeg4_bframes_ac3.avi", &["-c:v", "mpeg4", "-q:v", "10", "-bf", "2", "-c:a", "ac3"]),
    av(
        "avi",
        "h264_aac.avi",
        &["-c:v", "libx264", "-preset", "ultrafast", "-g", "5", "-c:a", "aac"],
    ),
    av("avi", "mjpeg_pcm.avi", &["-c:v", "mjpeg", "-q:v", "20", "-c:a", "pcm_s16le"]),
    av("avi", "msmpeg4_mp2.avi", &["-c:v", "msmpeg4v2", "-q:v", "10", "-c:a", "mp2"]),
    av("avi", "raw.avi", &["-s", "32x18", "-c:v", "rawvideo", "-pix_fmt", "bgr24", "-an"]),
    au("avi", "pcm.avi", &["-c:a", "pcm_s16le"]),
    au("avi", "mp3.avi", &["-c:a", "mp3"]),
    av(
        "flv",
        "h264_aac_g5.flv",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "5",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
            "-flvflags",
            "add_keyframe_index",
            "-f",
            "flv",
        ],
    ),
    av(
        "flv",
        "h264_bframes_mp3.flv",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-bf",
            "2",
            "-g",
            "10",
            "-c:a",
            "libmp3lame",
            "-ar",
            "44100",
            "-f",
            "flv",
        ],
    ),
    av(
        "flv",
        "hevc_aac_fast.flv",
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=none:bframes=2",
            "-c:a",
            "aac",
            "-b:a",
            "24k",
            "-flvflags",
            "add_keyframe_index",
            "-f",
            "flv",
        ],
    ),
    av("flv", "av1_svt.flv", &["-c:v", "libsvtav1", "-preset", "12", "-an", "-f", "flv"]),
    av(
        "flv",
        "vp9_pcm_22k.flv",
        &[
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-c:a",
            "pcm_s16le",
            "-ar",
            "22050",
            "-f",
            "flv",
        ],
    ),
    av(
        "flv",
        "h264_no_metadata.flv",
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-an",
            "-flvflags",
            "no_metadata",
            "-f",
            "flv",
        ],
    ),
    au("flv", "aac_32k.flv", &["-c:a", "aac", "-b:a", "32k", "-f", "flv"]),
    au("ogg", "opus.opus", &["-c:a", "libopus", "-b:a", "24k"]),
    au(
        "ogg",
        "opus_small_pages.opus",
        &["-c:a", "libopus", "-b:a", "24k", "-page_duration", "20000"],
    ),
    au("ogg", "opus_51.opus", &["-ac", "6", "-c:a", "libopus", "-b:a", "64k"]),
    au("ogg", "vorbis.ogg", &["-c:a", "libvorbis"]),
    au("ogg", "flac.oga", &["-c:a", "flac", "-f", "ogg"]),
    av("ogg", "theora_vorbis.ogv", &["-c:v", "libtheora", "-q:v", "2", "-c:a", "libvorbis"]),
    av(
        "ogg",
        "theora_opus.ogv",
        &["-c:v", "libtheora", "-q:v", "2", "-c:a", "libopus", "-b:a", "24k"],
    ),
    au("ogg", "speex.spx", &["-c:a", "speex", "-f", "ogg"]),
    av("ps", "mpeg2_ac3.vob", &["-c:v", "mpeg2video", "-b:v", "200k", "-c:a", "ac3", "-f", "vob"]),
    av("ps", "mpeg2_mp2.vob", &["-c:v", "mpeg2video", "-b:v", "200k", "-c:a", "mp2", "-f", "vob"]),
    // The dvd muxer adds the navigation packs of private stream 2.
    av("ps", "nav_ac3.vob", &["-c:v", "mpeg2video", "-b:v", "200k", "-c:a", "ac3", "-f", "dvd"]),
    av("ps", "mpeg1_mp2.mpg", &["-c:v", "mpeg1video", "-b:v", "200k", "-c:a", "mp2", "-f", "mpeg"]),
    av(
        "ps",
        "h264_mp2.vob",
        &["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "mp2", "-f", "vob"],
    ),
    au("ps", "lpcm16.vob", &["-c:a", "pcm_dvd", "-f", "vob"]),
    au("ps", "lpcm24.vob", &["-c:a", "pcm_dvd", "-sample_fmt", "s32", "-f", "vob"]),
    au("ps", "dts.vob", &["-c:a", "dca", "-strict", "-2", "-f", "vob"]),
];

const SRT: &str =
    "1\n00:00:00,100 --> 00:00:00,600\nhello\n\n2\n00:00:00,600 --> 00:00:00,900\nworld\n";
const CHAPTERS: &str = ";FFMETADATA1\ntitle=seed\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=500\ntitle=one\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=500\nEND=1000\ntitle=two\n";

fn corpus(root: &Path, ffmpeg: &Path, raw: &Path) -> Result<(), Error> {
    let exe = std::env::current_exe().map_err(|e| util::io_err(Path::new("current_exe"), e))?;
    let _ = Command::new(exe)
        .arg("fixtures")
        .env("FFMPEG", ffmpeg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let out = root.join("fixtures/out");
    for (suffix, dir) in
        [(".mkv", "mkv"), (".webm", "mkv"), (".mp4", "mp4"), (".ogg", "ogg"), (".oga", "ogg")]
    {
        copy_all(&out, suffix, &raw.join(dir))?;
    }
    Ok(())
}

fn clips(raw: &Path) -> Result<(), Error> {
    let srt = raw.join("subs.srt");
    let chap = raw.join("chapters.txt");
    util::write(&srt, SRT.as_bytes())?;
    util::write(&chap, CHAPTERS.as_bytes())?;
    for clip in CLIPS {
        let out = raw.join(clip.dir).join(clip.name);
        let mut cmd = Command::new("ffmpeg");
        cmd.env_remove("LD_LIBRARY_PATH").args(["-v", "error", "-y"]);
        if clip.video {
            cmd.args(["-f", "lavfi", "-i", V]);
        }
        cmd.args(["-f", "lavfi", "-i", A]);
        for &arg in clip.args {
            match arg {
                "@srt" => cmd.arg(&srt),
                "@chap" => cmd.arg(&chap),
                _ => cmd.arg(arg),
            };
        }
        let ok = cmd.arg(&out).stdin(Stdio::null()).status().is_ok_and(|s| s.success());
        if !ok {
            eprintln!("seeds: ffmpeg failed on {}", out.display());
        }
    }
    // No idx1, its tail cut off.
    let avi = raw.join("avi/mpeg4_mp3.avi");
    if avi.is_file() {
        let bytes = util::read(&avi)?;
        let keep = bytes.len().saturating_sub(2048);
        util::write(&raw.join("avi/no_index.avi"), &bytes[..keep])?;
    }
    let (opus, vorbis) = (raw.join("ogg/opus.opus"), raw.join("ogg/vorbis.ogg"));
    if opus.is_file() && vorbis.is_file() {
        let mut chained = util::read(&opus)?;
        chained.extend_from_slice(&util::read(&vorbis)?);
        util::write(&raw.join("ogg/chained.ogg"), &chained)?;
    }
    // Two VOBs back to back, the clock restarting at the join.
    let vob = raw.join("ps/mpeg2_ac3.vob");
    if vob.is_file() {
        let one = util::read(&vob)?;
        util::write(&raw.join("ps/joined.vob"), &[one.as_slice(), &one].concat())?;
    }
    Ok(())
}

pub fn seeds(root: &Path, out: &Path) -> Result<(), Error> {
    let raw = out.join("raw");
    for t in FORMATS {
        util::mkdir_p(&raw.join(t))?;
        util::mkdir_p(&out.join(format!("{t}_demux")))?;
    }
    committed(root, &raw)?;
    made::mkv(root, &raw)?;
    if let Some(ffmpeg) = util::which("ffmpeg".as_ref()) {
        corpus(root, &ffmpeg, &raw)?;
        clips(&raw)?;
    }
    for t in FORMATS {
        let dir = out.join(format!("{t}_demux"));
        for f in util::glob(&raw.join(t), "") {
            if fs::metadata(&f).is_ok_and(|m| m.len() <= MAX_RAW) {
                put(&dir, t, &f)?;
            }
        }
        let files = util::list(&raw.join(t))?.len();
        let inputs = util::list(&dir)?.len();
        println!("seeds: {t} {files} files, {inputs} inputs");
    }
    codec::seeds(&raw, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_is_the_control_block_then_the_file() {
        let mut out = Vec::new();
        assert!(frame(&CONTROLS[3], b"\x1a\x45\xdf\xa3", &mut out).is_ok());
        assert_eq!(out.len(), 20);
        assert_eq!(&out[..16], b"\x01\x0c\x00\x01\x05\x00\x00\x80\x00\x00\x00\x00\x04\x00\x00\x00");
        assert_eq!(&out[16..], b"\x1a\x45\xdf\xa3");
    }

    #[test]
    fn every_control_block_carries_its_own_seed() {
        let all = CONTROLS.iter().chain(&MP4_CONTROLS).chain(&VARIANT_CONTROLS);
        for (i, c) in all.enumerate() {
            assert_eq!(u32::from_le_bytes([c[12], c[13], c[14], c[15]]), i as u32 + 1);
        }
    }

    #[test]
    fn formats_take_their_extra_controls() {
        let count = |t| CONTROLS.len() + extra_controls(t).len();
        assert_eq!(count("mkv"), 7);
        assert_eq!(count("mp4"), 9);
        for t in ["ts", "avi", "ogg", "flv", "ps"] {
            assert_eq!(count(t), 10);
        }
    }

    #[test]
    fn the_size_cap_leaves_room_for_the_control_block() {
        assert_eq!(MAX_RAW + 16, 1 << 20);
    }

    #[test]
    fn clip_outputs_are_unique_and_in_a_format_dir() {
        let mut names: Vec<(&str, &str)> = CLIPS.iter().map(|c| (c.dir, c.name)).collect();
        assert!(names.iter().all(|(d, _)| FORMATS.contains(d)));
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CLIPS.len());
        assert_eq!(CLIPS.len(), 83);
    }
}
