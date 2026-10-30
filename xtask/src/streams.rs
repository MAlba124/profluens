//! The committed ffmpeg fixtures of the opus, ts, tsdemux and flv tests.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::Error;
use crate::util;

/// `fixtures-opus`, one sine per 5.1 channel, mapping family 1.
pub fn opus(root: &Path) -> Result<(), Error> {
    let src = "aevalsrc=0.25*sin(2*PI*440*t)|0.25*sin(2*PI*660*t)|0.25*sin(2*PI*880*t)|\
               0.5*sin(2*PI*60*t)|0.25*sin(2*PI*1100*t)|0.25*sin(2*PI*1320*t):s=48000:c=5.1:d=0.5";
    let mut cmd = Command::new("ffmpeg");
    cmd.env_remove("LD_LIBRARY_PATH")
        .current_dir(root.join("plugins/audio/opus/tests/fixtures"))
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", src])
        .args(["-c:a", "libopus", "-mapping_family", "1", "-b:a", "96k", "-bitexact"])
        .arg("sines_5_1.mka");
    util::run("ffmpeg", &mut cmd)
}

fn ffmpeg() -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.env_remove("LD_LIBRARY_PATH").args(["-v", "error", "-y", "-fflags", "+bitexact"]);
    cmd
}

const VIDEO: &str = "testsrc2=size=128x96:rate=15:duration=2";
const TONE: &str = "sine=frequency=440:sample_rate=48000:duration=2";
const AV: &[&str] = &["-f", "lavfi", "-i", VIDEO, "-f", "lavfi", "-i", TONE];
const V: &[&str] = &["-f", "lavfi", "-i", VIDEO];
const A: &[&str] = &["-f", "lavfi", "-i", TONE];

type TsClip = (&'static str, &'static [&'static str], &'static [&'static str]);

const TS: &[TsClip] = &[
    (
        "h264_aac",
        AV,
        &[
            "-c:v",
            "libx264",
            "-profile:v",
            "baseline",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
            "-bf",
            "0",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-ac",
            "2",
            "-metadata:s:a:0",
            "language=eng",
            "-flags",
            "+bitexact",
            "-f",
            "mpegts",
        ],
    ),
    (
        "hevc_ac3",
        AV,
        &[
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=error:keyint=15:bframes=0",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "ac3",
            "-b:a",
            "64k",
            "-ac",
            "2",
            "-metadata:s:a:0",
            "language=nor",
            "-flags",
            "+bitexact",
            "-f",
            "mpegts",
        ],
    ),
    (
        "hevc_eac3_dvb",
        AV,
        &[
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=error:keyint=15:bframes=0",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "eac3",
            "-b:a",
            "64k",
            "-ac",
            "2",
            "-metadata:s:a:0",
            "language=fra",
            "-flags",
            "+bitexact",
            "-mpegts_flags",
            "system_b",
            "-f",
            "mpegts",
        ],
    ),
    (
        "bframes",
        V,
        &[
            "-c:v",
            "libx264",
            "-profile:v",
            "main",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
            "-bf",
            "2",
            "-flags",
            "+bitexact",
            "-f",
            "mpegts",
        ],
    ),
    (
        "audio_mp2",
        A,
        &[
            "-c:a",
            "mp2",
            "-b:a",
            "64k",
            "-ac",
            "2",
            "-metadata:s:a:0",
            "language=deu",
            "-flags",
            "+bitexact",
            "-pes_payload_size",
            "0",
            "-f",
            "mpegts",
        ],
    ),
    (
        "aac_latm",
        A,
        &[
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-ac",
            "2",
            "-flags",
            "+bitexact",
            "-mpegts_flags",
            "latm",
            "-f",
            "mpegts",
        ],
    ),
    (
        "mpeg2",
        V,
        &[
            "-c:v",
            "mpeg2video",
            "-b:v",
            "200k",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
            "-bf",
            "0",
            "-flags",
            "+bitexact",
            "-f",
            "mpegts",
        ],
    ),
    // Starts 1.2 s before the 33-bit wrap.
    (
        "wrap",
        AV,
        &[
            "-c:v",
            "libx264",
            "-profile:v",
            "main",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
            "-bf",
            "2",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-ac",
            "2",
            "-flags",
            "+bitexact",
            "-muxdelay",
            "0",
            "-output_ts_offset",
            "95442.5",
            "-f",
            "mpegts",
        ],
    ),
];

const PROBES: [(&str, &[&str]); 2] = [
    (
        "streams.txt",
        &[
            "-show_entries",
            "stream=index,id,codec_name,codec_type:stream_tags=language",
            "-of",
            "compact=nk=0",
        ],
    ),
    (
        "packets.csv",
        &["-show_entries", "packet=stream_index,pts,dts,size,pos,flags", "-of", "csv=p=0"],
    ),
];

/// `fixtures-ts <out>`, the streams and their ffprobe oracles, regenerated together.
pub fn ts(out: &Path) -> Result<(), Error> {
    util::mkdir_p(out)?;
    for &(name, input, args) in TS {
        util::run("ffmpeg", ffmpeg().args(input).args(args).arg(out.join(format!("{name}.ts"))))?;
    }
    for &(name, _, _) in TS {
        let stream = out.join(format!("{name}.ts"));
        for (suffix, args) in PROBES {
            let table = out.join(format!("{name}.{suffix}"));
            let file = File::create(&table).map_err(|e| util::io_err(&table, e))?;
            let mut cmd = Command::new("ffprobe");
            cmd.env_remove("LD_LIBRARY_PATH").args(["-v", "error"]).args(args).arg(&stream);
            util::exec("ffprobe", cmd.stdout(Stdio::from(file)), true)?;
        }
    }
    Ok(())
}

/// `fixtures-tsdemux <out>`, 24 kHz AAC on PID 0x101, for a rate change across a splice.
pub fn tsdemux(out: &Path) -> Result<(), Error> {
    util::mkdir_p(out)?;
    let mut cmd = ffmpeg();
    cmd.args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=24000:duration=1"])
        .args(["-c:a", "aac", "-b:a", "32k", "-ac", "2", "-streamid", "0:257"])
        .args(["-flags", "+bitexact", "-f", "mpegts"])
        .arg(out.join("aac_24k.ts"));
    util::run("ffmpeg", &mut cmd)?;
    util::run_live("ls", Command::new("ls").arg("-l").arg(out))
}

const FLV_TONE: &str = "sine=frequency=440:sample_rate=44100:duration=2";
const FLV_AV: &[&str] = &["-f", "lavfi", "-i", VIDEO, "-f", "lavfi", "-i", FLV_TONE];
const FLV_A: &[&str] = &["-f", "lavfi", "-i", FLV_TONE];
const FLV_V: &[&str] = &["-f", "lavfi", "-i", VIDEO];
const FLV_LONG: &[&str] = &[
    "-f",
    "lavfi",
    "-i",
    "testsrc2=size=128x96:rate=15:duration=4",
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440:sample_rate=44100:duration=4",
];
const INDEXED: &str = "add_keyframe_index";

/// Each with `-f flv`. Enhanced FLV for HEVC, AV1 and VP9 is FFmpeg 6.1 and later.
const FLV: &[TsClip] = &[
    (
        "h264_aac",
        FLV_LONG,
        &[
            "-c:v",
            "libx264",
            "-profile:v",
            "main",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
            "-bf",
            "2",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-ac",
            "2",
            "-flvflags",
            INDEXED,
        ],
    ),
    (
        "h264_mp3",
        FLV_AV,
        &[
            "-c:v",
            "libx264",
            "-profile:v",
            "baseline",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "64k",
            "-ac",
            "2",
        ],
    ),
    (
        "hevc_aac",
        FLV_AV,
        &[
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=error:keyint=15:bframes=2",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-ac",
            "2",
            "-flvflags",
            INDEXED,
        ],
    ),
    ("av1", FLV_V, &["-c:v", "libsvtav1", "-preset", "12", "-g", "15", "-svtav1-params", "lp=1"]),
    (
        "vp9_pcm",
        FLV_AV,
        &[
            "-t",
            "1",
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-b:v",
            "100k",
            "-g",
            "15",
            "-c:a",
            "pcm_s16le",
            "-ar",
            "11025",
            "-ac",
            "1",
        ],
    ),
    ("aac", FLV_A, &["-c:a", "aac", "-b:a", "64k", "-ac", "2", "-flvflags", INDEXED]),
    (
        "flv1_mp3",
        FLV_AV,
        &[
            "-t",
            "1",
            "-c:v",
            "flv1",
            "-b:v",
            "64k",
            "-g",
            "15",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "32k",
        ],
    ),
    ("nellymoser", FLV_A, &["-t", "1", "-c:a", "nellymoser", "-ar", "22050", "-ac", "1"]),
];

/// `fixtures-flv <out>`, the flv fixtures and their ffprobe packet tables.
pub fn flv(out: &Path) -> Result<(), Error> {
    util::mkdir_p(out)?;
    for &(name, input, args) in FLV {
        let stream = out.join(format!("{name}.flv"));
        let mut cmd = ffmpeg();
        cmd.args(input).args(args).args(["-flags", "+bitexact", "-f", "flv"]).arg(&stream);
        util::run("ffmpeg", &mut cmd)?;
        let (suffix, args) = PROBES[1];
        let table = out.join(format!("{name}.{suffix}"));
        let file = File::create(&table).map_err(|e| util::io_err(&table, e))?;
        let mut cmd = Command::new("ffprobe");
        cmd.env_remove("LD_LIBRARY_PATH").args(["-v", "error"]).args(args).arg(&stream);
        util::exec("ffprobe", cmd.stdout(Stdio::from(file)), true)?;
    }
    util::run_live("ls", Command::new("ls").arg("-l").arg(out))
}
