//! `audiojoin`, where an audio branch is switched with no gap and no repeated frame.

use profluens_core::batch::Inputs;
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::format::{
    AudioFormat, FAMILY, FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE, announce_raw,
    negotiated_audio_format,
};
use crate::layout::negotiated_layout;
use crate::trim::{position_at, time_at, trim_frames};

static SAMPLE_VALUES: [ValueDesc; 5] = [
    ValueDesc::Id("u8"),
    ValueDesc::Id("s16"),
    ValueDesc::Id("s24"),
    ValueDesc::Id("s32"),
    ValueDesc::Id("f32"),
];
static FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: FIELD_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: FIELD_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: FIELD_SAMPLE,
        allowed: ConstraintDesc::Set(&SAMPLE_VALUES),
        preferred: None,
    },
];
static OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &FIELDS }];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

// Cold path, once at construction.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "audiojoin",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(AudioJoin::new())),
    io: false,
    couplings: &[Coupling::through(0, 0)],
};

/// Switches audio branches, cutting the new one to follow the last frame put out.
pub struct AudioJoin {
    input: Option<AudioFormat>,
    layout: Option<u64>,
    end: Timestamp,
    cut: Option<Timestamp>,
}

impl AudioJoin {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        AudioJoin { input: None, layout: None, end: Timestamp::NONE, cut: None }
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

impl Default for AudioJoin {
    fn default() -> Self {
        Self::new()
    }
}

impl Element for AudioJoin {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        (self.end, self.cut) = (Timestamp::NONE, None);
        self.learn(ctx);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.learn(ctx);
        while let Some(mut buf) = inputs.pop() {
            let placed = self.input.filter(|f| f.frame_stride() > 0 && buf.pts.is_some());
            let Some(format) = placed else {
                ctx.out(Self::OUT).push(buf);
                continue;
            };
            let (stride, rate) = (format.frame_stride(), format.sample_rate);
            let mut frames = buf.memory.len() / stride;
            if let Some(cut) = self.cut {
                let trim = trim_frames(buf.pts, frames, rate, Some(cut));
                if trim.skip == frames && frames > 0 {
                    log!(&*ctx, Level::Trace, "join_drop", pts = buf.pts, cut = cut);
                    ctx.count_drop();
                    continue;
                }
                let kept =
                    buf.memory.slice(trim.skip * stride, buf.memory.len() - trim.skip * stride);
                if let Some(kept) = kept.filter(|_| trim.skip > 0) {
                    (buf.memory, buf.pts, buf.duration) = (kept, trim.pts, trim.duration);
                    frames -= trim.skip;
                } else if trim.skip > 0 {
                    log!(&*ctx, Level::Warn, "join_cut_failed", skip = trim.skip, cut = cut);
                }
                if trim.skip == 0 && buf.pts > cut {
                    log!(&*ctx, Level::Warn, "join_gap", pts = buf.pts, cut = cut);
                }
                log!(&*ctx, Level::Debug, "join_cut", skip = trim.skip, cut = cut);
                self.cut = None;
            }
            self.end = time_at(position_at(buf.pts, rate) + frames as u64, rate);
            ctx.out(Self::OUT).push(buf);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::SinkLinked { on: true, .. } => {
                log!(&*ctx, Level::Debug, "branch_linked", end = self.end);
                self.cut = self.end.is_some().then_some(self.end);
            }
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush");
                (self.end, self.cut) = (Timestamp::NONE, None);
            }
            Event::FormatChange(_) | Event::Eos => self.learn(ctx),
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
// Tests build their buffers on the heap.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use pf_harness::Harness;
    use profluens_core::buffer::Buffer;

    fn rig() -> Harness {
        let mut h = Harness::new(AudioJoin::new());
        let fields = [
            (FIELD_RATE, ValueDesc::Int(48_000)),
            (FIELD_CHANNELS, ValueDesc::Int(2)),
            (FIELD_SAMPLE, ValueDesc::Id("s16")),
        ];
        h.fix_sink_format(AudioJoin::IN, FAMILY, &fields);
        h
    }

    fn pcm(h: &mut Harness, first: u64, frames: u64, track: u64) -> Buffer {
        let code = |k: u64| ((first + k + track) as i16).to_le_bytes();
        let bytes: Vec<u8> = (0..frames).flat_map(|k| [code(k), code(k)].concat()).collect();
        let mut buf = h.alloc(&bytes);
        buf.pts = time_at(first, 48_000);
        buf
    }

    fn frames(out: &[Buffer]) -> Vec<(u64, i16)> {
        let mut all = Vec::new();
        for buf in out {
            let first = position_at(buf.pts, 48_000);
            for (k, frame) in buf.memory.data().as_chunks::<4>().0.iter().enumerate() {
                all.push((first + k as u64, i16::from_le_bytes([frame[0], frame[1]])));
            }
        }
        all
    }

    #[test]
    fn a_new_upstream_continues_on_the_frame_after_the_last_one_put_out() {
        const TRACK: u64 = 10_000;
        let mut h = rig();
        for n in 0..3 {
            let buf = pcm(&mut h, n * 1_000, 1_000, 0);
            h.push(buf).expect("process");
        }
        h.push_event(Event::SinkLinked { pad: AudioJoin::IN, on: true }).expect("event");
        for n in 0..5 {
            let buf = pcm(&mut h, 1_700 + n * 512, 512, TRACK);
            h.push(buf).expect("process");
        }
        let out = h.drain_outputs();
        let got = frames(&out);
        let stamps: Vec<u64> = got.iter().map(|(at, _)| *at).collect();
        assert_eq!(
            stamps,
            (0..1_700 + 5 * 512).collect::<Vec<_>>(),
            "every frame once and in order"
        );
        for (at, code) in &got {
            let expected = if *at < 3_000 { *at as i16 } else { (*at + TRACK) as i16 };
            assert_eq!(
                *code, expected,
                "frame {at} is of the right track and is the one its stamp says"
            );
        }
        assert_eq!(out.len(), 3 + 3);
        assert_eq!(out[3].memory.len(), (1_700 + 3 * 512 - 3_000) * 4);
        assert_eq!(
            out[3].duration,
            time_at(1_700 + 3 * 512, 48_000).saturating_sub(time_at(3_000, 48_000))
        );
    }

    #[test]
    fn nothing_is_cut_from_a_stream_that_starts_behind_the_end_or_behind_a_flush() {
        let mut h = rig();
        let buf = pcm(&mut h, 0, 1_000, 0);
        h.push(buf).expect("process");
        h.push_event(Event::SinkLinked { pad: AudioJoin::IN, on: true }).expect("event");
        let buf = pcm(&mut h, 1_480, 480, 0);
        h.push(buf).expect("process");
        assert_eq!(frames(&h.drain_outputs()).len(), 1_480);
        h.push_event(Event::FlushStart).expect("event");
        h.push_event(Event::SinkLinked { pad: AudioJoin::IN, on: true }).expect("event");
        let buf = pcm(&mut h, 200, 480, 0);
        h.push(buf).expect("process");
        assert_eq!(
            frames(&h.drain_outputs()).first(),
            Some(&(200, 200)),
            "behind a flush it starts where it starts"
        );
    }

    #[test]
    fn a_stream_with_no_stamps_passes_whole() {
        let mut h = rig();
        let buf = pcm(&mut h, 0, 100, 0);
        h.push(buf).expect("process");
        h.push_event(Event::SinkLinked { pad: AudioJoin::IN, on: true }).expect("event");
        let mut buf = pcm(&mut h, 0, 100, 0);
        buf.pts = Timestamp::NONE;
        h.push(buf).expect("process");
        let out = h.drain_outputs();
        assert_eq!((out.len(), out[1].memory.len()), (2, 400));
    }
}
