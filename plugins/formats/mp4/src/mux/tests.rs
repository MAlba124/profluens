// Tests own their vectors.
#![allow(clippy::disallowed_methods)]

use super::*;

#[test]
fn opus_toc_gives_the_packet_duration() {
    // CELT 20 ms one frame, SILK 60 ms two frames, code 3 four CELT frames of 10 ms and 2.5 ms.
    assert_eq!(opus_samples(&[0xF8]), 960);
    assert_eq!(opus_samples(&[0x19]), 5760);
    assert_eq!(opus_samples(&[0xF3, 0x04]), 4 * 480);
    assert_eq!(opus_samples(&[0xE3, 0x04]), 4 * 120);
    assert_eq!(opus_samples(&[]), 0);
}

#[test]
fn mpeg_audio_headers_give_samples_rate_and_channels() {
    assert_eq!(mp3_samples(&[0xFF, 0xFB, 0x90, 0x44]), 1152);
    assert_eq!(mp3_rate_channels(&[0xFF, 0xFB, 0x90, 0x44]), Some((44_100, 2)));
    assert_eq!(mp3_samples(&[0xFF, 0xF3, 0x90, 0xC4]), 576);
    assert_eq!(mp3_rate_channels(&[0xFF, 0xF3, 0x90, 0xC4]), Some((22_050, 1)));
    assert_eq!(mp3_samples(&[0x00, 0x00, 0x00, 0x00]), 0);
}

#[test]
fn flac_frame_headers_give_the_block_size() {
    assert_eq!(flac_samples(&[0xFF, 0xF8, 0xC9, 0x18, 0x00]), 4096);
    assert_eq!(flac_samples(&[0xFF, 0xF8, 0x79, 0x18, 0x05, 0x04, 0x7F]), 1152);
    assert_eq!(flac_samples(&[0xFF, 0xF8, 0x69, 0x18, 0x05, 0xFF]), 256);
    assert_eq!(flac_samples(&[0xFF, 0xF8, 0x69]), 0, "cut short");
}

#[test]
fn adts_headers_give_an_audio_specific_config() {
    let adts = [0xFF, 0xF1, 0x4C, 0x80, 0x01, 0x3F, 0xFC];
    let (asc, rate, channels) = adts_config(&adts).unwrap();
    assert_eq!((asc, rate, channels), (vec![0x11, 0x90], 48_000, 2));
    assert_eq!(asc_rate_channels(&[0x11, 0x90]), (48_000, 2));
    assert!(adts_config(&[0x11, 0x90]).is_none());
    assert_eq!(aac_hint(&[0x11, 0x90], 48_000), Hint::Fixed(1024));
    // HE-AAC signalled implicitly, a 24 kHz core in a 48 kHz timescale.
    assert_eq!(aac_hint(&[0x13, 0x10], 48_000), Hint::Fixed(2048));
    // frameLengthFlag, 960 samples.
    assert_eq!(aac_hint(&[0x11, 0x94], 48_000), Hint::Fixed(960));
    assert_eq!(aac_hint(&[0x11, 0x90], 44_100), Hint::None, "no whole count of ticks");
}
