// Test code, not an element.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::process::Command;

use pf_mkv::codec::{pcm_sample, track_family};
use pf_mkv::ebml::id;
use pf_mkv::{MatroskaReader, Parsed};

/// ffmpeg's 24-bit six channel PCM with its BitDepth set to 20, as a 20-bit WAV remuxes.
fn twenty_bits() -> Option<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mkv_pcm");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("s24_six.mkv");
    let ok = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:d=1",
        ])
        .args(["-ac", "6", "-c:a", "pcm_s24le"])
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipped: ffmpeg could not make the PCM");
        return None;
    }
    let mut file = std::fs::read(&path).ok()?;
    let at = file.windows(4).position(|w| w == [0x62, 0x64, 0x81, 24]).expect("a BitDepth of 24");
    file[at + 3] = 20;
    Some(file)
}

#[test]
fn a_twenty_bit_pcm_track_is_named_s24_and_its_blocks_hold_whole_samples() {
    let Some(file) = twenty_bits() else { return };
    let cl = file.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster");
    let parsed = Parsed::of(&file[..cl]).expect("a head");
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
