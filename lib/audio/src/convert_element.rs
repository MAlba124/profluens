//! `audioconvert`, the element over [`crate::convert`].

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

use crate::convert::{convert_interleaved, converted_len};
use crate::error::{AudioErrorKind as K, failed};
use crate::format::{
    AudioFormat, FAMILY, FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE, SampleFormat, announce_raw,
    negotiated_audio_format,
};
use crate::layout::negotiated_layout;
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

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc {
    name: "src",
    offers: &OFFERS, // output sample format is announced at runtime
}];

static PROPS: [PropDesc; 1] = [PropDesc { name: "format", allowed: Constraint::Any, live: false }];

// Cold path, once at construction.
#[allow(clippy::disallowed_methods)]
pub static DESC: ElementDesc = ElementDesc {
    name: "audioconvert",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(AudioConvert::auto())),
    io: false,
    couplings: &[
        Coupling::same(0, 0, FIELD_RATE),
        Coupling::same(0, 0, FIELD_CHANNELS),
        Coupling::prop(0, FIELD_SAMPLE, "format"),
    ],
};

/// Converts interleaved PCM to a fixed target [`SampleFormat`], preserving rate and channels.
pub struct AudioConvert {
    target: Option<SampleFormat>,
    input: Option<AudioFormat>,
    fed: bool,
    layout: Option<u64>,
    in_stride: usize,
    carry: Vec<u8>,
    scratch: Vec<u8>,
    partial_in: Option<(u32, u16)>,
    warned_relatch: Option<AudioFormat>,
    preset: [(&'static str, ValueDesc); 1],
    presets: usize,
}

impl AudioConvert {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    /// A converter to `target`, its input format discovered from upstream caps.
    // Cold path, once at construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new(target: SampleFormat) -> Self {
        let mut c = Self::auto();
        c.target = Some(target);
        (c.preset, c.presets) = ([("format", ValueDesc::Id(target.caps_name()))], 1);
        c
    }

    /// A converter whose target the solve decides, the `format` prop, the src edge's demand or the
    /// input's.
    // Cold path, once at construction.
    #[allow(clippy::disallowed_methods)]
    pub fn auto() -> Self {
        Self {
            target: None,
            input: None,
            layout: None,
            in_stride: 0,
            carry: Vec::new(),
            scratch: Vec::new(),
            partial_in: None,
            warned_relatch: None,
            fed: false,
            preset: [("format", ValueDesc::Id("f32"))],
            presets: 0,
        }
    }

    /// A converter with the input format pinned at construction.
    pub fn with_input(input: AudioFormat, target: SampleFormat) -> Self {
        let mut c = Self::new(target);
        c.set_input(input);
        c
    }

    /// The target output sample format, `None` while the solve or the input has not said.
    pub fn target(&self) -> Option<SampleFormat> {
        self.target
    }

    fn out_sample(&self) -> Option<SampleFormat> {
        self.target.or(self.input.map(|f| f.format))
    }

    /// The input format, once known.
    pub fn input_format(&self) -> Option<AudioFormat> {
        self.input
    }

    /// The output format, the input's rate and channels in the target format.
    pub fn output_format(&self) -> Option<AudioFormat> {
        if let Some(f) = self.input {
            return Some(AudioFormat::new(
                f.sample_rate,
                f.channels,
                self.target.unwrap_or(f.format),
            ));
        }
        let target = self.target?;
        self.partial_in.map(|(rate, ch)| AudioFormat::new(rate, ch, target))
    }

    fn set_input(&mut self, input: AudioFormat) {
        if self.input == Some(input) {
            return;
        }
        self.in_stride = input.frame_stride();
        self.input = Some(input);
        self.carry.clear();
    }

    fn announce_output(&self, ctx: &mut Ctx) {
        let Some(out) = self.output_format() else {
            return;
        };
        announce_raw(ctx, Self::OUT, out, self.layout);
    }

    fn emit_converted(&mut self, ctx: &mut Ctx, bytes: &[u8], pts: Timestamp) -> Result<(), Error> {
        let (Some(inp), Some(target)) = (self.input, self.out_sample()) else {
            return Err(failed(ctx, DESC.name, K::NoInputFormat));
        };
        let channels = inp.channels as usize;
        let grid = (channels * target.bytes(), inp.sample_rate);

        if inp.format == target {
            return push_pcm(ctx, Self::OUT, bytes, grid, pts)
                .map_err(|k| failed(ctx, DESC.name, k));
        }

        let out_len = converted_len(inp.format, target, channels, bytes.len())
            .ok_or_else(|| failed(ctx, DESC.name, K::RaggedFrame { len: bytes.len() }))?;
        self.scratch.clear();
        self.scratch.resize(out_len, 0);
        convert_interleaved(inp.format, target, channels, bytes, &mut self.scratch)
            .ok_or_else(|| failed(ctx, DESC.name, K::Convert { from: inp.format, to: target }))?;

        let out = std::mem::take(&mut self.scratch);
        let r = push_pcm(ctx, Self::OUT, &out, grid, pts);
        self.scratch = out;
        self.scratch.clear();
        r.map_err(|k| failed(ctx, DESC.name, k))
    }
}

impl Element for AudioConvert {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn presets(&self) -> &[(&'static str, ValueDesc)] {
        &self.preset[..self.presets]
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let named = |v: Option<Value>| match v {
            Some(Value::Id(id)) => ctx.value_name(id).and_then(SampleFormat::from_caps_name),
            _ => None,
        };
        let solved =
            ctx.field_id(FIELD_SAMPLE).and_then(|id| ctx.negotiated_src(Self::OUT)?.get(id));
        self.target = named(ctx.prop("format")).or(self.target).or(named(solved));
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
                return Err(failed(ctx, DESC.name, K::PcmBeforeFormat));
            }
            if self.in_stride == 0 {
                return Err(failed(ctx, DESC.name, K::InputStrideUnknown));
            }
            let len = buf.memory.len();
            if let Some(inp) = self.input.filter(|inp| Some(inp.format) == self.out_sample())
                && self.carry.is_empty()
                && len > 0
                && len % self.in_stride == 0
            {
                forward_pcm(ctx, Self::OUT, buf, (self.in_stride, inp.sample_rate));
                continue;
            }
            let data = buf.memory.data();

            if self.carry.is_empty() {
                let whole = data.len() - (data.len() % self.in_stride);
                self.emit_converted(ctx, &data[..whole], buf.pts)?;
                self.carry.extend_from_slice(&data[whole..]);
            } else {
                let mut joined = std::mem::take(&mut self.carry);
                joined.extend_from_slice(data);
                let whole = joined.len() - (joined.len() % self.in_stride);
                self.emit_converted(ctx, &joined[..whole], buf.pts)?;
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
            Event::FormatChange(_) => {
                log!(&*ctx, Level::Debug, "format_change", fed = self.fed);
                if !self.fed {
                    self.unlearn();
                }
                self.learn_from_sink(ctx);
                self.warn_if_relatched(ctx);
            }
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
    }
}

impl AudioConvert {
    fn unlearn(&mut self) {
        self.input = None;
        self.partial_in = None;
        self.in_stride = 0;
    }

    fn learn_from_sink(&mut self, ctx: &mut Ctx) {
        if self.input.is_some() {
            return; // pinned or already learned
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

        let partial = Some((rate as u32, channels as u16));
        match sample {
            Some(fmt) => self.set_input(AudioFormat::new(rate as u32, channels as u16, fmt)),
            None if self.partial_in == partial => return,
            None => self.partial_in = partial,
        }
        self.layout = negotiated_layout(ctx, Self::IN);
        log!(
            &*ctx,
            Level::Info,
            "format",
            rate = rate,
            channels = channels,
            sample = sample.map_or("unknown", SampleFormat::caps_name),
            out = self.out_sample().map_or("unknown", SampleFormat::caps_name)
        );
        self.announce_output(ctx);
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
}
