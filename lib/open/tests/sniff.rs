#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

mod common;

use std::process::Command;

use common::*;
use pf_open::{Kind, OpenError, TrackKind};
use pf_probe::{PREFIX_LEN, probe};

/// What ffmpeg writes from `args` as `name`, None when it cannot, the test then skips.
fn ffmpeg(name: &str, args: &[&str]) -> Option<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("pf_open_sniff_{}_{name}", std::process::id()));
    let ok = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .status()
        .is_ok_and(|s| s.success());
    let got = std::fs::read(&path).ok().filter(|_| ok);
    let _ = std::fs::remove_file(&path);
    if got.is_none() {
        eprintln!("skipped: ffmpeg could not write {name}");
    }
    got
}

fn sniff(b: &[u8]) -> Option<Kind> {
    probe(&b[..b.len().min(PREFIX_LEN)]).ok()
}

/// Bare MPEG audio as ffmpeg writes it, the longest frames of layer III and layer II among them,
/// still sniffs and opens.
#[test]
fn real_mpeg_audio_sniffs() {
    let tone = ["-f", "lavfi", "-i", "sine=frequency=440:duration=2"];
    let lame = ["-c:a", "libmp3lame", "-id3v2_version", "0", "-b:a"];
    let cases: [(&str, &[&str], u32); 4] = [
        ("l3_320.mp3", &[&lame[..], &["320k", "-ar", "44100"]].concat(), 44_100),
        ("l3_320_32k.mp3", &[&lame[..], &["320k", "-ar", "32000"]].concat(), 32_000),
        ("lsf_32.mp3", &[&lame[..], &["32k", "-ar", "22050"]].concat(), 22_050),
        ("l2_384_32k.mp2", &["-c:a", "mp2", "-b:a", "384k", "-ar", "32000", "-f", "mp2"], 32_000),
    ];
    for (name, codec, rate) in cases {
        let Some(b) = ffmpeg(name, &[&tone[..], codec].concat()) else { return };
        assert_eq!(b[0], 0xFF, "{name} starts on a frame");
        assert_eq!(sniff(&b), Some(Kind::Mp3), "{name}");
        let o = open(&b);
        assert_eq!((o.tracks[0].family, o.tracks[0].rate), ("mp3", rate), "{name}");
    }
}

/// A program stream as ffmpeg writes it still sniffs and opens with its video and audio.
#[test]
fn a_real_program_stream_sniffs() {
    let args = [
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=128x96:rate=25:duration=2",
        "-f",
        "lavfi",
        "-i",
        "sine=duration=2",
        "-c:v",
        "mpeg2video",
        "-c:a",
        "mp2",
        "-f",
        "vob",
    ];
    let Some(b) = ffmpeg("av.vob", &args) else { return };
    assert_eq!(sniff(&b), Some(Kind::Ps));
    let o = open(&b);
    let got: Vec<_> = o.tracks.iter().map(|t| (t.kind, t.family)).collect();
    assert_eq!(got, [(TrackKind::Video, "mpeg2"), (TrackKind::Audio, "mp3")]);
}

/// A subtitle in UTF-8 or UTF-16 behind its byte order mark is no file pf opens.
#[test]
fn a_subtitle_text_is_unknown() {
    let text = "1\r\n00:00:01,000 --> 00:00:02,500\r\nHello there\r\n\r\n".repeat(100);
    let mut le = vec![0xFF, 0xFE];
    le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    let mut be = vec![0xFE, 0xFF];
    be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
    let bom8 = [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat();
    for b in [text.as_bytes(), &le, &be, &bom8, &le[..200]] {
        assert_eq!(sniff(b), None, "{:02X?}", &b[..4]);
        let (o, _) = open_with(b, pf_open::Want::ALL, pf_open::Caps::default());
        assert!(matches!(o, Err(OpenError::Unknown(_))), "{:02X?}", &b[..4]);
    }
}
