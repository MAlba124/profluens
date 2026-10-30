//! `flacenc`, a passive transform around [`FlacEncoder`]. Its STREAMINFO has unknown (0)
//! sizes and sample count, since a sequential sink cannot be back-patched.

use profluens_core::batch::Inputs;
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

use crate::encoder::{EncodeError, FlacEncoder, SampleFormat};
use crate::error::FlacError;

/// Within the streamable subset for 48 kHz and below (RFC 9639 section 7).
const BLOCK_SIZE: u32 = 4096;

const FAMILY: &str = "audio/raw";
const F_RATE: &str = "rate";
const F_CHANNELS: &str = "channels";
const F_SAMPLE: &str = "sample";

static SAMPLE_VALUES: [ValueDesc; 4] =
    [ValueDesc::Id("s8"), ValueDesc::Id("s16"), ValueDesc::Id("s24"), ValueDesc::Id("s32")];

static SINK_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: F_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: F_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: F_SAMPLE, allowed: ConstraintDesc::Set(&SAMPLE_VALUES), preferred: None },
];

/// Typed `audio/raw` first, then the `bytes` bridge. Order is preference.
static SINK_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: FAMILY, fields: &SINK_FIELDS }, OfferDesc::any("bytes")];
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

static PROPS: [PropDesc; 3] = [
    PropDesc { name: "rate", allowed: Constraint::Any, live: false },
    PropDesc { name: "channels", allowed: Constraint::Any, live: false },
    PropDesc { name: "format", allowed: Constraint::Any, live: false },
];

// Boxed once at registry or parse time.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "flacenc",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(FlacEnc::new(44_100, 2, SampleFormat::S16))),
    io: false,
    couplings: &[],
};

fn sample_format_from_name(s: &str) -> Option<SampleFormat> {
    Some(match s {
        "s8" => SampleFormat::S8,
        "s16" => SampleFormat::S16,
        "s24" => SampleFormat::S24,
        "s32" => SampleFormat::S32,
        _ => return None,
    })
}

pub struct FlacEnc {
    sample_rate: u32,
    channels: u32,
    format: SampleFormat,
    encoder: Option<FlacEncoder>,
    header: Vec<u8>,
    header_sent: bool,
    frame_stride: usize,
    carry: Vec<u8>,
    scratch_out: Vec<u8>,
}

impl FlacEnc {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    // One-time construction, the buffers are reused.
    #[allow(clippy::disallowed_methods)]
    pub fn new(sample_rate: u32, channels: u32, format: SampleFormat) -> Self {
        let frame_stride = channels as usize * format.bytes_per_sample();
        Self {
            sample_rate,
            channels,
            format,
            encoder: None,
            header: Vec::new(),
            header_sent: false,
            frame_stride,
            carry: Vec::new(),
            scratch_out: Vec::new(),
        }
    }

    /// Adopt the sink pad's `audio/raw` format. Absent fields keep their values. Returns whether
    /// anything changed.
    fn learn_from_caps(&mut self, ctx: &Ctx) -> bool {
        let Some(f) = ctx.negotiated_sink(Self::IN) else {
            return false;
        };
        if ctx.family_name(f.family) != Some(FAMILY) {
            return false; // the bytes bridge fixes no audio parameters
        }
        let mut changed = false;
        if let Some(Value::Int(r)) = ctx.field_id(F_RATE).and_then(|id| f.get(id))
            && r > 0
            && r as u32 != self.sample_rate
        {
            self.sample_rate = r as u32;
            changed = true;
        }
        if let Some(Value::Int(c)) = ctx.field_id(F_CHANNELS).and_then(|id| f.get(id))
            && c > 0
            && c as u32 != self.channels
        {
            self.channels = c as u32;
            changed = true;
        }
        if let Some(Value::Id(id)) = ctx.field_id(F_SAMPLE).and_then(|id| f.get(id))
            && let Some(sf) = ctx.value_name(id).and_then(sample_format_from_name)
            && sf != self.format
        {
            self.format = sf;
            changed = true;
        }
        if changed {
            self.frame_stride = self.channels as usize * self.format.bytes_per_sample();
        }
        changed
    }

    fn rebuild_encoder(&mut self, ctx: &Ctx) -> Result<(), EncodeError> {
        let (encoder, header) =
            FlacEncoder::new_streaming(self.sample_rate, self.channels, self.format, BLOCK_SIZE)?;
        self.encoder = Some(encoder);
        self.header = header;
        self.carry.clear();
        log!(
            ctx,
            Level::Info,
            "encoder",
            rate = self.sample_rate,
            channels = self.channels,
            bytes_per_sample = self.format.bytes_per_sample(),
            block = BLOCK_SIZE
        );
        Ok(())
    }

    fn emit(ctx: &mut Ctx, bytes: &[u8]) -> Result<(), Error> {
        let mut off = 0;
        while off < bytes.len() {
            let mut buf = ctx.alloc();
            let cap = buf.memory.capacity();
            debug_assert!(cap > 0, "flacenc: zero-capacity pool slot");
            let n = cap.min(bytes.len() - off);
            buf.memory.as_mut_full()[..n].copy_from_slice(&bytes[off..off + n]);
            buf.memory.set_len(n);
            ctx.out(Self::OUT).push(buf);
            off += n;
        }
        Ok(())
    }
}

fn enc_failed(ctx: &Ctx, e: EncodeError) -> Error {
    Error::element_failed(ctx.element(), FlacError::Encode(e))
}

impl Element for FlacEnc {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(Value::Int(r)) = ctx.prop("rate")
            && r > 0
        {
            self.sample_rate = r as u32;
        }
        if let Some(Value::Int(c)) = ctx.prop("channels")
            && c > 0
        {
            self.channels = c as u32;
        }
        if let Some(Value::Id(id)) = ctx.prop("format")
            && let Some(sf) = ctx.value_name(id).and_then(sample_format_from_name)
        {
            self.format = sf;
        }
        self.frame_stride = self.channels as usize * self.format.bytes_per_sample();
        self.learn_from_caps(ctx);

        // The header goes out on the first `process`, so an early `FormatChange` can rebuild.
        self.rebuild_encoder(ctx).map_err(|e| enc_failed(ctx, e))?;
        self.header_sent = false;
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            // Lazily, so an upstream format announcement can still set the parameters.
            if !self.header_sent {
                log!(&*ctx, Level::Debug, "header", bytes = self.header.len());
                Self::emit(ctx, &self.header)?;
                self.header_sent = true;
            }

            let data = buf.memory.data();
            let combined_len = self.carry.len() + data.len();
            let whole = combined_len - (combined_len % self.frame_stride);

            self.scratch_out.clear();
            let encoder = self.encoder.as_mut().ok_or_else(|| {
                Error::element_failed(ctx.element(), FlacError::EncoderNotStarted)
            })?;

            if self.carry.is_empty() {
                let split = data.len() - (data.len() % self.frame_stride);
                encoder
                    .encode_interleaved(&data[..split], &mut self.scratch_out)
                    .map_err(|e| enc_failed(ctx, e))?;
                self.carry.extend_from_slice(&data[split..]);
            } else {
                let mut joined = std::mem::take(&mut self.carry);
                joined.extend_from_slice(data);
                encoder
                    .encode_interleaved(&joined[..whole], &mut self.scratch_out)
                    .map_err(|e| enc_failed(ctx, e))?;
                self.carry.clear();
                self.carry.extend_from_slice(&joined[whole..]);
            }

            let out = std::mem::take(&mut self.scratch_out);
            Self::emit(ctx, &out)?;
            self.scratch_out = out; // reclaim the allocation
            self.scratch_out.clear();
        }

        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        // After the header went out FLAC cannot change stream parameters, so this fails.
        if matches!(event, Event::FormatChange(_)) && self.learn_from_caps(ctx) {
            if self.header_sent {
                return Err(Error::element_failed(
                    ctx.element(),
                    FlacError::FormatChanged {
                        rate: self.sample_rate,
                        channels: self.channels,
                        format: self.format,
                    },
                ));
            }
            log!(&*ctx, Level::Debug, "format_change", rebuild = true);
            self.rebuild_encoder(ctx).map_err(|e| enc_failed(ctx, e))?;
        }
        if matches!(event, Event::Eos)
            && self.header_sent
            && let Some(encoder) = self.encoder.as_mut()
        {
            self.scratch_out.clear();
            if !self.carry.is_empty() {
                log!(&*ctx, Level::Warn, "eos_partial_frame", bytes = self.carry.len());
            }
            // Streamed, the header already went out so the final STREAMINFO is not written back.
            let _ = encoder.finish(&mut self.scratch_out);
            log!(&*ctx, Level::Info, "eos", tail = self.scratch_out.len());
            let out = std::mem::take(&mut self.scratch_out);
            Self::emit(ctx, &out)?;
            self.scratch_out = out;
            self.scratch_out.clear();
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        // Carried bytes are a ragged final sample, dropped to keep the stream well-formed.
        self.encoder = None;
        self.carry.clear();
    }
}
