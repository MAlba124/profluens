//! `audiodownmix`, folds multichannel `audio/raw` to stereo, or to mono when asked.

use profluens_core::batch::Inputs;
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::convert::{
    downmix_clip_gain, downmix_layout_to_stereo, downmix_layout_to_stereo_scaled,
    fold_stereo_to_mono,
};
use crate::error::{AudioErrorKind as K, failed};
use crate::format::{
    AudioFormat, FAMILY, FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE, SampleFormat, announce_raw,
    negotiated_audio_format,
};
use crate::layout::{FC, LAYOUT_FIELD, STEREO, layout_or_default, negotiated_layout};
use crate::stamp::{forward_pcm, push_pcm};

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
static SRC_FIELDS: [FieldDesc; 4] = [
    FieldDesc { field: FIELD_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: FIELD_CHANNELS,
        allowed: ConstraintDesc::Set(&[ValueDesc::Int(1), ValueDesc::Int(2)]),
        preferred: Some(ValueDesc::Int(2)),
    },
    FieldDesc {
        field: FIELD_SAMPLE,
        allowed: ConstraintDesc::Set(&SAMPLE_VALUES),
        preferred: None,
    },
    LAYOUT_FIELD,
];
static SRC_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: FAMILY, fields: &SRC_FIELDS }, OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
// Asked for mono the pad offers one channel only, so a link fixed before the input is known
// (a build's preroll) is mono too. A stereo preference there built the encoder behind it for
// two channels and it read the mono frames as pairs, at twice the speed.
static MONO_SRC_FIELDS: [FieldDesc; 4] = [
    FieldDesc { field: FIELD_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: FIELD_CHANNELS,
        allowed: ConstraintDesc::Set(&[ValueDesc::Int(1)]),
        preferred: Some(ValueDesc::Int(1)),
    },
    FieldDesc {
        field: FIELD_SAMPLE,
        allowed: ConstraintDesc::Set(&SAMPLE_VALUES),
        preferred: None,
    },
    LAYOUT_FIELD,
];
static MONO_SRC_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: FAMILY, fields: &MONO_SRC_FIELDS }, OfferDesc::any("bytes")];
static MONO_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &MONO_SRC_OFFERS }];

static PROPS: [PropDesc; 0] = [];

// Cold path, once at construction.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "audiodownmix",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(AudioDownmix::new())),
    io: false,
    couplings: &[Coupling::same(0, 0, FIELD_RATE), Coupling::same(0, 0, FIELD_SAMPLE)],
};

#[allow(clippy::disallowed_methods)]
static DESC_MONO: ElementDesc = ElementDesc {
    name: "audiodownmix",
    sinks: &SINKS,
    srcs: &MONO_SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(AudioDownmix::new().to_mono())),
    io: false,
    couplings: &[Coupling::same(0, 0, FIELD_RATE), Coupling::same(0, 0, FIELD_SAMPLE)],
};

/// Folds interleaved multichannel PCM to stereo, preserving rate and sample format. The channels
/// out are the ones the src link fixed, so a mono input to a link solved for two goes out on both
/// sides. With no src link fixed a mono input passes as it is.
#[derive(Default)]
pub struct AudioDownmix {
    input: Option<(u32, u16, SampleFormat)>,
    layout: Option<u64>,
    in_stride: usize,
    carry: Vec<u8>,
    scratch: Vec<u8>,
    /// The fold scaled so a full scale input cannot clip, else clamped.
    normalized: bool,
    /// One channel out, the mean of the stereo fold.
    mono: bool,
    mono_buf: Vec<u8>,
    /// Channels out, picked by `fix_out`.
    out: u16,
}

impl AudioDownmix {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self::default()
    }

    /// Scales the fold by [`downmix_clip_gain`], a full scale input stays within full scale.
    pub fn normalized(mut self) -> Self {
        self.normalized = true;
        self
    }

    /// One channel out, the mean of the two the fold makes, a mono input passes.
    pub fn to_mono(mut self) -> Self {
        self.mono = true;
        self
    }

    fn out_channels(&self) -> u16 {
        self.out
    }

    /// Channels out, the src link's when it is fixed, else the input's up to two.
    fn fix_out(&mut self, ctx: &Ctx) {
        let fixed = ctx
            .negotiated_src(Self::OUT)
            .zip(ctx.field_id(FIELD_CHANNELS))
            .and_then(|(f, id)| f.get(id));
        self.out = match (self.mono, fixed, self.input) {
            (true, ..) => 1,
            (false, Some(Value::Int(n @ 1..=2)), _) => n as u16,
            (false, _, Some((_, 1, _))) => 1,
            _ => 2,
        };
    }

    fn set_input(&mut self, rate: u32, channels: u16, sample: SampleFormat) {
        if self.input == Some((rate, channels, sample)) {
            return;
        }
        self.in_stride = channels as usize * sample.bytes();
        self.input = Some((rate, channels, sample));
        self.carry.clear();
    }

    fn unlearn(&mut self) {
        self.input = None;
        self.in_stride = 0;
    }

    fn learn_from_sink(&mut self, ctx: &mut Ctx) {
        if self.input.is_some() {
            return;
        }
        let Some(fixed) = ctx.negotiated_sink(Self::IN) else {
            return;
        };
        let int_field = |name: &str| -> Option<i64> {
            ctx.field_id(name).and_then(|id| fixed.get(id)).and_then(|v| match v {
                Value::Int(n) => Some(n),
                _ => None,
            })
        };
        let (Some(rate), Some(channels)) = (int_field(FIELD_RATE), int_field(FIELD_CHANNELS))
        else {
            return;
        };
        // Bounds too, an `i64` past `u16` or `u32` must not wrap into a plausible format.
        if rate <= 0 || channels <= 0 || rate > u32::MAX as i64 || channels > u16::MAX as i64 {
            log!(&*ctx, Level::Warn, "format_rejected", rate = rate, channels = channels);
            return;
        }
        let sample = ctx
            .field_id(FIELD_SAMPLE)
            .and_then(|id| fixed.get(id))
            .and_then(|v| match v {
                Value::Id(vid) => ctx.value_name(vid),
                _ => None,
            })
            .and_then(SampleFormat::from_caps_name);
        if let Some(fmt) = sample {
            self.set_input(rate as u32, channels as u16, fmt);
            self.layout = negotiated_layout(ctx, Self::IN);
            self.fix_out(ctx);
            log!(
                &*ctx,
                Level::Info,
                "format",
                rate = rate,
                channels = channels,
                sample = fmt.caps_name(),
                layout = self.layout.unwrap_or(0)
            );
            self.announce_output(ctx);
        }
    }

    /// A format change between buffers. A new input format or layout drops the old one's partial
    /// frame and is learned again, so the fold follows a 5.1 to 2.0 switch mid stream.
    fn relearn(&mut self, ctx: &mut Ctx) {
        let Some(now) = negotiated_audio_format(ctx, Self::IN) else {
            return;
        };
        let latched =
            self.input.map(|(rate, channels, sample)| AudioFormat::new(rate, channels, sample));
        let layout = negotiated_layout(ctx, Self::IN);
        if latched == Some(now) && layout == self.layout {
            return;
        }
        if !self.carry.is_empty() {
            log!(&*ctx, Level::Warn, "format_change_partial_frame", bytes = self.carry.len());
        }
        self.carry.clear();
        self.unlearn();
        self.learn_from_sink(ctx);
    }

    fn announce_output(&self, ctx: &mut Ctx) {
        let Some((rate, channels, fmt)) = self.input else {
            return;
        };
        let out = self.out_channels();
        let layout = match (out, channels) {
            (1, 1) | (2, 2) => self.layout,
            (1, _) => Some(FC),
            _ => Some(STEREO),
        };
        announce_raw(ctx, Self::OUT, AudioFormat::new(rate, out, fmt), layout);
    }

    fn emit(&mut self, ctx: &mut Ctx, bytes: &[u8], pts: Timestamp) -> Result<(), Error> {
        let Some((rate, channels, fmt)) = self.input else {
            return Err(failed(ctx, DESC.name, K::NoInputFormat));
        };
        let fold = channels > 2;
        if channels == self.out_channels() {
            return push_pcm(ctx, Self::OUT, bytes, (self.in_stride, rate), pts)
                .map_err(|k| failed(ctx, DESC.name, k));
        }
        let bps = fmt.bytes();
        let frames = bytes.len() / self.in_stride;
        let mut stereo = std::mem::take(&mut self.scratch);
        if channels == 1 {
            // Stereo out of a mono input, each sample on both sides
            stereo.clear();
            for s in bytes.chunks_exact(bps) {
                stereo.extend_from_slice(s);
                stereo.extend_from_slice(s);
            }
            let r = push_pcm(ctx, Self::OUT, &stereo, (2 * bps, rate), pts)
                .map_err(|k| failed(ctx, DESC.name, k));
            self.scratch = stereo;
            self.scratch.clear();
            return r;
        }
        if fold {
            stereo.clear();
            stereo.resize(frames * 2 * bps, 0);
            let layout = layout_or_default(self.layout, u32::from(channels));
            let n = channels as usize;
            let gain = downmix_clip_gain(layout, n);
            let folded = if self.normalized && gain < 1.0 {
                downmix_layout_to_stereo_scaled(fmt, layout, n, bytes, &mut stereo, gain)
            } else {
                downmix_layout_to_stereo(fmt, layout, n, bytes, &mut stereo)
            };
            if folded.is_none() {
                self.scratch = stereo;
                return Err(failed(ctx, DESC.name, K::RaggedFrame { len: bytes.len() }));
            }
        }
        let wide: &[u8] = if fold { &stereo } else { bytes };
        let r = if self.out == 1 {
            let mut mono = std::mem::take(&mut self.mono_buf);
            mono.clear();
            mono.resize(frames * bps, 0);
            let r = match fold_stereo_to_mono(fmt, wide, &mut mono) {
                Some(_) => push_pcm(ctx, Self::OUT, &mono, (bps, rate), pts)
                    .map_err(|k| failed(ctx, DESC.name, k)),
                None => Err(failed(ctx, DESC.name, K::RaggedFrame { len: bytes.len() })),
            };
            self.mono_buf = mono;
            r
        } else {
            push_pcm(ctx, Self::OUT, wide, (2 * bps, rate), pts)
                .map_err(|k| failed(ctx, DESC.name, k))
        };
        self.scratch = stereo;
        self.scratch.clear();
        r
    }
}

impl Element for AudioDownmix {
    fn desc(&self) -> &'static ElementDesc {
        if self.mono { &DESC_MONO } else { &DESC }
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.fix_out(ctx);
        self.announce_output(ctx);
        self.learn_from_sink(ctx);
        self.carry.clear();
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.input.is_none() {
            self.learn_from_sink(ctx);
        }

        while let Some(buf) = crate::stamp::take_pcm(ctx, &mut inputs) {
            if self.input.is_none() {
                return Err(failed(ctx, DESC.name, K::PcmBeforeFormat));
            }
            if self.in_stride == 0 {
                return Err(failed(ctx, DESC.name, K::InputStrideUnknown));
            }
            let len = buf.memory.len();
            // what passes as it is, every other channel count goes through `emit`
            if let Some((rate, channels, _)) = self.input
                && channels == self.out_channels()
                && self.carry.is_empty()
                && len > 0
                && len % self.in_stride == 0
            {
                forward_pcm(ctx, Self::OUT, buf, (self.in_stride, rate));
                continue;
            }
            let data = buf.memory.data();
            if self.carry.is_empty() {
                let whole = data.len() - (data.len() % self.in_stride);
                self.emit(ctx, &data[..whole], buf.pts)?;
                self.carry.extend_from_slice(&data[whole..]);
            } else {
                let mut joined = std::mem::take(&mut self.carry);
                joined.extend_from_slice(data);
                let whole = joined.len() - (joined.len() % self.in_stride);
                self.emit(ctx, &joined[..whole], buf.pts)?;
                self.carry.clear();
                self.carry.extend_from_slice(&joined[whole..]);
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            // A pre-seek partial frame would shift every later sample and rotate the channel lanes.
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", carry = self.carry.len());
                self.carry.clear();
            }
            Event::FormatChange(_) => self.relearn(ctx),
            Event::Eos if !self.carry.is_empty() => {
                log!(&*ctx, Level::Warn, "eos_partial_frame", bytes = self.carry.len());
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
        self.carry.clear();
        self.scratch = Vec::new();
        self.mono_buf = Vec::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels(d: &ElementDesc) -> &'static FieldDesc {
        d.srcs[0].offers[0].fields.iter().find(|f| f.field == FIELD_CHANNELS).unwrap()
    }

    #[test]
    fn a_mono_downmix_offers_one_channel_before_its_input_is_known() {
        let stereo = channels(AudioDownmix::new().desc());
        let mono = channels(AudioDownmix::new().to_mono().desc());
        assert!(matches!(stereo.preferred, Some(ValueDesc::Int(2))));
        assert!(matches!(mono.preferred, Some(ValueDesc::Int(1))));
        assert!(matches!(mono.allowed, ConstraintDesc::Set(&[ValueDesc::Int(1)])));
    }
}
