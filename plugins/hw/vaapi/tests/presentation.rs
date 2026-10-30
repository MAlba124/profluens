//! VA-API decoders and the presentation point on streams with reordering.
//! Fixtures are 320x240, 20 frames, two GOPs of 16 with B-pyramids, SEI NALs dropped.
//!
//! ```sh
//! SRC="testsrc2=size=320x240:rate=25:duration=1,noise=alls=12:allf=t"
//! ffmpeg -f lavfi -i "$SRC" -pix_fmt yuv420p -c:v libx264 -preset medium -qp 38 \
//!   -x264-params "keyint=16:min-keyint=16:scenecut=0:bframes=3:b-pyramid=normal:b-adapt=0:aud=1" \
//!   -vframes 20 a.mp4
//! ffmpeg -f lavfi -i "$SRC" -pix_fmt yuv420p -c:v libx265 -preset medium -x265-params \
//!   "qp=38:keyint=16:min-keyint=16:scenecut=0:bframes=3:b-pyramid=1:b-adapt=0:no-open-gop=1:aud=1" \
//!   -vframes 20 b.mp4
//! ffmpeg -i a.mp4 -c copy -bsf:v h264_mp4toannexb -f h264 a.h264
//! ffmpeg -i b.mp4 -c copy -bsf:v hevc_mp4toannexb -f hevc b.hevc
//! ```

// Test code.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::time::Timestamp;

use pf_vaapi::{VaapiH264Dec, VaapiH265Dec};

const H264: &[u8] = include_bytes!("fixtures/ipb_320x240.h264");
const H265: &[u8] = include_bytes!("fixtures/ipb_320x240.hevc");

/// Display index of each access unit in decode order, from ffprobe pts.
const H264_ORDER: [u64; 20] =
    [0, 4, 2, 1, 3, 8, 6, 5, 7, 12, 10, 9, 11, 15, 13, 14, 16, 19, 17, 18];
const H265_ORDER: [u64; 20] =
    [0, 4, 2, 1, 3, 8, 6, 5, 7, 12, 10, 9, 11, 15, 14, 13, 16, 19, 18, 17];
const SECOND_IDR: usize = 16;

const FRAME_NS: u64 = 40_000_000;
const SLOT: usize = 320 * 240 * 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Codec {
    H264,
    H265,
}

fn nal_starts(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    for i in 0..data.len().saturating_sub(3) {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push((if i > 0 && data[i - 1] == 0 { i - 1 } else { i }, i + 3));
        }
    }
    starts
}

impl Codec {
    fn stream(self) -> &'static [u8] {
        if self == Codec::H264 { H264 } else { H265 }
    }

    fn order(self) -> &'static [u64; 20] {
        if self == Codec::H264 { &H264_ORDER } else { &H265_ORDER }
    }

    fn family(self) -> &'static str {
        if self == Codec::H264 { "h264/annexb" } else { "h265/annexb" }
    }

    fn harness(self) -> Harness {
        if self == Codec::H264 {
            Harness::with_slot_size(VaapiH264Dec::new(), SLOT)
        } else {
            Harness::with_slot_size(VaapiH265Dec::new(), SLOT)
        }
    }

    fn opens_picture(self, data: &[u8], at: usize) -> bool {
        match self {
            Codec::H264 => matches!(data[at] & 0x1F, 1 | 5) && data[at + 1] & 0x80 != 0,
            Codec::H265 => (data[at] >> 1) & 0x3F < 32 && data[at + 2] & 0x80 != 0,
        }
    }

    fn units(self) -> Vec<Vec<u8>> {
        let data = self.stream();
        let starts = nal_starts(data);
        let mut cuts = vec![0];
        let mut open = false;
        let mut head = 0;
        for &(code, at) in &starts {
            if self.opens_picture(data, at) {
                if open {
                    cuts.push(head);
                }
                open = true;
                head = usize::MAX;
            } else if head == usize::MAX {
                head = code;
            }
        }
        let ends = cuts.iter().skip(1).copied().chain([data.len()]);
        let units: Vec<Vec<u8>> =
            cuts.iter().zip(ends).map(|(&a, b)| data[a..b].to_vec()).collect();
        assert_eq!(units.len(), self.order().len(), "{self:?}");
        units
    }
}

fn at(frames: u64) -> Timestamp {
    Timestamp::from_nanos(frames * FRAME_NS)
}

type Shown = (Timestamp, Vec<u8>, bool);

fn shown(buffers: Vec<Buffer>) -> Vec<Shown> {
    buffers
        .iter()
        .map(|b| (b.pts, b.memory.data().to_vec(), b.flags.contains(BufferFlags::PREVIEW)))
        .collect()
}

struct Run {
    codec: Codec,
    units: Vec<Vec<u8>>,
}

impl Run {
    fn new(codec: Codec, test: &str) -> Option<Run> {
        let supported = pf_vaapi::probe().is_some_and(|caps| caps.supports(codec.family()));
        if !supported {
            eprintln!("skip {test} for {codec:?}, no VA-API device decodes it");
            return None;
        }
        Some(Run { codec, units: codec.units() })
    }

    fn feed(&self, h: &mut Harness, range: std::ops::Range<usize>) -> Vec<Shown> {
        let mut out = Vec::new();
        for i in range {
            let mut buf = h.alloc(&self.units[i]);
            buf.pts = at(self.codec.order()[i]);
            h.push(buf).expect("push");
            out.extend(shown(h.drain_outputs()));
        }
        out
    }

    fn finish(&self, h: &mut Harness) -> Vec<Shown> {
        let tail = shown(h.eos().expect("eos"));
        let warnings: Vec<String> = h
            .bus_messages()
            .into_iter()
            .filter_map(|m| match m {
                BusMessage::Warning { error, .. } => Some(format!("{error:?}")),
                _ => None,
            })
            .collect();
        assert!(warnings.is_empty(), "{:?} warned, {warnings:?}", self.codec);
        tail
    }

    fn full(&self) -> Vec<Shown> {
        let mut h = self.codec.harness();
        let mut out = self.feed(&mut h, 0..self.units.len());
        out.extend(self.finish(&mut h));
        assert_eq!(out.len(), 20, "{:?}", self.codec);
        for (i, s) in out.iter().enumerate() {
            assert_eq!(s.0, at(i as u64), "{:?} display order, display pts", self.codec);
            assert_eq!(s.1.len(), 320 * 240 * 3 / 2);
        }
        out
    }
}

#[test]
fn reordered_pictures_leave_in_display_order_and_are_all_distinct() {
    for codec in [Codec::H264, Codec::H265] {
        let Some(run) = Run::new(codec, "reordered_pictures") else {
            continue;
        };
        let all = run.full();
        let mut distinct: Vec<&Vec<u8>> = all.iter().map(|s| &s.1).collect();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 20, "{codec:?}, so a swap of two would show");
    }
}

#[test]
fn from_the_point_on_a_seek_shows_what_a_full_decode_shows() {
    for codec in [Codec::H264, Codec::H265] {
        let Some(run) = Run::new(codec, "from_the_point_on_a_seek") else {
            continue;
        };
        let all = run.full();
        for (start, first) in [(0, 0), (SECOND_IDR, 16)] {
            for half in first * 2..=41 {
                let point = Timestamp::from_nanos(half * FRAME_NS / 2);
                let mut h = codec.harness();
                h.seek(0, point).expect("seek");
                let mut got = run.feed(&mut h, start..run.units.len());
                got.extend(run.finish(&mut h));
                let want: Vec<Shown> = all.iter().filter(|s| s.0 >= point).cloned().collect();
                let pts: Vec<Timestamp> = got.iter().map(|s| s.0).collect();
                assert!(
                    got == want,
                    "{codec:?} restart at unit {start}, point {point:?}, got {pts:?}"
                );
            }
        }
    }
}

#[test]
fn a_preview_seek_shows_the_keyframe_before_the_point_once() {
    for codec in [Codec::H264, Codec::H265] {
        let Some(run) = Run::new(codec, "a_preview_seek") else {
            continue;
        };
        let all = run.full();
        for (start, first) in [(0, 0), (SECOND_IDR, 16)] {
            for half in first * 2 + 1..(first + 16).min(20) * 2 {
                let point = Timestamp::from_nanos(half * FRAME_NS / 2);
                let mut h = codec.harness();
                h.seek_preview(0, point).expect("seek");
                let mut got = run.feed(&mut h, start..run.units.len());
                got.extend(run.finish(&mut h));
                let want: Vec<Shown> = all.iter().filter(|s| s.0 >= point).cloned().collect();
                let pts: Vec<(Timestamp, bool)> = got.iter().map(|s| (s.0, s.2)).collect();
                let Some((head, rest)) = got.split_first() else {
                    panic!("{codec:?} point {point:?}, nothing out")
                };
                assert!(
                    head.0 == at(first) && head.2,
                    "{codec:?} point {point:?}, the preview, got {pts:?}"
                );
                assert!(
                    head.1 == all[first as usize].1,
                    "{codec:?} point {point:?}, the preview's pixels"
                );
                assert!(
                    rest == want.as_slice(),
                    "{codec:?} restart at unit {start}, point {point:?}, got {pts:?}"
                );
            }
        }
    }
}

#[test]
fn a_skip_mid_stream_loses_and_changes_nothing_from_the_point_on() {
    for codec in [Codec::H264, Codec::H265] {
        let Some(run) = Run::new(codec, "a_skip_mid_stream") else {
            continue;
        };
        let all = run.full();
        for fed in (0..run.units.len()).step_by(3) {
            for target in 0..=20u64 {
                let mut h = codec.harness();
                let before = run.feed(&mut h, 0..fed);
                h.skip_to(at(target)).expect("skip");
                let mut after = run.feed(&mut h, fed..run.units.len());
                after.extend(run.finish(&mut h));
                assert!(
                    after.iter().all(|s| s.0 >= at(target)),
                    "{codec:?} fed {fed}, to {target}"
                );
                let got: Vec<&Shown> =
                    before.iter().chain(&after).filter(|s| s.0 >= at(target)).collect();
                let want: Vec<&Shown> = all.iter().filter(|s| s.0 >= at(target)).collect();
                assert!(got == want, "{codec:?} fed {fed}, skip to {target}");
            }
        }
    }
}

/// 1200 pictures through a pool of a few dozen surfaces, so a per-round leak runs it dry.
#[test]
fn no_surface_is_lost_to_a_picture_that_is_not_output() {
    for codec in [Codec::H264, Codec::H265] {
        let Some(run) = Run::new(codec, "no_surface_is_lost") else {
            continue;
        };
        let all = run.full();
        let mut h = codec.harness();
        for round in 0..60u64 {
            let point = at(3 + round % 8);
            h.seek(0, point).expect("seek");
            drop(h.drain_outputs());
            let mut got = run.feed(&mut h, 0..SECOND_IDR);
            let skipped = round % 2 == 1;
            if skipped {
                h.skip_to(at(18)).expect("skip");
            }
            got.extend(run.feed(&mut h, SECOND_IDR..run.units.len()));
            let mut expect = all.iter().filter(|s| s.0 >= point);
            for s in &got {
                let want = expect.find(|w| w.0 == s.0);
                assert!(want == Some(s), "{codec:?} round {round}, picture at {:?}", s.0);
            }
            assert!(skipped || got.len() >= 2, "{codec:?} round {round} showed {}", got.len());
            let warned = h.bus_messages().iter().any(|m| matches!(m, BusMessage::Warning { .. }));
            assert!(!warned, "{codec:?} round {round} warned, a surface did not come back");
        }
        let tail = run.finish(&mut h);
        assert!(!tail.is_empty(), "{codec:?}, the last round's tail");
    }
}
