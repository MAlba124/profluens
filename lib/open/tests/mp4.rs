#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

mod common;

use common::*;
use pf_open::TrackKind;
use profluens_audio::layout::{BL, BR, FC, FL, FR, LFE};

const SINE: [&str; 4] = ["-f", "lavfi", "-i", "sine=d=1:r=48000"];

/// The sample entry's channelcount set to `n`, as muxers that state two for any layout write it.
fn said_channels(mut b: Vec<u8>, entry: &[u8; 4], n: u16) -> Vec<u8> {
    let at = find_nth(&b, entry, 0) + 4 + 16;
    b[at..at + 2].copy_from_slice(&n.to_be_bytes());
    b
}

fn audio(b: &[u8]) -> (u16, u32, u64) {
    let o = open(b);
    let t = o.tracks.iter().find(|t| t.kind == TrackKind::Audio).unwrap();
    (t.channels, t.rate, t.layout)
}

/// AAC states its channels in the AudioSpecificConfig and E-AC-3 in its `dec3`, a sample entry
/// that says two for 5.1 is not taken.
#[test]
fn five_one_audio_states_its_channels_from_the_codec_record() {
    let six = FL | FR | FC | LFE | BL | BR;
    let Some(aac) = ffmpeg("aac_51.mp4", &[&SINE[..], &["-ac", "6", "-c:a", "aac"]].concat())
    else {
        return;
    };
    assert_eq!(audio(&aac), (6, 48_000, six));
    assert_eq!(audio(&said_channels(aac, b"mp4a", 2)), (6, 48_000, six));
    let Some(eac3) = ffmpeg("eac3_51.mp4", &[&SINE[..], &["-ac", "6", "-c:a", "eac3"]].concat())
    else {
        return;
    };
    assert_eq!(audio(&eac3).0, 6);
    assert_eq!(audio(&said_channels(eac3, b"ec-3", 2)).0, 6);
    let Some(ac3) = ffmpeg("ac3_51.mp4", &[&SINE[..], &["-ac", "6", "-c:a", "ac3"]].concat())
    else {
        return;
    };
    assert_eq!(audio(&said_channels(ac3, b"ac-3", 2)).0, 6);
}

/// ffmpeg's fragmented form the sweep found, 6 s at 25 fps in 1 s fragments, the video 80 ms
/// late as an empty edit in a `moov` whose durations are 0, the edit to the media's end.
fn late_fragments(name: &str, flags: &str) -> Option<Vec<u8>> {
    let movflags = format!("frag_keyframe+empty_moov+default_base_moof+delay_moov{flags}");
    ffmpeg(
        name,
        &[
            "-itsoffset",
            "0.08",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=d=6:s=160x96:r=25",
            "-f",
            "lavfi",
            "-i",
            "sine=d=6:r=48000",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "libx264",
            "-g",
            "25",
            "-c:a",
            "aac",
            "-use_editlist",
            "1",
            "-movflags",
            &movflags,
        ],
    )
}

/// A fragmented movie lasts to the end of its last fragment, not its empty edit, whether a
/// `sidx` leads each fragment, one indexes them all or none does, its frame rate is its rows'.
#[test]
fn a_fragmented_movie_without_mehd_lasts_its_fragments() {
    const SECOND: u64 = 1_000 * MS;
    for (name, flags) in [("dash.mp4", "+dash"), ("global.mp4", "+global_sidx"), ("plain.mp4", "")]
    {
        let Some(b) = late_fragments(name, flags) else { return };
        let o = open(&b);
        assert!(o.duration_ns.abs_diff(6 * SECOND) < SECOND / 10, "{name} {}", o.duration_ns);
        let video = o.tracks.iter().find(|t| t.kind == TrackKind::Video).unwrap();
        assert_eq!(video.frame_ns, 40 * MS, "{name}");
        assert!(o.tracks.iter().all(|t| t.duration_ns.abs_diff(6 * SECOND) < SECOND / 10));
        let pf_open::Index::Mp4(tables) = &o.index else { panic!("{name} {:?}", o.index) };
        let index = tables.seek_index(&|_| false);
        let last = index.entries.last().map(|e| e.time_ns);
        assert_eq!((index.entries.len(), last), (6, Some(5 * SECOND)), "{name}");
        let (tracks, _) = open_with(&b, pf_open::Want::TRACKS, pf_open::Caps::default());
        assert_eq!(tracks.unwrap().duration_ns, o.duration_ns, "{name} described alone");
    }
}
