//! `opusenc`, 48 kHz `s16` PCM to one Opus packet per buffer, the last frame zero padded. Stamps
//! count from an input that begins a frame, so a gap or seek upstream carries through.
//! `aligned` puts the frames on absolute multiples of the frame duration instead, so a run
//! started mid-stream cuts and stamps its frames where a full run did. It drops the samples
//! before the first grid point and starts over on the grid when a stamp slips from the count.

use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
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

use crate::config::{Application, EncoderConfig, OPUS_RATE};
use crate::error::{CodecError, OpusError};

trait EncBackend: Send {
    fn frame_bytes(&self) -> usize;
    fn encode(&mut self, frame: &[u8], out: &mut Vec<u8>) -> Result<(), CodecError>;
    fn reset(&mut self);
}

impl EncBackend for crate::libopus::LibopusEncoder {
    fn frame_bytes(&self) -> usize {
        crate::libopus::LibopusEncoder::frame_bytes(self)
    }
    fn encode(&mut self, frame: &[u8], out: &mut Vec<u8>) -> Result<(), CodecError> {
        crate::libopus::LibopusEncoder::encode(self, frame, out)
    }
    fn reset(&mut self) {
        crate::libopus::LibopusEncoder::reset(self)
    }
}

#[allow(clippy::disallowed_methods)] // one-time per-stream construction (Box), not a hot path
fn make_backend(cfg: EncoderConfig) -> Result<Box<dyn EncBackend>, CodecError> {
    crate::libopus::LibopusEncoder::new(cfg).map(|e| Box::new(e) as Box<dyn EncBackend>)
}

const FAMILY_RAW: &str = "audio/raw";
const FAMILY_OPUS: &str = "opus";
const F_RATE: &str = "rate";
const F_CHANNELS: &str = "channels";
const F_SAMPLE: &str = "sample";

/// One second of 48 kHz stereo `s16`, past which unformatted input fails instead of buffering.
const MAX_UNFORMATTED_BYTES: usize = OPUS_RATE as usize * 2 * 2;

const DEFAULT_FRAME_TENTHS: u16 = 200;

/// Aligned, a stamp this far from the count starts over on the grid.
const SLIP_NS: i128 = 5_000_000;

static SINK_FIELDS: [FieldDesc; 3] = [
    FieldDesc {
        field: F_RATE,
        allowed: ConstraintDesc::Set(&[ValueDesc::Int(OPUS_RATE as i64)]),
        preferred: None,
    },
    FieldDesc {
        field: F_CHANNELS,
        allowed: ConstraintDesc::Set(&[ValueDesc::Int(1), ValueDesc::Int(2)]),
        preferred: None,
    },
    FieldDesc {
        field: F_SAMPLE,
        allowed: ConstraintDesc::Set(&[ValueDesc::Id("s16")]),
        preferred: None,
    },
];
static SINK_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: FAMILY_RAW, fields: &SINK_FIELDS }, OfferDesc::any("bytes")];

static SRC_OFFERS: [OfferDesc; 2] = [OfferDesc::any(FAMILY_OPUS), OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

/// `bitrate` 0 derives from channels, `vbr` 0 is CBR, `complexity` 0 to 10, `aligned` 1 puts
/// frames on absolute multiples of the frame duration.
static PROPS: [PropDesc; 4] = [
    PropDesc { name: "bitrate", allowed: Constraint::Any, live: false },
    PropDesc { name: "vbr", allowed: Constraint::Any, live: false },
    PropDesc { name: "complexity", allowed: Constraint::Any, live: false },
    PropDesc { name: "aligned", allowed: Constraint::Any, live: false },
];

#[allow(clippy::disallowed_methods)] // make_default boxes once at parse time
pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "opusenc",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    // Up to one frame plus lookahead, left zero until the clock gives a figure.
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(OpusEnc::new())),
    io: false,
    couplings: &[],
};

pub struct OpusEnc {
    /// 0 derives from channels.
    bitrate_bps: u32,
    frame_ms_tenths: u16,
    application: Application,
    vbr: bool,
    complexity: u8,
    aligned: bool,
    enc: Option<Box<dyn EncBackend>>,
    channels: usize,
    frame_bytes: usize,
    /// Carry for a frame straddling two inputs, whole frames encode in place.
    acc: Vec<u8>,
    packet: Vec<u8>,
    origin: Timestamp,
    sent: u64,
    /// Aligned, samples still to drop before the first grid point.
    skip: u64,
    packets: u64,
    rejected: u64,
}

impl Default for OpusEnc {
    fn default() -> Self {
        Self::new()
    }
}

impl OpusEnc {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    #[allow(clippy::disallowed_methods)] // one-time construction, the Vecs are reused
    pub fn new() -> Self {
        OpusEnc {
            bitrate_bps: 0,
            frame_ms_tenths: DEFAULT_FRAME_TENTHS,
            application: Application::Audio,
            vbr: true,
            complexity: 10,
            aligned: false,
            enc: None,
            channels: 0,
            frame_bytes: 0,
            acc: Vec::new(),
            packet: Vec::new(),
            origin: Timestamp::NONE,
            sent: 0,
            skip: 0,
            packets: 0,
            rejected: 0,
        }
    }

    /// Frames on absolute multiples of the frame duration, so a mid-stream start cuts and
    /// stamps where a full run did.
    pub fn aligned(mut self) -> Self {
        self.aligned = true;
        self
    }

    fn frame_ns(&self) -> u64 {
        u64::from(self.frame_ms_tenths) * 100_000
    }

    /// Aligned, the input's place on the grid. A stamp off the count by more than `SLIP_NS` or a
    /// discont pads out the carry and starts over at the next grid point.
    fn place(&mut self, ctx: &mut Ctx, pts: Timestamp, discont: bool) -> Result<(), Error> {
        // Unformatted input has no sample size yet, it is placed once and counted after.
        let stride = self.channels * 2;
        if self.origin.is_some() && pts.is_some() && stride > 0 {
            let due = (self.sent + (self.acc.len() / stride) as u64) as i128 - self.skip as i128;
            let expected = self.origin.0 as i128 + due * 1_000_000_000 / i128::from(OPUS_RATE);
            let slip = pts.0 as i128 - expected;
            if !discont && slip.abs() <= SLIP_NS {
                return Ok(());
            }
            log!(&*ctx, Level::Debug, "slip", ns = slip as i64, discont = discont);
            self.pad_tail(ctx)?;
            (self.origin, self.skip) = (Timestamp::NONE, 0);
        }
        if self.origin.is_some() {
            return Ok(());
        }
        let p = pts.nanos().unwrap_or(0);
        let f = self.frame_ns();
        let grid = p.div_ceil(f).saturating_mul(f);
        let ns = u128::from(grid.saturating_sub(p));
        self.skip = ((ns * u128::from(OPUS_RATE) + 500_000_000) / 1_000_000_000) as u64;
        (self.origin, self.sent) = (Timestamp(grid), 0);
        log!(&*ctx, Level::Debug, "origin", pts = Timestamp(grid), skip = self.skip);
        Ok(())
    }

    /// Bytes of `data` that go before the grid, taken off the pending skip.
    fn skipped(&mut self, len: usize) -> usize {
        let stride = (self.channels * 2) as u64;
        if self.skip == 0 || stride == 0 {
            return 0;
        }
        let n = (self.skip * stride).min(len as u64);
        self.skip -= n / stride;
        n as usize
    }

    /// Encodes the partial frame in `acc` zero padded, the padding left out of its duration.
    fn pad_tail(&mut self, ctx: &mut Ctx) -> Result<usize, Error> {
        if self.frame_bytes == 0 || self.acc.is_empty() {
            return Ok(0);
        }
        let padded = self.frame_bytes.saturating_sub(self.acc.len());
        self.acc.resize(self.frame_bytes, 0);
        let mut carry = std::mem::take(&mut self.acc);
        let res = self.encode_frame_bytes(ctx, &carry[..self.frame_bytes], padded);
        carry.clear();
        self.acc = carry;
        res.map(|()| padded)
    }

    fn anchor(&mut self, ctx: &Ctx, pts: Timestamp) {
        if !self.acc.is_empty() {
            return;
        }
        if pts.is_some() {
            let expected = self.at(self.sent);
            // One 48 kHz sample of slack, a counted stamp may round either way.
            if self.origin.is_some() && pts.0.abs_diff(expected.0) > 20_834 {
                log!(ctx, Level::Debug, "reanchor", expected = expected, pts = pts);
            }
            (self.origin, self.sent) = (pts, 0);
        } else if self.origin.is_none() {
            (self.origin, self.sent) = (Timestamp::ZERO, 0);
        }
    }

    fn at(&self, n: u64) -> Timestamp {
        let ns = (n as u128 * 1_000_000_000 / OPUS_RATE as u128) as u64;
        let origin = if self.origin.is_none() { Timestamp::ZERO } else { self.origin };
        origin.saturating_add(Timestamp::from_nanos(ns))
    }

    /// Builds the encoder once the sink format is fixed, then encodes the whole frames buffered
    /// while it was unknown.
    fn build_from_sink(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.enc.is_none() {
            self.build_encoder(ctx);
        }
        let n = self.skipped(self.acc.len());
        self.acc.drain(..n);
        self.encode_carry(ctx)
    }

    fn build_encoder(&mut self, ctx: &mut Ctx) {
        let Some(fixed) = ctx.negotiated_sink(Self::IN) else { return };
        let said = ctx.field_id(F_CHANNELS).and_then(|id| fixed.get(id));
        let channels = said.and_then(|v| match v {
            Value::Int(n) if (1..=2).contains(&n) => Some(n as usize),
            _ => None,
        });
        let Some(channels) = channels else {
            if let Some(Value::Int(n)) = said {
                log!(&*ctx, Level::Warn, "channels_unsupported", channels = n);
            }
            return;
        };

        let bitrate = if self.bitrate_bps > 0 {
            self.bitrate_bps
        } else {
            EncoderConfig::new(channels as u8).bitrate_bps
        };
        let cfg = EncoderConfig {
            sample_rate: OPUS_RATE,
            channels: channels as u8,
            bitrate_bps: bitrate,
            application: self.application,
            frame_ms_tenths: self.frame_ms_tenths,
            vbr: self.vbr,
            complexity: self.complexity,
            ..EncoderConfig::new(channels as u8)
        };
        match make_backend(cfg) {
            Ok(enc) => {
                log!(
                    &*ctx,
                    Level::Info,
                    "encoder",
                    channels = channels,
                    bitrate = bitrate,
                    vbr = self.vbr,
                    complexity = self.complexity
                );
                log!(
                    &*ctx,
                    Level::Debug,
                    "frame",
                    tenths_ms = self.frame_ms_tenths,
                    bytes = enc.frame_bytes()
                );
                self.frame_bytes = enc.frame_bytes();
                self.channels = channels;
                self.enc = Some(enc);
                let fields = [
                    (F_RATE, ValueDesc::Int(OPUS_RATE as i64)),
                    (F_CHANNELS, ValueDesc::Int(channels as i64)),
                ];
                ctx.announce_format(Self::OUT, FAMILY_OPUS, &fields);
            }
            // Unreachable under the caps, process() reports the unbuilt encoder.
            Err(e) => {
                log!(&*ctx, Level::Error, "encoder_setup_failed", reason = e.reason());
            }
        }
    }

    /// `pad` is the zero tail bytes added at EOS, left out of the stamped duration.
    fn encode_frame_bytes(&mut self, ctx: &mut Ctx, frame: &[u8], pad: usize) -> Result<(), Error> {
        let Some(enc) = self.enc.as_mut() else {
            return Err(Error::element_failed(ctx.element(), OpusError::InputFormatUnknown));
        };
        if let Err(e) = enc.encode(frame, &mut self.packet) {
            log!(&*ctx, Level::Debug, "frame_rejected", reason = e.reason());
            self.rejected += 1;
            let element = ctx.element();
            ctx.post(BusMessage::Warning {
                element,
                error: Error::element_failed(element, OpusError::FrameRejected(e)),
            });
            return Ok(());
        }
        let n = self.packet.len();
        let Some(mut buf) = ctx.alloc_exact(n) else { return Err(ctx.exact_refused(n)) };
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.packet);
        buf.memory.set_len(n);
        let samples = (frame.len().saturating_sub(pad) / (self.channels.max(1) * 2)) as u64;
        buf.pts = self.at(self.sent);
        buf.dts = buf.pts;
        buf.duration = self.at(self.sent + samples).saturating_sub(buf.pts);
        buf.flags = BufferFlags::KEYFRAME;
        self.sent += samples;
        self.packets += 1;
        ctx.out(Self::OUT).push(buf);
        Ok(())
    }

    /// Encodes every whole frame in `acc`, leaving less than one frame.
    fn encode_carry(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let fb = self.frame_bytes;
        if fb == 0 || self.acc.len() < fb {
            return Ok(());
        }
        // `mem::take` frees the borrow for `encode_frame_bytes`.
        let mut carry = std::mem::take(&mut self.acc);
        let mut off = 0;
        let mut res = Ok(());
        while res.is_ok() && off + fb <= carry.len() {
            res = self.encode_frame_bytes(ctx, &carry[off..off + fb], 0);
            off += fb;
        }
        carry.drain(..off);
        self.acc = carry;
        res
    }

    fn feed(&mut self, ctx: &mut Ctx, data: &[u8]) -> Result<(), Error> {
        let fb = self.frame_bytes;
        if fb == 0 {
            if self.acc.len() + data.len() > MAX_UNFORMATTED_BYTES {
                log!(&*ctx, Level::Warn, "unformatted_budget", bytes = self.acc.len() + data.len());
                return Err(Error::element_failed(ctx.element(), OpusError::InputFormatUnknown));
            }
            self.acc.extend_from_slice(data);
            return Ok(());
        }

        // A build always encodes the carry, so it holds less than one frame here.
        debug_assert!(self.acc.len() < fb);
        let mut off = 0;
        if !self.acc.is_empty() {
            off = (fb - self.acc.len()).min(data.len());
            self.acc.extend_from_slice(&data[..off]);
            if self.acc.len() < fb {
                return Ok(()); // still partial, wait for more input
            }
            self.encode_carry(ctx)?;
        }

        while off + fb <= data.len() {
            self.encode_frame_bytes(ctx, &data[off..off + fb], 0)?;
            off += fb;
        }
        self.acc.extend_from_slice(&data[off..]);
        Ok(())
    }
}

impl Element for OpusEnc {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(Value::Int(b)) = ctx.prop("bitrate")
            && b > 0
        {
            self.bitrate_bps = b as u32;
        }
        if let Some(Value::Int(v)) = ctx.prop("vbr") {
            self.vbr = v != 0;
        }
        if let Some(Value::Int(c)) = ctx.prop("complexity") {
            self.complexity = c.clamp(0, 10) as u8;
        }
        if let Some(Value::Int(a)) = ctx.prop("aligned") {
            self.aligned = a != 0;
        }
        self.enc = None;
        self.channels = 0;
        self.frame_bytes = 0;
        self.acc.clear();
        (self.origin, self.sent, self.skip) = (Timestamp::NONE, 0, 0);
        (self.packets, self.rejected) = (0, 0);
        // Link-time negotiation may already have fixed the sink format.
        self.build_from_sink(ctx)
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.build_from_sink(ctx)?;
        while let Some(buf) = inputs.pop() {
            let data = buf.memory.data();
            if !self.aligned {
                self.anchor(ctx, buf.pts);
                self.feed(ctx, data)?;
                continue;
            }
            self.place(ctx, buf.pts, buf.flags.contains(BufferFlags::DISCONT))?;
            let n = self.skipped(data.len());
            self.feed(ctx, &data[n..])?;
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FormatChange(_) => self.build_from_sink(ctx)?,
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", carry = self.acc.len());
                if let Some(enc) = self.enc.as_mut() {
                    enc.reset();
                }
                self.acc.clear();
                (self.origin, self.sent, self.skip) = (Timestamp::NONE, 0, 0);
            }
            Event::Eos => {
                // A format fixed without a FormatChange still gets the buffered input encoded.
                self.build_from_sink(ctx)?;
                let padded = self.pad_tail(ctx)?;
                if self.frame_bytes == 0 && !self.acc.is_empty() {
                    log!(&*ctx, Level::Error, "eos_unformatted", bytes = self.acc.len());
                }
                let level = if self.rejected > 0 { Level::Warn } else { Level::Info };
                log!(
                    &*ctx,
                    level,
                    "eos",
                    packets = self.packets,
                    samples = self.sent,
                    rejected = self.rejected,
                    padded = padded
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.acc.clear();
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests: one-shot fixtures, not a hot path
mod tests {
    use super::*;
    use crate::libopus::{LibopusDecoder, LibopusEncoder};

    #[test]
    fn desc_is_named_and_registered_shape() {
        let e = OpusEnc::new();
        assert_eq!(e.desc().name, "opusenc");
        assert_eq!((e.desc().sinks.len(), e.desc().srcs.len()), (1, 1));
    }

    #[test]
    fn irregular_chunks_reframe_and_decode() {
        let mut enc = LibopusEncoder::new(EncoderConfig::new(1)).expect("encoder");
        let frame_bytes = enc.frame_bytes();
        let n = frame_bytes / 2;
        let mut bytes = Vec::new();
        for i in 0..n * 3 {
            let s = ((i % 200) as i16 - 100) * 100;
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let mut acc: Vec<u8> = Vec::new();
        let mut dec = LibopusDecoder::new();
        dec.set_channels(1);
        let (mut packet, mut pcm) = (Vec::new(), Vec::new());
        let mut total_out = 0usize;
        for chunk in bytes.chunks(777) {
            acc.extend_from_slice(chunk);
            while acc.len() >= frame_bytes {
                enc.encode(&acc[..frame_bytes], &mut packet).expect("encode");
                acc.drain(..frame_bytes);
                assert_eq!(dec.decode_packet_into(&packet, &mut pcm), Ok(1));
                total_out += pcm.len();
            }
        }
        assert_eq!(total_out, n * 3, "all three frames decoded");
    }
}
