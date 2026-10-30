//! `audioresample`, the element over the [`crate::resample`] polyphase resampler.

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

use crate::convert::convert_interleaved_vec;
use crate::error::{AudioErrorKind as K, failed};
use crate::format::{
    AudioFormat, FAMILY, FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE, MAX_SAMPLE_RATE, SampleFormat,
    announce_raw, negotiated_audio_format,
};
use crate::layout::negotiated_layout;
use crate::resample::{ChannelResampler, PolyphaseFilter};
use crate::stamp::push_pcm;
use crate::trim::{position_at, time_at};

const HALF_TAPS: usize = 32;
const KAISER_BETA: f64 = 9.0;

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
    offers: &OFFERS, // output rate is announced at runtime
}];

static PROPS: [PropDesc; 1] = [PropDesc { name: "rate", allowed: Constraint::Any, live: false }];

// Cold path, once at construction.
#[allow(clippy::disallowed_methods)]
pub static DESC: ElementDesc = ElementDesc {
    name: "audioresample",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(AudioResample::auto())),
    io: false,
    couplings: &[
        Coupling::same(0, 0, FIELD_CHANNELS),
        Coupling::same(0, 0, FIELD_SAMPLE),
        Coupling::prop(0, FIELD_RATE, "rate"),
    ],
};

struct Engine {
    channels: Vec<ChannelResampler>,
    sample: SampleFormat,
    nch: usize,
}

impl Engine {
    fn reset(&mut self) {
        for ch in &mut self.channels {
            ch.reset();
        }
    }

    fn output_start(&self, pts: Timestamp, in_rate: u32) -> Option<i128> {
        let first = self.channels.first()?;
        let (l, m) = (first.filter().l as i128, first.filter().m as i128);
        pts.nanos()?;
        let n = i128::from(position_at(pts, in_rate));
        Some((2 * n * l + i128::from(first.lead()) + m).div_euclid(2 * m))
    }
}

/// Resamples interleaved PCM to a fixed target rate, preserving channels and sample format.
pub struct AudioResample {
    target: Option<u32>,
    input: Option<AudioFormat>,
    fed: bool,
    layout: Option<u64>,
    refused: Option<AudioFormat>,
    engine: Option<Engine>,
    in_stride: usize,
    carry: Vec<u8>,
    partial_in: Option<(u32, u16)>,
    warned_relatch: Option<AudioFormat>,
    preset: [(&'static str, ValueDesc); 1],
    presets: usize,
}

impl AudioResample {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    /// A resampler to `target_rate` Hz, its input format discovered from upstream caps.
    // Cold path, once at construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new(target_rate: u32) -> Self {
        assert!(target_rate > 0, "audioresample: target rate must be positive");
        let mut r = Self::auto();
        r.target = Some(target_rate);
        (r.preset, r.presets) = ([("rate", ValueDesc::Int(target_rate as i64))], 1);
        r
    }

    /// A resampler whose target the solve decides. The input's own rate passes audio through.
    // Cold path, once at construction.
    #[allow(clippy::disallowed_methods)]
    pub fn auto() -> Self {
        Self {
            target: None,
            input: None,
            layout: None,
            refused: None,
            engine: None,
            in_stride: 0,
            carry: Vec::new(),
            partial_in: None,
            warned_relatch: None,
            fed: false,
            preset: [("rate", ValueDesc::Int(0))],
            presets: 0,
        }
    }

    /// A resampler with the input format pinned at construction.
    pub fn with_input(input: AudioFormat, target_rate: u32) -> Self {
        let mut r = Self::new(target_rate);
        r.set_input(input);
        r
    }

    /// The target output sample rate, `None` while nothing has said.
    pub fn target_rate(&self) -> Option<u32> {
        self.target
    }

    fn rate(&self) -> u32 {
        self.target.unwrap_or(0)
    }

    /// The input format, once known.
    pub fn input_format(&self) -> Option<AudioFormat> {
        self.input
    }

    /// The output format, the input's with `sample_rate` set to the target.
    pub fn output_format(&self) -> Option<AudioFormat> {
        if let Some(f) = self.input {
            return Some(AudioFormat::new(self.rate(), f.channels, f.format));
        }
        let target = self.target?;
        self.partial_in.map(|(_, ch)| AudioFormat::new(target, ch, SampleFormat::F32))
    }

    fn set_input(&mut self, input: AudioFormat) {
        if self.input == Some(input) {
            return;
        }
        if !input.within_dsp_limits() {
            self.refused = Some(input);
            return;
        }
        self.in_stride = input.frame_stride();
        self.input = Some(input);
        self.carry.clear();
        let target = *self.target.get_or_insert(input.sample_rate);

        if input.sample_rate == target {
            self.engine = None;
            return;
        }
        let filter = PolyphaseFilter::design(input.sample_rate, target, HALF_TAPS, KAISER_BETA);
        let nch = input.channels as usize;
        let channels = (0..nch).map(|_| ChannelResampler::new(filter.clone())).collect();
        self.engine = Some(Engine { channels, sample: input.format, nch });
    }

    fn unlearn(&mut self) {
        self.input = None;
        self.engine = None;
        self.refused = None;
        self.partial_in = None;
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

        match sample {
            Some(fmt) => {
                let input = AudioFormat::new(rate as u32, channels as u16, fmt);
                self.set_input(input);
                if self.refused == Some(input) {
                    log!(&*ctx, Level::Warn, "format_refused", rate = rate, channels = channels);
                }
                self.layout = negotiated_layout(ctx, Self::IN);
                log!(
                    &*ctx,
                    Level::Info,
                    "format",
                    in_rate = rate,
                    out_rate = self.rate(),
                    channels = channels,
                    sample = fmt.caps_name()
                );
                self.announce_output(ctx);
            }
            None => self.partial_in = Some((rate as u32, channels as u16)),
        }
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

    fn announce_output(&self, ctx: &mut Ctx) {
        let Some(inp) = self.input else { return };
        announce_raw(ctx, Self::OUT, AudioFormat { sample_rate: self.rate(), ..inp }, self.layout);
    }

    fn emit_resampled(&mut self, ctx: &mut Ctx, bytes: &[u8], pts: Timestamp) -> Result<(), Error> {
        if bytes.is_empty() {
            return Ok(());
        }
        let in_rate = self.input.map_or(0, |f| f.sample_rate);
        let Some(engine) = self.engine.as_mut() else {
            return self.push(ctx, bytes, in_rate, pts);
        };
        let nch = engine.nch;
        let start = engine.output_start(pts, in_rate);
        let inter = convert_interleaved_vec(engine.sample, SampleFormat::F32, nch, bytes)
            .ok_or_else(|| failed(ctx, DESC.name, K::RaggedFrame { len: bytes.len() }))?;
        let frames = inter.len() / (4 * nch); // f32 == 4 bytes

        let pcm = {
            let arena = ctx.scratch();

            let mut planar_out: Vec<Vec<f32, &_>, &_> = Vec::with_capacity_in(nch, arena);
            let mut max_out = 0usize;
            for ch in 0..nch {
                let mut lane: Vec<f32, &_> = Vec::with_capacity_in(frames, arena);
                for f in 0..frames {
                    lane.push(f32::from_le_bytes([
                        inter[(f * nch + ch) * 4],
                        inter[(f * nch + ch) * 4 + 1],
                        inter[(f * nch + ch) * 4 + 2],
                        inter[(f * nch + ch) * 4 + 3],
                    ]));
                }
                let mut out: Vec<f32, &_> = Vec::new_in(arena);
                engine.channels[ch].process(&lane, &mut out);
                max_out = max_out.max(out.len());
                planar_out.push(out);
            }
            debug_assert!(
                planar_out.iter().all(|c| c.len() == max_out),
                "channel output counts diverged"
            );
            if max_out == 0 {
                return Ok(());
            }

            let mut inter_out: Vec<u8, &_> = Vec::with_capacity_in(max_out * nch * 4, arena);
            inter_out.resize(max_out * nch * 4, 0u8);
            for f in 0..max_out {
                for (ch, lane) in planar_out.iter().enumerate() {
                    let v = lane.get(f).copied().unwrap_or(0.0);
                    let off = (f * nch + ch) * 4;
                    inter_out[off..off + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
            convert_interleaved_vec(SampleFormat::F32, engine.sample, nch, &inter_out).ok_or_else(
                || {
                    let kind = K::Convert { from: SampleFormat::F32, to: engine.sample };
                    failed(ctx, DESC.name, kind)
                },
            )?
        };
        self.push_from(ctx, &pcm, start)
    }

    fn push_from(&self, ctx: &mut Ctx, pcm: &[u8], start: Option<i128>) -> Result<(), Error> {
        let before_zero = start.map_or(0, |at| usize::try_from(-at).unwrap_or(0));
        let cut = before_zero.saturating_mul(self.in_stride).min(pcm.len());
        if cut > 0 {
            log!(&*ctx, Level::Debug, "cut_before_zero", frames = before_zero);
        }
        let on_grid = |at: i128| time_at(u64::try_from(at.max(0)).unwrap_or(u64::MAX), self.rate());
        self.push(ctx, &pcm[cut..], self.rate(), start.map_or(Timestamp::NONE, on_grid))
    }

    fn push(&self, ctx: &mut Ctx, pcm: &[u8], rate: u32, pts: Timestamp) -> Result<(), Error> {
        push_pcm(ctx, Self::OUT, pcm, (self.in_stride, rate), pts)
            .map_err(|k| failed(ctx, DESC.name, k))
    }
}

impl Element for AudioResample {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn presets(&self) -> &[(&'static str, ValueDesc)] {
        &self.preset[..self.presets]
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let rate = |v: Option<Value>| match v {
            Some(Value::Int(r)) if r > 0 && r <= MAX_SAMPLE_RATE as i64 => Some(r as u32),
            _ => None,
        };
        let solved = ctx.field_id(FIELD_RATE).and_then(|id| ctx.negotiated_src(Self::OUT)?.get(id));
        let target = rate(ctx.prop("rate")).or(self.target).or(rate(solved));
        if target != self.target {
            log!(&*ctx, Level::Debug, "target", rate = target.unwrap_or(0));
            self.target = target;
            if let Some(input) = self.input.take() {
                self.set_input(input);
            }
        }
        self.announce_output(ctx);
        self.fed = false;
        self.learn_from_sink(ctx);
        self.carry.clear();
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.input.is_none() {
            self.learn_from_sink(ctx);
        }

        while let Some(buf) = crate::stamp::take_pcm(ctx, &mut inputs) {
            self.fed = true;
            if self.input.is_none() {
                let kind = match self.refused {
                    Some(format) => K::UnsupportedFormat { format },
                    None => K::PcmBeforeFormat,
                };
                return Err(failed(ctx, DESC.name, kind));
            }
            let data = buf.memory.data();
            if self.carry.is_empty() {
                let whole = data.len() - (data.len() % self.in_stride);
                self.emit_resampled(ctx, &data[..whole], buf.pts)?;
                self.carry.extend_from_slice(&data[whole..]);
            } else {
                let mut joined = std::mem::take(&mut self.carry);
                joined.extend_from_slice(data);
                let whole = joined.len() - (joined.len() % self.in_stride);
                self.emit_resampled(ctx, &joined[..whole], buf.pts)?;
                self.carry.clear();
                self.carry.extend_from_slice(&joined[whole..]);
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            // Drop the partial frame and the FIR delay lines, else post-seek output blends pre-seek
            // audio.
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", carry = self.carry.len());
                self.carry.clear();
                if let Some(engine) = self.engine.as_mut() {
                    engine.reset();
                }
            }
            Event::Eos => {
                let first = self.engine.as_ref().and_then(|e| e.channels.first());
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    consumed = first.map_or(0, |c| c.consumed()),
                    produced = first.map_or(0, |c| c.produced()),
                    carry = self.carry.len()
                );
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

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.carry.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_format_tracks_target_rate_and_input_shape() {
        let r = AudioResample::with_input(AudioFormat::new(44_100, 2, SampleFormat::S16), 48_000);
        assert_eq!(r.target_rate(), Some(48_000));
        assert_eq!(r.input_format(), Some(AudioFormat::new(44_100, 2, SampleFormat::S16)));
        assert_eq!(
            r.output_format(),
            Some(AudioFormat::new(48_000, 2, SampleFormat::S16)),
            "output keeps channels + sample format, only the rate changes"
        );
    }

    #[test]
    fn equal_rate_is_pass_through_engine_none() {
        let r = AudioResample::with_input(AudioFormat::new(48_000, 1, SampleFormat::S16), 48_000);
        assert!(r.engine.is_none(), "matching rate needs no resampling engine");
    }
}
