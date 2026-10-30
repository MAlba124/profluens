//! Each encoder ends a stream only after every staged frame is out, under a pool that is dry at
//! the end and frees a pass later. Drives the element as the scheduler does, no device needed.

// Test code. The allocation ban is for element hot paths.
#![allow(clippy::disallowed_methods)]

use profluens_core::bus::Bus;
use profluens_core::ctx::Ctx;
use profluens_core::element::Element;
use profluens_core::event::Event;
use profluens_core::id::{ElementId, FormatId, SrcPad};
use profluens_core::memory::{Pool, PoolConfig};
use profluens_core::sched::process_once;
use profluens_core::time::Timestamp;

use crate::enc::{Staged, Stamp};
use crate::{Settings, VaapiAv1Enc, VaapiH264Enc, VaapiH265Enc, VaapiVp8Enc, VaapiVp9Enc};

const FRAMES: u64 = 7;
const SLOTS: u32 = 2;
const PASSES_MAX: u32 = 64;

/// The frames an encoder staged, a dry output pool leaves them there.
pub(crate) trait Stage {
    fn staged(&mut self) -> &mut Staged;
}

/// Every pts out, in order, before the end is let through.
fn ends_after_every_frame(name: &str, mut el: impl Element + Stage) {
    let pool = Pool::with_config(PoolConfig::heap(4096, SLOTS));
    let (bus_tx, _bus) = Bus::channel();
    let mut ctx = Ctx::new(pool, bus_tx, ElementId(0), FormatId(0), 1);
    ctx.configure_pads(1, 1);
    for i in 0..FRAMES {
        let stamp =
            Stamp { pts: Timestamp(i), duration: Timestamp::NONE, key: false, preview: false };
        el.staged().push(vec![i as u8; 64], stamp);
    }
    el.event(&mut ctx, &Event::Eos).expect("eos");
    let mut out = Vec::new();
    let mut passes = 0;
    loop {
        // Dropped here, so the slots return before the next pass as a sink would.
        let mut batch = ctx.take_output(SrcPad(0));
        while let Some(buf) = batch.pop_front() {
            assert_eq!(buf.memory.data(), &[buf.pts.0 as u8; 64], "{name} bytes of {}", buf.pts.0);
            out.push(buf.pts.0);
        }
        if !ctx.take_end_held() {
            break;
        }
        passes += 1;
        assert!(passes < PASSES_MAX, "{name} holds the end with nothing left to send");
        process_once(&mut el, &mut ctx, true).expect("process");
    }
    let want: Vec<u64> = (0..FRAMES).collect();
    assert_eq!(out, want, "{name} lost frames staged at the end");
}

#[test]
fn h264enc_drains_a_dry_pool_before_the_end() {
    ends_after_every_frame("vaapih264enc", VaapiH264Enc::new());
}

#[test]
fn h265enc_drains_a_dry_pool_before_the_end() {
    ends_after_every_frame("vaapih265enc", VaapiH265Enc::new());
}

#[test]
fn vp8enc_drains_a_dry_pool_before_the_end() {
    ends_after_every_frame("vaapivp8enc", VaapiVp8Enc::new());
}

#[test]
fn vp9enc_drains_a_dry_pool_before_the_end() {
    ends_after_every_frame("vaapivp9enc", VaapiVp9Enc::new(Settings::default()));
}

#[test]
fn av1enc_drains_a_dry_pool_before_the_end() {
    ends_after_every_frame("vaapiav1enc", VaapiAv1Enc::new(Settings::default()));
}
