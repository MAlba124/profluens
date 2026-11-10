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
