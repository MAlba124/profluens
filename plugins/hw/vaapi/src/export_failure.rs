//! A zero-copy decoder whose export of one picture fails, as iHD's `vaSyncSurface` does under
//! rapid seeks. Only that picture is lost. Needs a real device, skips without one.

// Test code. The allocation ban is for element hot paths.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use pf_harness::Harness;
use profluens_core::buffer::Buffer;
use profluens_core::bus::BusMessage;
use profluens_core::time::Timestamp;

use crate::ffi;
use crate::gpuframe::{FrameRx, GpuFrameChannel, GpuFrameHeader};
use crate::va::{Display, ExportedSurface, VaResult};
use crate::{VaapiH264Dec, VaapiH265Dec};

const H264: &[u8] = include_bytes!("../tests/fixtures/ipb_320x240.h264");
const H265: &[u8] = include_bytes!("../tests/fixtures/ipb_320x240.hevc");
const H264_ORDER: [u64; 20] =
    [0, 4, 2, 1, 3, 8, 6, 5, 7, 12, 10, 9, 11, 15, 13, 14, 16, 19, 17, 18];
const H265_ORDER: [u64; 20] =
    [0, 4, 2, 1, 3, 8, 6, 5, 7, 12, 10, 9, 11, 15, 14, 13, 16, 19, 18, 17];
const FRAME_NS: u64 = 40_000_000;
const FAILS: u32 = 2;

static EXPORTS: AtomicU32 = AtomicU32::new(0);

fn export_failing_one(dpy: ffi::VADisplay, surface: ffi::VASurfaceID) -> VaResult<ExportedSurface> {
    let nth = EXPORTS.fetch_add(1, Ordering::Relaxed);
    ExportedSurface::export(dpy, if nth == FAILS { ffi::VA_INVALID_SURFACE } else { surface })
}

fn units(data: &[u8], h265: bool) -> Vec<&[u8]> {
    let opens = |at: usize| match h265 {
        true => (data[at] >> 1) & 0x3F < 32 && data[at + 2] & 0x80 != 0,
        false => matches!(data[at] & 0x1F, 1 | 5) && data[at + 1] & 0x80 != 0,
    };
    let (mut cuts, mut open, mut head) = (vec![0], false, 0);
    for i in 0..data.len().saturating_sub(3) {
        if data[i..i + 3] != [0, 0, 1] {
            continue;
        }
        let code = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
        if opens(i + 3) {
            if open {
                cuts.push(head);
            }
            (open, head) = (true, usize::MAX);
        } else if head == usize::MAX {
            head = code;
        }
    }
    let ends = cuts.iter().skip(1).copied().chain([data.len()]);
    cuts.iter().zip(ends).map(|(&a, b)| &data[a..b]).collect()
}

fn show(
    family: &str,
    out: impl IntoIterator<Item = Buffer>,
    frames: &mut FrameRx,
    shown: &mut Vec<Timestamp>,
) {
    for buf in out {
        let header = GpuFrameHeader::from_bytes(buf.memory.data());
        let frame = header.and_then(|header| frames.take(header.token));
        assert!(
            frame.is_some(),
            "{family}, the buffer at {:?} is no frame of the channel",
            buf.pts
        );
        shown.push(buf.pts);
    }
}

fn device(family: &str) -> bool {
    let found = crate::probe().filter(|caps| caps.supports(family)).and_then(|caps| {
        let display = Display::open(&caps.device).ok()?;
        ExportedSurface::probe(&display).ok()
    });
    if found.is_none() {
        eprintln!("skip {family}, no VA-API device decodes and exports it");
    }
    found.is_some()
}

#[test]
fn a_picture_whose_export_fails_is_not_shown_and_the_rest_are() {
    for (family, data, order) in
        [("h264/annexb", H264, H264_ORDER), ("h265/annexb", H265, H265_ORDER)]
    {
        if !device(family) {
            continue;
        }
        EXPORTS.store(0, Ordering::Relaxed);
        let channel = GpuFrameChannel::new();
        let mut h = match family {
            "h264/annexb" => {
                let dec = VaapiH264Dec::new_zerocopy(Arc::clone(&channel))
                    .with_export(export_failing_one);
                Harness::with_pool(dec, 4096, 64)
            }
            _ => {
                let dec = VaapiH265Dec::new_zerocopy(Arc::clone(&channel))
                    .with_export(export_failing_one);
                Harness::with_pool(dec, 4096, 64)
            }
        };
        let mut frames = channel.take_rx().expect("the sink's end");
        let units = units(data, family == "h265/annexb");
        assert_eq!(units.len(), 20, "{family}");
        let mut shown = Vec::new();
        for (unit, at) in units.iter().zip(order) {
            let mut buf = h.alloc(unit);
            buf.pts = Timestamp::from_nanos(at * FRAME_NS);
            h.push(buf).expect("push");
            show(family, h.drain_outputs(), &mut frames, &mut shown);
        }
        show(family, h.eos().expect("eos"), &mut frames, &mut shown);
        let failed = h.bus_messages().into_iter().any(|m| match m {
            BusMessage::Warning { error, .. } => format!("{error}").contains("vaSyncSurface"),
            _ => false,
        });
        assert!(failed, "{family}, the failed export is warned of");
        let want: Vec<Timestamp> = (0..20)
            .filter(|n| *n != u64::from(FAILS))
            .map(|n| Timestamp::from_nanos(n * FRAME_NS))
            .collect();
        assert_eq!(shown, want, "{family}, all but the picture whose export failed");
    }
}
