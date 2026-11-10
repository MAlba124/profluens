// Test code, not an element.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::process::Command;

use pf_mkv::codec::{config_depth, pcm_sample, track_family};
use pf_mkv::ebml::id;
use pf_mkv::{MatroskaReader, Parsed};

fn ffmpeg(name: &str, args: &[&str]) -> Option<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mkv_describe");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(name);
    let ok = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipped: ffmpeg could not make {name}");
        return None;
    }
    std::fs::read(&path).ok()
}

fn parsed(file: &[u8]) -> Parsed {
    let cl = file.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster");
    Parsed::of(&file[..cl]).expect("a head")
}

/// ffmpeg's 24-bit six channel PCM with its BitDepth set to 20, as a 20-bit WAV remuxes.
fn twenty_bits() -> Option<Vec<u8>> {
    let tone = "sine=frequency=440:sample_rate=48000:d=1";
    let args = ["-f", "lavfi", "-i", tone, "-ac", "6", "-c:a", "pcm_s24le"];
    let mut file = ffmpeg("s24_six.mkv", &args)?;
    let at = file.windows(4).position(|w| w == [0x62, 0x64, 0x81, 24]).expect("a BitDepth of 24");
    file[at + 3] = 20;
    Some(file)
}

#[test]
fn a_twenty_bit_pcm_track_is_named_s24_and_its_blocks_hold_whole_samples() {
    let Some(file) = twenty_bits() else { return };
    let parsed = parsed(&file);
    let t = &parsed.tracks[0];
    assert_eq!((t.codec_id.as_str(), t.bit_depth, t.channels), ("A_PCM/INT/LIT", 20, 6));
    assert_eq!((track_family(t), pcm_sample(t)), ("audio/raw", Some("s24")));
    let mut r = MatroskaReader::new();
    r.push(&file).expect("a clean file");
    let mut bytes = 0;
    while let Some(f) = r.next_frame() {
        assert_eq!(f.data.len() % (3 * 6), 0, "whole six channel samples of 3 octets");
        bytes += f.data.len();
    }
    assert_eq!(bytes, 48_000 * 3 * 6, "a second of it");
}

fn ten_bit(name: &str, codec: &str) -> Option<Vec<u8>> {
    let pictures = "testsrc2=size=96x64:rate=25:d=1";
    let args = ["-f", "lavfi", "-i", pictures, "-pix_fmt", "yuv420p10le", "-c:v", codec];
    ffmpeg(name, &args)
}

#[test]
fn an_avc_or_hevc_depth_is_its_sps_depth() {
    let Some(avc) = ten_bit("avc10.mkv", "libx264") else { return };
    let t = &parsed(&avc).tracks[0];
    assert_eq!(config_depth(track_family(t), &t.codec_private), 10, "the avcC's SPS");
    let Some(hevc) = ten_bit("hevc10.mkv", "libx265") else { return };
    let t = &parsed(&hevc).tracks[0];
    let mut record = t.codec_private.clone();
    assert_eq!(config_depth(track_family(t), &record), 10);
    // a record whose bitDepthLumaMinus8 says 8, as some muxers write it
    record[17] &= !7;
    assert_eq!(config_depth(track_family(t), &record), 10, "the hvcC's SPS over its field");
}
