//! A zero-copy decoder whose stream changes size or reference count while old frames are
//! still held at the sink. Needs a real device, skips without one.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::VecDeque;
use std::sync::Arc;

use pf_harness::Harness;
use profluens_core::bus::BusMessage;
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::{FrameRx, GpuFrame, GpuFrameChannel, GpuFrameHeader};
use pf_vaapi::va::{Display, ExportedSurface};
use pf_vaapi::{VaapiH264Dec, VaapiH265Dec};

const H264_IPB: &[u8] = include_bytes!("fixtures/ipb_320x240.h264");
/// Fewer surfaces than the 320x240 session, so the driver reissues only some old ids.
const H264_SMALL: &[u8] = include_bytes!("fixtures/p_160x96.h264");
const H264_P: &[u8] = include_bytes!("fixtures/p_320x240.h264");
const H264_REFS: &[u8] = include_bytes!("fixtures/refs16_320x240.h264");
const H265_IPB: &[u8] = include_bytes!("fixtures/ipb_320x240.hevc");
const H265_SMALL: &[u8] = include_bytes!("fixtures/p_160x96.hevc");
const H265_P: &[u8] = include_bytes!("fixtures/p_320x240.hevc");
const H265_REFS: &[u8] = include_bytes!("fixtures/refs6_320x240.hevc");
const FRAME_NS: u64 = 40_000_000;
const SLOTS: u32 = 64;
const SLOT: usize = 4096;
const HELD: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Codec {
    H264,
    H265,
}

impl Codec {
    fn family(self) -> &'static str {
        if self == Codec::H264 { "h264/annexb" } else { "h265/annexb" }
    }

    fn opens_picture(self, data: &[u8], at: usize) -> bool {
        match self {
            Codec::H264 => matches!(data[at] & 0x1F, 1 | 5) && data[at + 1] & 0x80 != 0,
            Codec::H265 => (data[at] >> 1) & 0x3F < 32 && data[at + 2] & 0x80 != 0,
        }
    }

    fn units(self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut cuts = vec![0];
        let (mut open, mut head) = (false, 0);
        for i in 0..data.len().saturating_sub(3) {
            if data[i..i + 3] != [0, 0, 1] {
                continue;
            }
            let code = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            if self.opens_picture(data, i + 3) {
                if open {
                    cuts.push(if head == usize::MAX { code } else { head });
                }
                (open, head) = (true, usize::MAX);
            } else if head == usize::MAX {
                head = code;
            }
        }
        let ends = cuts.iter().skip(1).copied().chain([data.len()]);
        cuts.iter().zip(ends).map(|(&a, b)| data[a..b].to_vec()).collect()
    }

    fn streams(self) -> [&'static [u8]; 4] {
        match self {
            Codec::H264 => [H264_IPB, H264_SMALL, H264_P, H264_REFS],
            Codec::H265 => [H265_IPB, H265_SMALL, H265_P, H265_REFS],
        }
    }

    fn harness(self, channel: &Arc<GpuFrameChannel>) -> Harness {
        match self {
            Codec::H264 => {
                Harness::with_pool(VaapiH264Dec::new_zerocopy(Arc::clone(channel)), SLOT, SLOTS)
            }
            Codec::H265 => {
                Harness::with_pool(VaapiH265Dec::new_zerocopy(Arc::clone(channel)), SLOT, SLOTS)
            }
        }
    }
}

fn device(codec: Codec, test: &str) -> bool {
    let found =
        pf_vaapi::probe().filter(|caps| caps.supports(codec.family())).is_some_and(|caps| {
            Display::open(&caps.device)
                .ok()
                .is_some_and(|display| ExportedSurface::probe(&display).is_ok())
        });
    if !found {
        eprintln!("skip {test} for {codec:?}, no VA-API device decodes and exports it");
    }
    found
}

fn token_of(bytes: &[u8]) -> u64 {
    GpuFrameHeader::from_bytes(bytes).expect("a video/gpu header").token
}

fn take(h: &mut Harness, frames: &mut FrameRx, held: &mut VecDeque<GpuFrame>) -> usize {
    let out = h.drain_outputs();
    for buf in &out {
        let mut frame =
            frames.take(token_of(buf.memory.data())).expect("the frame of a token put out");
        frame.close_fds();
        held.push_back(frame);
        while held.len() > HELD {
            held.pop_front();
        }
    }
    out.len()
}

fn feed(
    h: &mut Harness,
    frames: &mut FrameRx,
    held: &mut VecDeque<GpuFrame>,
    units: &[Vec<u8>],
    from: u64,
) -> usize {
    let mut shown = 0;
    for (i, unit) in units.iter().enumerate() {
        let mut buf = h.alloc(unit);
        buf.pts = Timestamp::from_nanos((from + i as u64) * FRAME_NS);
        h.push(buf).expect("push");
        shown += take(h, frames, held);
    }
    shown
}

fn play(codec: Codec, streams: &[&[u8]]) -> (u64, u64, Vec<String>) {
    let channel = GpuFrameChannel::new();
    let mut h = codec.harness(&channel);
    let mut frames = channel.take_rx().expect("the sink's end");
    let mut held = VecDeque::new();
    let (mut shown, mut at) = (0, 0);
    for data in streams {
        let units = codec.units(data);
        shown += feed(&mut h, &mut frames, &mut held, &units, at);
        at += units.len() as u64;
    }
    held.clear();
    h.crank().expect("crank");
    shown += take(&mut h, &mut frames, &mut held);
    held.clear();
    shown += h.eos().expect("eos").len();
    let said = h
        .bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            BusMessage::Warning { error, .. } | BusMessage::Error { error, .. } => {
                Some(format!("{error}"))
            }
            _ => None,
        })
        // The harness runs no preroll so video/gpu was never interned.
        .filter(|said| !said.starts_with("format announcement 'video/gpu'"))
        .collect();
    (shown as u64, at, said)
}

#[test]
fn a_size_change_with_frames_out_decodes_into_live_surfaces_only() {
    for codec in [Codec::H264, Codec::H265] {
        if !device(codec, "a_size_change_with_frames_out") {
            continue;
        }
        let [ipb, small, _, _] = codec.streams();
        let (shown, all, said) = play(codec, &[ipb, small, ipb, small]);
        assert!(said.is_empty(), "{codec:?} said {said:?}");
        assert_eq!(shown, all, "{codec:?}, every picture out");
        eprintln!("{codec:?}: {shown} pictures over three size changes");
    }
}

#[test]
fn a_rendition_of_more_references_at_the_same_size_gets_the_surfaces_it_needs() {
    for codec in [Codec::H264, Codec::H265] {
        if !device(codec, "a_rendition_of_more_references") {
            continue;
        }
        let [_, _, p, refs] = codec.streams();
        let (shown, all, said) = play(codec, &[p, refs, p, refs]);
        assert!(said.is_empty(), "{codec:?} said {said:?}");
        assert_eq!(shown, all, "{codec:?}, every picture out");
        eprintln!("{codec:?}: {shown} pictures over three changes of references");
    }
}
