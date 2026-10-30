//! `audiogain`, linear volume with a declick ramp.

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{Constraint, ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::error::{AudioErrorKind as K, failed};
use crate::format::{
    AudioFormat, FAMILY, FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE, SampleFormat, announce_raw,
    negotiated_audio_format,
};
use crate::layout::negotiated_layout;
use crate::trim::span_at;

/// Gain clamp.
pub const GAIN_MIN: f32 = 0.0;
pub const GAIN_MAX: f32 = 4.0;

/// Declick ramp length in ms, an inaudible slope yet an instant-feeling volume key.
pub const RAMP_MS: usize = 5;

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
static OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: FAMILY, fields: &FIELDS }, OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc {
    name: "src",
    offers: &OFFERS, // the (unchanged) format is announced at runtime
}];

static PROPS: [PropDesc; 2] = [
    PropDesc { name: "gain", allowed: Constraint::Any, live: true },
    PropDesc { name: "mute", allowed: Constraint::Any, live: true },
];

// Cold path, once at construction.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "audiogain",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(AudioGain::new(1.0))),
    io: false,
    couplings: &[Coupling::through(0, 0)],
};

#[inline]
fn saturate(sample: i32, gain: f32, lo: i32, hi: i32) -> i32 {
    let v = (sample as f64 * gain as f64).round();
    v.clamp(lo as f64, hi as f64) as i32
}

#[inline]
fn scale_sample(fmt: SampleFormat, gain: f32, s: &mut [u8]) {
    match fmt {
        SampleFormat::F32 => {
            let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]) * gain;
            s[..4].copy_from_slice(&v.to_le_bytes());
        }
        SampleFormat::U8 => {
            s[0] = (saturate(s[0] as i32 - 128, gain, -128, 127) + 128) as u8;
        }
        SampleFormat::S16 => {
            let v = saturate(
                i16::from_le_bytes([s[0], s[1]]) as i32,
                gain,
                i16::MIN as i32,
                i16::MAX as i32,
            );
            s[..2].copy_from_slice(&(v as i16).to_le_bytes());
        }
        SampleFormat::S24 => {
            let raw = s[0] as i32 | (s[1] as i32) << 8 | (s[2] as i32) << 16;
            let v = saturate((raw << 8) >> 8, gain, -(1 << 23), (1 << 23) - 1);
            s[..3].copy_from_slice(&v.to_le_bytes()[..3]);
        }
        SampleFormat::S32 => {
            let v =
                saturate(i32::from_le_bytes([s[0], s[1], s[2], s[3]]), gain, i32::MIN, i32::MAX);
            s[..4].copy_from_slice(&v.to_le_bytes());
        }
    }
}

#[inline]
fn copy_concat(head: &[u8], body: &[u8], off: usize, dst: &mut [u8]) {
    let mut written = 0;
    if off < head.len() {
        written = (head.len() - off).min(dst.len());
        dst[..written].copy_from_slice(&head[off..off + written]);
    }
    if written < dst.len() {
        let start = (off + written) - head.len();
        let n = dst.len() - written;
        dst[written..].copy_from_slice(&body[start..start + n]);
    }
}

/// Linear volume with a declick ramp.
pub struct AudioGain {
    gain: f32,
    mute: bool,
    current: f32,
    ramp_remaining: usize,
    ramp_step: f32,
    input: Option<AudioFormat>,
    fed: bool,
    layout: Option<u64>,
    in_stride: usize,
    carry: Vec<u8>,
    warned_relatch: Option<AudioFormat>,
}

impl AudioGain {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    /// A gain stage at `gain` (1.0 is the copy-free unity path), its format discovered from
    /// upstream caps.
    // Cold path, once at construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new(gain: f32) -> Self {
        let mut el = Self {
            gain: 1.0,
            mute: false,
            current: 1.0,
            ramp_remaining: 0,
            ramp_step: 0.0,
            input: None,
            layout: None,
            in_stride: 0,
            carry: Vec::new(),
            warned_relatch: None,
            fed: false,
        };
        el.set_gain(gain);
        el.snap();
        el
    }

    /// A gain stage with the input format pinned at construction.
    pub fn with_input(input: AudioFormat, gain: f32) -> Self {
        let mut g = Self::new(gain);
        g.set_input(input);
        g.snap();
        g
    }

    /// The configured target gain, post-clamp.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    pub fn muted(&self) -> bool {
        self.mute
    }

    /// The multiplier the next output frame will be scaled by, the ramp's current position.
    pub fn applied_gain(&self) -> f32 {
        self.current
    }

    pub fn input_format(&self) -> Option<AudioFormat> {
        self.input
    }

    fn effective_gain(&self) -> f32 {
        if self.mute { 0.0 } else { self.gain }
    }

    fn is_unity(&self) -> bool {
        self.ramp_remaining == 0 && !self.mute && self.gain == 1.0 && self.current == 1.0
    }

    fn set_gain(&mut self, gain: f32) {
        if !gain.is_finite() {
            return;
        }
        self.gain = gain.clamp(GAIN_MIN, GAIN_MAX);
        self.retarget();
    }

    fn set_mute(&mut self, mute: bool) {
        if mute == self.mute {
            return;
        }
        self.mute = mute;
        self.retarget();
    }

    fn ramp_frames(&self) -> usize {
        match self.input {
            Some(inp) => ((inp.sample_rate as usize * RAMP_MS) / 1000).max(1),
            None => 1,
        }
    }

    fn retarget(&mut self) {
        let target = self.effective_gain();
        if target == self.current {
            self.ramp_remaining = 0;
            self.ramp_step = 0.0;
            return;
        }
        let frames = self.ramp_frames();
        self.ramp_remaining = frames;
        self.ramp_step = (target - self.current) / frames as f32;
    }

    fn snap(&mut self) {
        self.current = self.effective_gain();
        self.ramp_remaining = 0;
        self.ramp_step = 0.0;
    }

    #[inline]
    fn advance(&mut self) -> f32 {
        let g = self.current;
        if self.ramp_remaining > 0 {
            self.ramp_remaining -= 1;
            // Snap exactly, accumulating `ramp_step` would leave the gain a few ULPs off forever.
            self.current = if self.ramp_remaining == 0 {
                self.effective_gain()
            } else {
                self.current + self.ramp_step
            };
        }
        g
    }

    fn gain_from_value(ctx: &Ctx, v: Value) -> Option<f32> {
        match v {
            Value::Int(n) => Some(n as f32),
            Value::Rat(n, d) if d != 0 => Some(n as f32 / d as f32),
            Value::Rat(_, _) => None,
            Value::Id(id) => ctx.value_name(id).and_then(|s| s.parse::<f32>().ok()),
        }
    }

    fn mute_from_value(ctx: &Ctx, v: Value) -> Option<bool> {
        match v {
            Value::Int(n) => Some(n != 0),
            Value::Id(id) => match ctx.value_name(id)? {
                "1" | "true" | "on" | "yes" => Some(true),
                "0" | "false" | "off" | "no" => Some(false),
                _ => None,
            },
            Value::Rat(_, _) => None,
        }
    }

    fn set_input(&mut self, input: AudioFormat) {
        if self.input == Some(input) {
            return;
        }
        self.in_stride = input.frame_stride();
        self.input = Some(input);
        self.carry.clear();
        self.carry.reserve(self.in_stride.max(1));
    }

    fn unlearn(&mut self) {
        self.input = None;
        self.in_stride = 0;
    }

    fn learn_from_sink(&mut self, ctx: &mut Ctx) {
        if self.input.is_some() {
            return;
        }
        if let Some(fmt) = negotiated_audio_format(ctx, Self::IN) {
            self.set_input(fmt);
            self.layout = negotiated_layout(ctx, Self::IN);
            log!(
                &*ctx,
                Level::Info,
                "format",
                rate = fmt.sample_rate,
                channels = fmt.channels,
                sample = fmt.format.caps_name(),
                gain_milli = (self.effective_gain() * 1000.0) as i64
            );
            self.announce_output(ctx);
        }
    }

    fn announce_output(&self, ctx: &mut Ctx) {
        let Some(inp) = self.input else { return };
        announce_raw(ctx, Self::OUT, inp, self.layout);
    }

    fn warn_if_relatched(&mut self, ctx: &mut Ctx) {
        let Some(latched) = self.input else { return };
        let Some(offered) = negotiated_audio_format(ctx, Self::IN) else {
            return;
        };
        if offered == latched || self.warned_relatch == Some(offered) {
            return;
        }
        self.warned_relatch = Some(offered);
        let element = ctx.element();
        ctx.post(BusMessage::Warning {
            element,
            error: failed(ctx, DESC.name, K::FormatChangeIgnored { latched, offered }),
        });
    }

    fn apply(&mut self, pcm: &mut [u8]) {
        let Some(inp) = self.input else { return };
        let bps = inp.format.bytes();
        for frame in pcm.chunks_exact_mut(self.in_stride) {
            let g = self.advance();
            // Unity skips the multiply so it touches nothing, `NaN * 1.0` may re-quantise a
            // payload.
            if g == 1.0 {
                continue;
            }
            for s in frame.chunks_exact_mut(bps) {
                scale_sample(inp.format, g, s);
            }
        }
    }

    fn emit_gained(
        &mut self,
        ctx: &mut Ctx,
        head: &[u8],
        body: &[u8],
        base: Timestamp,
    ) -> Result<(), Error> {
        let stride = self.in_stride;
        if stride == 0 {
            return Err(failed(ctx, DESC.name, K::InputStrideUnknown));
        }
        let total = head.len() + body.len();
        debug_assert_eq!(total % stride, 0, "emit_gained takes whole frames");
        let rate = self.input.map_or(0, |f| f.sample_rate);
        let mut off = 0usize;
        while off < total {
            let mut buf = ctx.alloc();
            let capacity = buf.memory.capacity();
            let frames = capacity / stride;
            if frames == 0 {
                return Err(failed(ctx, DESC.name, K::SlotTooSmall { capacity, stride }));
            }
            let n = (frames * stride).min(total - off);
            let dst = &mut buf.memory.as_mut_full()[..n];
            copy_concat(head, body, off, dst);
            self.apply(dst);
            buf.memory.set_len(n);
            (buf.pts, buf.duration) = span_at(base, off / stride, n / stride, rate);
            ctx.out(Self::OUT).push(buf);
            off += n;
        }
        Ok(())
    }
}

impl Element for AudioGain {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(v) = ctx.prop("gain")
            && let Some(g) = Self::gain_from_value(ctx, v)
        {
            self.set_gain(g);
        }
        if let Some(v) = ctx.prop("mute")
            && let Some(m) = Self::mute_from_value(ctx, v)
        {
            self.set_mute(m);
        }
        self.announce_output(ctx);
        self.fed = false;
        self.learn_from_sink(ctx);
        self.carry.clear();
        self.snap();
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.input.is_none() {
            self.learn_from_sink(ctx);
        }

        while let Some(buf) = crate::stamp::take_pcm(ctx, &mut inputs) {
            self.fed = true;
            if self.input.is_none() {
                return Err(failed(ctx, DESC.name, K::PcmBeforeFormat));
            }
            let stride = self.in_stride;
            if stride == 0 {
                return Err(failed(ctx, DESC.name, K::InputStrideUnknown));
            }

            if self.is_unity() && self.carry.is_empty() && buf.memory.data().len() % stride == 0 {
                ctx.out(Self::OUT).push(buf);
                continue;
            }

            let mut data = buf.memory.data();
            let mut frame = std::mem::take(&mut self.carry);
            let had_carry = !frame.is_empty();
            if had_carry {
                let take = (stride - frame.len()).min(data.len());
                frame.extend_from_slice(&data[..take]);
                data = &data[take..];
            }
            let complete = frame.len() == stride;
            let whole = data.len() - (data.len() % stride);
            let head: &[u8] = if complete { &frame } else { &[] };
            let r = self.emit_gained(ctx, head, &data[..whole], buf.pts);
            self.carry = frame;
            if complete || !had_carry {
                self.carry.clear();
                self.carry.extend_from_slice(&data[whole..]);
            }
            r?;
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::PropChanged { name: "gain", value } => {
                match Self::gain_from_value(ctx, *value).filter(|g| g.is_finite()) {
                    Some(g) => {
                        self.set_gain(g);
                        log!(&*ctx, Level::Debug, "gain", milli = (self.gain * 1000.0) as i64);
                    }
                    None => log!(&*ctx, Level::Warn, "prop_rejected", prop = "gain"),
                }
            }
            Event::PropChanged { name: "mute", value } => {
                match Self::mute_from_value(ctx, *value) {
                    Some(m) => {
                        self.set_mute(m);
                        log!(&*ctx, Level::Debug, "mute", on = m);
                    }
                    None => log!(&*ctx, Level::Warn, "prop_rejected", prop = "mute"),
                }
            }
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", carry = self.carry.len());
                self.carry.clear();
                self.snap();
            }
            Event::Eos => {
                if !self.carry.is_empty() {
                    log!(&*ctx, Level::Warn, "eos_partial_frame", bytes = self.carry.len());
                }
                self.carry.clear();
            }
            Event::FormatChange(_) => {
                log!(&*ctx, Level::Debug, "format_change", fed = self.fed);
                if !self.fed {
                    self.unlearn();
                }
                self.learn_from_sink(ctx);
                self.warn_if_relatched(ctx);
            }
            _ => {}
        }
        if self.input.is_none() {
            self.learn_from_sink(ctx);
        }
        Ok(())
    }

    // Cold path, teardown.
    #[allow(clippy::disallowed_methods)]
    fn stop(&mut self, _ctx: &mut Ctx) {
        self.carry = Vec::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_is_clamped_and_non_finite_requests_are_ignored() {
        assert_eq!(AudioGain::new(0.5).gain(), 0.5);
        assert_eq!(AudioGain::new(-1.0).gain(), GAIN_MIN);
        assert_eq!(AudioGain::new(99.0).gain(), GAIN_MAX);
        assert_eq!(AudioGain::new(f32::NAN).gain(), 1.0);
        assert_eq!(AudioGain::new(f32::INFINITY).gain(), 1.0);
    }

    #[test]
    fn a_fresh_element_starts_at_its_configured_gain() {
        let g = AudioGain::new(0.25);
        assert_eq!(g.applied_gain(), 0.25);
        assert!(!g.is_unity());
        assert!(AudioGain::new(1.0).is_unity(), "unity is the copy-free fast path");
    }

    #[test]
    fn integer_scaling_saturates_instead_of_wrapping() {
        assert_eq!(saturate(i16::MAX as i32, 4.0, i16::MIN as i32, i16::MAX as i32), 32767);
        assert_eq!(saturate(i16::MIN as i32, 4.0, i16::MIN as i32, i16::MAX as i32), -32768);
        assert_eq!(saturate(i32::MAX, 4.0, i32::MIN, i32::MAX), i32::MAX);
        assert_eq!(saturate(i32::MIN, 2.0, i32::MIN, i32::MAX), i32::MIN);
        assert_eq!(saturate(1000, 0.5, i16::MIN as i32, i16::MAX as i32), 500);
        assert_eq!(saturate(1001, 0.5, i16::MIN as i32, i16::MAX as i32), 501); // round half away
    }

    #[test]
    fn s24_scaling_round_trips_through_the_three_byte_layout() {
        let mut s = [0xFFu8, 0xFF, 0xFF];
        scale_sample(SampleFormat::S24, 0.5, &mut s);
        let raw = s[0] as i32 | (s[1] as i32) << 8 | (s[2] as i32) << 16;
        assert_eq!((raw << 8) >> 8, -1, "round-half-away keeps -0.5 at -1");
        let mut s = [0xFFu8, 0xFF, 0x7F]; // 8388607
        scale_sample(SampleFormat::S24, 4.0, &mut s);
        assert_eq!(s, [0xFF, 0xFF, 0x7F]);
    }

    #[test]
    fn u8_scaling_respects_the_128_bias() {
        let mut s = [128u8];
        scale_sample(SampleFormat::U8, 0.5, &mut s);
        assert_eq!(s, [128]);
        let mut s = [255u8];
        scale_sample(SampleFormat::U8, 4.0, &mut s);
        assert_eq!(s, [255]);
    }

    #[test]
    fn ramp_lands_exactly_on_the_target() {
        let mut g = AudioGain::with_input(AudioFormat::new(48_000, 2, SampleFormat::F32), 1.0);
        g.set_gain(0.5);
        let frames = g.ramp_frames();
        assert_eq!(frames, 240, "5 ms at 48 kHz");
        for _ in 0..frames {
            g.advance();
        }
        assert_eq!(g.applied_gain(), 0.5, "the last step snaps exactly onto the target");
        assert_eq!(g.ramp_remaining, 0);
    }

    #[test]
    fn copy_concat_reads_the_two_slices_as_one_stream() {
        let (head, body) = ([1u8, 2, 3, 4], [5u8, 6, 7, 8, 9, 10, 11, 12]);
        let mut dst = [0u8; 6];
        copy_concat(&head, &body, 0, &mut dst);
        assert_eq!(dst, [1, 2, 3, 4, 5, 6]);
        copy_concat(&head, &body, 6, &mut dst);
        assert_eq!(dst, [7, 8, 9, 10, 11, 12]);
        let mut dst = [0u8; 4];
        copy_concat(&head, &body, 4, &mut dst);
        assert_eq!(dst, [5, 6, 7, 8]);
    }
}
