//! `audiochop`, PCM cut into buffers of a few milliseconds to bound control latency.

use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::format::{AudioFormat, RAW_ANY_OFFER, announce_raw, negotiated_audio_format};
use crate::layout::negotiated_layout;
use crate::trim::span_at;

/// The longest piece.
pub const CHOP: Timestamp = Timestamp::from_millis(20);

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_ANY_OFFER }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &RAW_ANY_OFFER }];

// Cold path, once at construction.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "audiochop",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(AudioChop::new())),
    io: false,
    couplings: &[Coupling::through(0, 0)],
};

/// Cuts PCM buffers into slices of at most [`CHOP`], without copying.
#[derive(Default)]
pub struct AudioChop {
    input: Option<AudioFormat>,
    layout: Option<u64>,
}

impl AudioChop {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        AudioChop::default()
    }

    fn learn(&mut self, ctx: &mut Ctx) {
        let (now, layout) =
            (negotiated_audio_format(ctx, Self::IN), negotiated_layout(ctx, Self::IN));
        let Some(inp) = now.filter(|_| (now, layout) != (self.input, self.layout)) else {
            return;
        };
        (self.input, self.layout) = (now, layout);
        log!(
            &*ctx,
            Level::Debug,
            "format",
            rate = inp.sample_rate,
            channels = inp.channels,
            sample = inp.format.caps_name(),
            layout = layout.unwrap_or(0)
        );
        announce_raw(ctx, Self::OUT, inp, layout);
    }
}

fn piece_frames(rate: u32) -> usize {
    ((u128::from(rate) * u128::from(CHOP.0)) / 1_000_000_000).max(1) as usize
}

impl Element for AudioChop {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.learn(ctx);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.learn(ctx);
        while let Some(buf) = inputs.pop() {
            let known = self
                .input
                .filter(|f| f.frame_stride() > 0 && f.sample_rate > 0 && buf.pts.is_some());
            let Some(format) = known else {
                ctx.out(Self::OUT).push(buf);
                continue;
            };
            let (stride, rate) = (format.frame_stride(), format.sample_rate);
            let (frames, piece) = (buf.memory.len() / stride, piece_frames(rate));
            if frames <= piece {
                crate::stamp::forward_pcm(ctx, Self::OUT, buf, (stride, rate));
                continue;
            }
            let mut at = 0;
            while at < frames {
                let n = piece.min(frames - at);
                // the last slice carries any partial trailing frame, as the short path does
                let len = if at + n == frames { buf.memory.len() - at * stride } else { n * stride };
                let Some(memory) = buf.memory.slice(at * stride, len) else {
                    log!(&*ctx, Level::Warn, "slice_failed", at = at, frames = frames);
                    break;
                };
                let (pts, duration) = span_at(buf.pts, at, n, rate);
                let (dts, flags, fmt) = (Timestamp::NONE, buf.flags, buf.format);
                ctx.out(Self::OUT).push(Buffer {
                    memory,
                    pts,
                    dts,
                    duration,
                    flags,
                    format: fmt,
                    sync: None,
                });
                at += n;
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FormatChange(_) | Event::Eos) {
            self.learn(ctx);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{FAMILY, FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE};
    use crate::trim::{position_at, time_at};
    use pf_harness::Harness;
    use profluens_core::format::ValueDesc;

    fn rig(rate: i64) -> Harness {
        let mut h = Harness::new(AudioChop::new());
        let fields = [
            (FIELD_RATE, ValueDesc::Int(rate)),
            (FIELD_CHANNELS, ValueDesc::Int(2)),
            (FIELD_SAMPLE, ValueDesc::Id("s16")),
        ];
        h.fix_sink_format(AudioChop::IN, FAMILY, &fields);
        h
    }

    fn pcm(h: &mut Harness, first: u64, frames: u64, rate: u32) -> Buffer {
        let code = |k: u64| ((first + k) as i16).to_le_bytes();
        let bytes: Vec<u8> = (0..frames).flat_map(|k| [code(k), code(k)].concat()).collect();
        let mut buf = h.alloc(&bytes);
        buf.pts = time_at(first, rate);
        buf
    }

    #[test]
    fn a_buffer_is_cut_into_pieces_that_keep_every_frame_on_its_stamp() {
        for rate in [48_000u32, 44_100] {
            let mut h = rig(i64::from(rate));
            let mut first = 0;
            for frames in [4_608u64, 4_096, 300, 1] {
                let buf = pcm(&mut h, first, frames, rate);
                h.push(buf).expect("process");
                first += frames;
            }
            let out = h.drain_outputs();
            let mut next = 0u64;
            for buf in &out {
                let frames = buf.memory.len() / 4;
                assert!(frames <= piece_frames(rate), "a piece of {frames} frames at {rate}");
                assert_eq!(position_at(buf.pts, rate), next, "stamped as the frame it starts on");
                let end = time_at(next + frames as u64, rate);
                assert_eq!(
                    buf.pts.saturating_add(buf.duration),
                    end,
                    "it runs to the stamp of the next"
                );
                for (k, frame) in buf.memory.data().as_chunks::<4>().0.iter().enumerate() {
                    assert_eq!(i16::from_le_bytes([frame[0], frame[1]]), (next + k as u64) as i16);
                }
                next += frames as u64;
            }
            assert_eq!(next, first, "every frame once at {rate}");
            assert!(out.len() >= 10, "{} pieces", out.len());
        }
    }

    #[test]
    fn a_partial_trailing_frame_survives_the_slicing() {
        let mut h = rig(48_000);
        let bytes: Vec<u8> = (0..2_000 * 4 + 2).map(|k| k as u8).collect();
        let mut buf = h.alloc(&bytes);
        buf.pts = time_at(0, 48_000);
        h.push(buf).expect("process");
        let out = h.drain_outputs();
        let total: usize = out.iter().map(|b| b.memory.len()).sum();
        assert_eq!(total, 2_000 * 4 + 2, "every byte goes out");
    }

    #[test]
    fn a_buffer_with_no_stamp_passes_whole() {
        let mut h = rig(48_000);
        let mut buf = pcm(&mut h, 0, 4_800, 48_000);
        buf.pts = Timestamp::NONE;
        h.push(buf).expect("process");
        let out = h.drain_outputs();
        assert_eq!((out.len(), out[0].memory.len()), (1, 4_800 * 4));
    }

    #[test]
    fn a_piece_is_twenty_milliseconds_of_frames_and_never_none() {
        assert_eq!(piece_frames(48_000), 960);
        assert_eq!(piece_frames(44_100), 882);
        assert_eq!(piece_frames(8_000), 160);
        assert_eq!(piece_frames(1), 1);
    }
}
