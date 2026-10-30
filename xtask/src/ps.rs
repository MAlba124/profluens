//! `fixtures-ps <out>`, the program stream fixtures of pf-ps and their ffprobe oracles.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::Error;
use crate::util;

const VIDEO: &str = "testsrc2=size=128x96:rate=15:duration=1.2";
/// MPEG-1 has no 15 fps (ISO/IEC 11172-2 2.4.3.2).
const VIDEO_25: &str = "testsrc2=size=128x96:rate=25:duration=1.2";
const TONE: &str = "sine=frequency=440:sample_rate=48000:duration=1.2";
const HALF: &str = "sine=frequency=440:sample_rate=48000:duration=0.5";
const QUARTER: &str = "sine=frequency=440:sample_rate=48000:duration=0.25";

const AV: &[&str] = &["-f", "lavfi", "-i", VIDEO, "-f", "lavfi", "-i", TONE];
const V: &[&str] = &["-f", "lavfi", "-i", VIDEO];
const AV_25: &[&str] = &["-f", "lavfi", "-i", VIDEO_25, "-f", "lavfi", "-i", TONE];

const MPEG2: &[&str] = &["-c:v", "mpeg2video", "-b:v", "150k", "-g", "12", "-bf", "2"];

type Clip = (&'static str, &'static [&'static str], &'static [&'static [&'static str]]);

pub const CLIPS: &[Clip] = &[
    // The dvd muxer adds the navigation packs of private stream 2.
    ("mpeg2_ac3.vob", AV, &[MPEG2, &["-c:a", "ac3", "-b:a", "64k", "-ac", "2", "-f", "dvd"]]),
    ("mpeg2_mp2.vob", AV, &[MPEG2, &["-c:a", "mp2", "-b:a", "64k", "-ac", "2", "-f", "vob"]]),
    (
        "mpeg1_mp2.mpg",
        AV_25,
        &[
            &["-c:v", "mpeg1video", "-b:v", "150k", "-g", "12", "-bf", "2"],
            &["-c:a", "mp2", "-b:a", "64k", "-ac", "2", "-f", "mpeg"],
        ],
    ),
    ("lpcm16.vob", &["-f", "lavfi", "-i", HALF], &[&["-c:a", "pcm_dvd", "-ac", "2", "-f", "vob"]]),
    (
        "lpcm24.vob",
        &["-f", "lavfi", "-i", QUARTER],
        &[&["-c:a", "pcm_dvd", "-sample_fmt", "s32", "-ac", "2", "-f", "vob"]],
    ),
    (
        "dts.vob",
        &["-f", "lavfi", "-i", QUARTER],
        &[&["-c:a", "dca", "-strict", "-2", "-ac", "2", "-f", "vob"]],
    ),
    (
        "h264.vob",
        V,
        &[
            &["-c:v", "libx264", "-preset", "fast", "-g", "12", "-bf", "2", "-pix_fmt", "yuv420p"],
            &["-f", "vob"],
        ],
    ),
    // Starts 0.72 s before the 33-bit wrap.
    (
        "wrap.vob",
        AV,
        &[
            MPEG2,
            &["-c:a", "ac3", "-b:a", "64k", "-ac", "2", "-output_ts_offset", "95443", "-f", "vob"],
        ],
    ),
];

const PROBES: [(&str, &[&str]); 2] = [
    (
        "streams.txt",
        &[
            "-show_entries",
            "stream=index,id,codec_name,codec_type,sample_rate,channels",
            "-of",
            "compact=nk=0",
        ],
    ),
    (
        "packets.csv",
        &["-show_entries", "packet=stream_index,pts,dts,size,pos,flags", "-of", "csv=p=0"],
    ),
];

/// `fixtures-ps <out>`, the streams and their ffprobe oracles, regenerated together.
pub fn fixtures(out: &Path) -> Result<(), Error> {
    util::mkdir_p(out)?;
    for &(name, input, args) in CLIPS {
        let mut cmd = Command::new("ffmpeg");
        cmd.env_remove("LD_LIBRARY_PATH").args(["-v", "error", "-y", "-fflags", "+bitexact"]);
        cmd.args(input).args(args.iter().flat_map(|a| a.iter())).args(["-flags", "+bitexact"]);
        util::run("ffmpeg", cmd.arg(out.join(name)))?;
    }
    for &(name, _, _) in CLIPS {
        let stream = out.join(name);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_clip_names_its_container() {
        for (name, _, args) in CLIPS {
            let last = args.last().and_then(|a| a.last()).copied();
            assert!(matches!(last, Some("vob" | "dvd" | "mpeg")), "{name}");
            assert!(name.ends_with(".vob") || name.ends_with(".mpg"), "{name}");
        }
    }
}
