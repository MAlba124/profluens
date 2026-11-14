//! `libavaacenc`, libavcodec's native AAC-LC encoder. Interleaved PCM in, the
//! AudioSpecificConfig then one raw access unit per buffer out, stamped by sample count.
//! Unstamped input counts on from the end of the last packet put out, zero at the start.
//! Stamps run the encoder's priming ahead, declared as `delay` (as Matroska's `CodecDelay`), so
//! the priming packet goes out even at zero and a decoder cuts what falls before it.

use std::fmt;

use profluens_audio::FIELD_DELAY;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
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

use crate::lav::{Codec, Encoded, Encoder, LavError, Sent};

const F_RATE: &str = "rate";
const F_CHANNELS: &str = "channels";
const F_SAMPLE: &str = "sample";

/// ISO/IEC 14496-3 Table 1.18.
pub const AAC_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

const SLIP_NS: i64 = 5_000_000;

static RATE_VALUES: [ValueDesc; 13] = [
    ValueDesc::Int(96_000),
    ValueDesc::Int(88_200),
    ValueDesc::Int(64_000),
    ValueDesc::Int(48_000),
    ValueDesc::Int(44_100),
    ValueDesc::Int(32_000),
    ValueDesc::Int(24_000),
    ValueDesc::Int(22_050),
    ValueDesc::Int(16_000),
    ValueDesc::Int(12_000),
    ValueDesc::Int(11_025),
    ValueDesc::Int(8_000),
    ValueDesc::Int(7_350),
];
static SAMPLE_VALUES: [ValueDesc; 2] = [ValueDesc::Id("s16"), ValueDesc::Id("f32")];
static SINK_FIELDS: [FieldDesc; 3] = [
    FieldDesc {
        field: F_RATE,
        allowed: ConstraintDesc::Set(&RATE_VALUES),
        preferred: Some(ValueDesc::Int(48_000)),
    },
    // Any. A set would fix the link at its first value, mono, before upstream says its count.
    FieldDesc { field: F_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: F_SAMPLE,
        allowed: ConstraintDesc::Set(&SAMPLE_VALUES),
        preferred: Some(ValueDesc::Id("f32")),
    },
];
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &SINK_FIELDS }];
static SRC_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: F_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: F_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: FIELD_DELAY, allowed: ConstraintDesc::Any, preferred: None },
];
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "aac", fields: &SRC_FIELDS }];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

/// `bitrate` (0 for the encoder's own), `aligned` (frames on multiples of 1024), `fast`.
static PROPS: [PropDesc; 3] = [
    PropDesc { name: "bitrate", allowed: Constraint::Any, live: false },
    PropDesc { name: "aligned", allowed: Constraint::Any, live: false },
    PropDesc { name: "fast", allowed: Constraint::Any, live: false },
];

// One-time construction.
#[allow(clippy::disallowed_methods)]
pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "libavaacenc",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(LibavAacEnc::new())),
    io: false,
    couplings: &[],
};

#[derive(Debug, PartialEq, Eq)]
pub enum LavAacError {
    Open(LavError),
    InputFormatUnknown,
    Encode(LavError),
}

impl fmt::Display for LavAacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LavAacError::Open(e) => write!(f, "opening the AAC encoder failed, {e}"),
            LavAacError::InputFormatUnknown => f.write_str("audio arrived before its format"),
            LavAacError::Encode(e) => write!(f, "the AAC encoder refused a frame, {e}"),
        }
    }
}

impl std::error::Error for LavAacError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Input {
    rate: u32,
    channels: u8,
    float: bool,
}

pub struct LibavAacEnc {
    bitrate: u32,
    aligned: bool,
    fast: bool,
    input: Option<Input>,
    enc: Option<Encoder>,
    config: Vec<u8>,
    acc: Vec<f32>,
    origin: Option<i64>,
    sent: i64,
    /// The end of the last packet put out, in samples before the priming shift. `None` since
    /// start or flush.
    floor: Option<i64>,
    head_due: bool,
    /// Packets put out and PCM samples the encoder refused, for the end of stream summary.
    packets: u64,
    lost: u64,
}

impl Default for LibavAacEnc {
    fn default() -> Self {
        Self::new()
    }
}

fn sample_of(ns: i64, rate: u32) -> i64 {
    ((i128::from(ns) * i128::from(rate) + 500_000_000) / 1_000_000_000) as i64
}

fn ns_of(sample: i64, rate: u32) -> i64 {
    (i128::from(sample) * 1_000_000_000 / i128::from(rate.max(1))) as i64
}

impl LibavAacEnc {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    #[allow(clippy::disallowed_methods)] // one-time construction, the Vec is reused
    pub fn new() -> Self {
        LibavAacEnc {
            bitrate: 0,
            aligned: false,
            fast: false,
            input: None,
            enc: None,
            config: Vec::new(),
            acc: Vec::new(),
            origin: None,
            sent: 0,
            floor: None,
            head_due: true,
            packets: 0,
            lost: 0,
        }
    }

    pub fn with_bitrate(mut self, bps: u32) -> Self {
        self.bitrate = bps;
        self
    }

    /// Frames on absolute multiples of 1024 samples, so a mid-stream start cuts where a full run
    /// did.
    pub fn aligned(mut self) -> Self {
        self.aligned = true;
        self
    }

    /// The encoder's cheaper `fast` search over `twoloop`.
    pub fn fast(mut self) -> Self {
        self.fast = true;
        self
    }

    /// The AudioSpecificConfig this element puts out first for `rate` and `channels`.
    #[allow(clippy::disallowed_methods)] // one-time setup, a muxer's sample entry
    pub fn config_for(rate: u32, channels: u8) -> Result<Vec<u8>, LavError> {
        Encoder::open(Codec::Aac, rate, channels, 0, false).map(|e| e.config().to_vec())
    }

    fn read_input(ctx: &Ctx) -> Option<Input> {
        let fixed = ctx.negotiated_sink(Self::IN)?;
        let int = |name: &str| match ctx.field_id(name).and_then(|id| fixed.get(id)) {
            Some(Value::Int(v)) => Some(v),
            _ => None,
        };
        let rate = u32::try_from(int(F_RATE)?).ok().filter(|r| AAC_RATES.contains(r))?;
        let channels = u8::try_from(int(F_CHANNELS)?).ok().filter(|c| (1..=8).contains(c))?;
        let float = match ctx.field_id(F_SAMPLE).and_then(|id| fixed.get(id)) {
            Some(Value::Id(v)) => match ctx.value_name(v)? {
                "f32" => true,
                "s16" => false,
                _ => return None,
            },
            _ => return None,
        };
        Some(Input { rate, channels, float })
    }

    fn build(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let Some(input) = Self::read_input(ctx) else {
            return Ok(());
        };
        if self.input == Some(input) && self.enc.is_some() {
            return Ok(());
        }
        if let Some(old) = self.input.filter(|i| *i != input) {
            log!(&*ctx, Level::Debug, "input_change", rate = old.rate, channels = old.channels);
            self.finish(ctx)?;
        }
        self.input = Some(input);
        self.open(ctx)?;
        log!(
            &*ctx,
            Level::Info,
            "encoder_open",
            rate = input.rate,
            channels = input.channels,
            float = input.float,
            bitrate = self.bitrate,
        );
        let delay = self.enc.as_ref().map_or(0, |e| e.delay() as i64);
        let fields = [
            (F_RATE, ValueDesc::Int(i64::from(input.rate))),
            (F_CHANNELS, ValueDesc::Int(i64::from(input.channels))),
            (FIELD_DELAY, ValueDesc::Int(ns_of(delay, input.rate))),
        ];
        ctx.announce_format(Self::OUT, "aac", &fields);
        Ok(())
    }

    fn open(&mut self, ctx: &Ctx) -> Result<(), Error> {
        let Some(i) = self.input else { return Ok(()) };
        let enc = Encoder::open(Codec::Aac, i.rate, i.channels, self.bitrate, self.fast)
            .map_err(|e| Error::element_failed(ctx.element(), LavAacError::Open(e)))?;
        self.acc.clear();
        self.acc.reserve(enc.frame_size() * usize::from(i.channels));
        self.config.clear();
        self.config.extend_from_slice(enc.config());
        self.enc = Some(enc);
        self.origin = None;
        self.sent = 0;
        Ok(())
    }

    fn expected(&self) -> Option<i64> {
        let ch = self.input.map_or(1, |i| usize::from(i.channels));
        self.origin.map(|o| o + self.sent + (self.acc.len() / ch) as i64)
    }

    fn finish(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.enc.is_none() {
            return Ok(());
        }
        let tail = self.acc.len();
        if tail > 0 {
            let pcm = std::mem::take(&mut self.acc);
            let r = self.encode(ctx, &pcm);
            self.acc = pcm;
            self.acc.clear();
            r?;
        }
        log!(&*ctx, Level::Debug, "drain", tail = tail, sent = self.sent);
        // A frame still waiting in the encoder refuses the drain, receiving takes it in.
        self.emit(ctx)?;
        if let Some(enc) = self.enc.as_mut() {
            enc.drain()
                .map_err(|e| Error::element_failed(ctx.element(), LavAacError::Encode(e)))?;
        }
        self.emit(ctx)?;
        self.open(ctx)
    }

    fn send(&mut self, ctx: &Ctx, pcm: &[f32]) -> Result<Sent, Error> {
        let Some(enc) = self.enc.as_mut() else {
            return Ok(Sent::Ended);
        };
        enc.send(pcm, self.sent)
            .map_err(|e| Error::element_failed(ctx.element(), LavAacError::Encode(e)))
    }

    fn encode(&mut self, ctx: &mut Ctx, pcm: &[f32]) -> Result<(), Error> {
        let (Some(i), true) = (self.input, self.enc.is_some()) else {
            return Ok(());
        };
        let samples = (pcm.len() / usize::from(i.channels)) as i64;
        let mut sent = self.send(ctx, pcm)?;
        if sent == Sent::Full {
            // Full holds the frame back until the packet ahead of it is received.
            self.emit(ctx)?;
            sent = self.send(ctx, pcm)?;
        }
        if sent == Sent::Taken {
            self.sent += samples;
        } else {
            self.lost += samples as u64;
            if self.lost == samples as u64 {
                log!(
                    &*ctx,
                    Level::Warn,
                    "pcm_lost",
                    samples = samples,
                    ended = sent == Sent::Ended
                );
            }
        }
        self.emit(ctx)
    }

    fn emit(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let (Some(enc), Some(i)) = (self.enc.as_mut(), self.input) else {
            return Ok(());
        };
        // place() sets it before any PCM goes in, the fallback never stamps a packet.
        let origin = self.origin.unwrap_or(self.floor.unwrap_or(0));
        let delay = enc.delay() as i64;
        loop {
            let (data, pts, duration) = match enc.receive() {
                Ok(Encoded::Packet { data, pts, duration }) => (data, pts, duration),
                Ok(Encoded::Again | Encoded::Ended) => return Ok(()),
                Err(e) => return Err(Error::element_failed(ctx.element(), LavAacError::Encode(e))),
            };
            // The priming packet presents `delay` ahead of the origin. After a restart it
            // overlaps what went out and no consumer takes overlapping audio.
            let at = origin + pts;
            if let Some(floor) = self.floor.filter(|f| at < *f) {
                log!(&*ctx, Level::Debug, "overlap_drop", at = at, floor = floor);
                continue;
            }
            let end = at + duration.max(1);
            let stamp = Timestamp(ns_of(at + delay, i.rate) as u64);
            if self.head_due {
                self.head_due = false;
                let cfg = &self.config;
                let Some(mut head) = ctx.alloc_exact(cfg.len()) else {
                    return Err(ctx.exact_refused(cfg.len()));
                };
                head.memory.as_mut_full()[..cfg.len()].copy_from_slice(cfg);
                head.memory.set_len(cfg.len());
                head.pts = stamp;
                head.dts = stamp;
                log!(&*ctx, Level::Debug, "config_out", bytes = cfg.len(), pts = stamp);
                ctx.out(Self::OUT).push(head);
            }
            let Some(mut buf) = ctx.alloc_exact(data.len()) else {
                return Err(ctx.exact_refused(data.len()));
            };
            buf.memory.as_mut_full()[..data.len()].copy_from_slice(data);
            buf.memory.set_len(data.len());
            buf.pts = stamp;
            buf.dts = stamp;
            buf.duration =
                Timestamp((ns_of(end + delay, i.rate) - ns_of(at + delay, i.rate)) as u64);
            buf.flags = BufferFlags::KEYFRAME;
            log!(&*ctx, Level::Trace, "packet", pts = stamp, bytes = data.len());
            ctx.out(Self::OUT).push(buf);
            self.packets += 1;
            self.floor = Some(end);
        }
    }

    fn place(
        &mut self,
        ctx: &mut Ctx,
        pts: Option<i64>,
        discont: bool,
        samples: usize,
    ) -> Result<Option<usize>, Error> {
        let Some(i) = self.input else {
            return Ok(Some(0));
        };
        let slip = sample_of(SLIP_NS, i.rate);
        if let (Some(e), Some(p)) = (self.expected(), pts)
            && (discont || (sample_of(p, i.rate) - e).abs() > slip)
        {
            log!(
                &*ctx,
                Level::Debug,
                "restart",
                discont = discont,
                slip = sample_of(p, i.rate) - e
            );
            self.finish(ctx)?;
        }
        if self.origin.is_some() {
            return Ok(Some(0));
        }
        let Some(p) = pts else {
            let at = self.floor.unwrap_or(0);
            log!(&*ctx, Level::Debug, "origin", sample = at, skip = 0, stamped = false);
            self.origin = Some(at);
            return Ok(Some(0));
        };
        let s = sample_of(p, i.rate);
        let frame = self.enc.as_ref().map_or(1024, Encoder::frame_size) as i64;
        let start = if self.aligned {
            s.div_euclid(frame) * frame + if s.rem_euclid(frame) == 0 { 0 } else { frame }
        } else {
            s
        };
        let skip = (start - s) as usize;
        if skip >= samples {
            log!(&*ctx, Level::Debug, "align_drop", samples = samples, to = start);
            return Ok(None);
        }
        log!(&*ctx, Level::Debug, "origin", sample = start, skip = skip, stamped = true);
        self.origin = Some(start);
        Ok(Some(skip))
    }

    fn take(
        &mut self,
        ctx: &mut Ctx,
        data: &[u8],
        pts: Option<i64>,
        discont: bool,
    ) -> Result<(), Error> {
        let Some(i) = self.input else {
            return Err(Error::element_failed(ctx.element(), LavAacError::InputFormatUnknown));
        };
        let ch = usize::from(i.channels);
        let width = if i.float { 4 } else { 2 };
        let samples = data.len() / (width * ch);
        let Some(skip) = self.place(ctx, pts, discont, samples)? else {
            return Ok(());
        };
        let frame = self.enc.as_ref().map_or(1024, Encoder::frame_size) * ch;
        let body = &data[skip * width * ch..samples * width * ch];
        if i.float {
            for c in body.as_chunks::<4>().0 {
                self.acc.push(f32::from_le_bytes(*c));
                self.drain_full(ctx, frame)?;
            }
        } else {
            for c in body.as_chunks::<2>().0 {
                self.acc.push(f32::from(i16::from_le_bytes(*c)) / 32768.0);
                self.drain_full(ctx, frame)?;
            }
        }
        Ok(())
    }

    fn drain_full(&mut self, ctx: &mut Ctx, frame: usize) -> Result<(), Error> {
        if self.acc.len() < frame {
            return Ok(());
        }
        let pcm = std::mem::take(&mut self.acc);
        let r = self.encode(ctx, &pcm);
        self.acc = pcm;
        self.acc.clear();
        r
    }

    fn reset(&mut self, ctx: &Ctx) -> Result<(), Error> {
        log!(ctx, Level::Debug, "flush", tail = self.acc.len(), lost = self.lost);
        self.floor = None;
        self.head_due = true;
        if self.input.is_some() {
            self.open(ctx)?;
        }
        Ok(())
    }
}

impl Element for LibavAacEnc {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(Value::Int(b)) = ctx.prop("bitrate") {
            self.bitrate = u32::try_from(b.max(0)).unwrap_or(0);
        }
        if let Some(Value::Int(a)) = ctx.prop("aligned") {
            self.aligned = a != 0;
        }
        if let Some(Value::Int(f)) = ctx.prop("fast") {
            self.fast = f != 0;
        }
        self.build(ctx)
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.build(ctx)?;
        while let Some(b) = inputs.pop() {
            let pts = b.pts.nanos().map(|p| p as i64);
            self.take(ctx, b.memory.data(), pts, b.flags.contains(BufferFlags::DISCONT))?;
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FormatChange(_) => self.build(ctx)?,
            Event::FlushStart => self.reset(ctx)?,
            Event::Eos => {
                self.finish(ctx)?;
                log!(&*ctx, Level::Info, "eos", packets = self.packets, lost = self.lost);
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.enc = None;
        self.input = None;
        self.acc.clear();
        self.origin = None;
        self.packets = 0;
        self.lost = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_and_samples_convert_both_ways() {
        assert_eq!(sample_of(1_000_000_000, 48_000), 48_000);
        assert_eq!(sample_of(21_333_333, 48_000), 1024);
        assert_eq!(ns_of(1024, 48_000), 21_333_333);
        assert_eq!(ns_of(-1024, 48_000), -21_333_333);
        assert_eq!(sample_of(ns_of(1_234_567, 44_100), 44_100), 1_234_567);
    }
}
