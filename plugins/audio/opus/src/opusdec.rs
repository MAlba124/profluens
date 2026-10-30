//! `opusdec`, Opus packets to `f32` or `s16` PCM at the best rate downstream takes.

use profluens_audio::{
    DELAY_FIELDS, FIELD_LAYOUT, LAYOUT_FIELD, Place, Rebase, SampleFormat, declared_delay,
    default_layout, delay_frames, link_samples, place,
};
use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::error::{CodecError, OpusError};
use crate::head::{OPUS_HEAD_MAGIC, OpusHead, apply_output_gain, apply_output_gain_f32};
use crate::libopus::DECODE_RATES;
use crate::multistream::{Layout, MAX_CHANNELS, canonical_order, reorder};

const FAMILY: &str = "audio/raw";
const F_RATE: &str = "rate";
const F_CHANNELS: &str = "channels";
const F_SAMPLE: &str = "sample";

const OPUS_RATE: u32 = 48_000;

// Stated so a device taking none of them gets a resampler at link time.
static RATE_VALUES: [ValueDesc; 5] = [
    ValueDesc::Int(DECODE_RATES[0] as i64),
    ValueDesc::Int(DECODE_RATES[1] as i64),
    ValueDesc::Int(DECODE_RATES[2] as i64),
    ValueDesc::Int(DECODE_RATES[3] as i64),
    ValueDesc::Int(DECODE_RATES[4] as i64),
];
static SRC_FIELDS: [FieldDesc; 4] = [
    FieldDesc { field: F_RATE, allowed: ConstraintDesc::Set(&RATE_VALUES), preferred: None },
    FieldDesc { field: F_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: F_SAMPLE,
        allowed: ConstraintDesc::Set(&[ValueDesc::Id("s16"), ValueDesc::Id("f32")]),
        preferred: None,
    },
    LAYOUT_FIELD,
];
static SRC_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: FAMILY, fields: &SRC_FIELDS }, OfferDesc::any("bytes")];
// Naming the `delay` field lets it read the one a demuxer declares.
static SINK_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: "opus", fields: &DELAY_FIELDS }, OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

#[allow(clippy::disallowed_methods)] // make_default boxes once at parse time
static DESC: ElementDesc = ElementDesc {
    name: "opusdec",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(OpusDec::new())),
    io: false,
    couplings: &[Coupling::takes(0, F_RATE), Coupling::takes(0, F_SAMPLE)],
};

enum Pcm {
    S16(Vec<i16>),
    F32(Vec<f32>),
}

impl Pcm {
    fn len(&self) -> usize {
        match self {
            Pcm::S16(v) => v.len(),
            Pcm::F32(v) => v.len(),
        }
    }

    fn clear(&mut self) {
        match self {
            Pcm::S16(v) => v.clear(),
            Pcm::F32(v) => v.clear(),
        }
    }

    fn truncate(&mut self, len: usize) {
        match self {
            Pcm::S16(v) => v.truncate(len),
            Pcm::F32(v) => v.truncate(len),
        }
    }

    fn sample(&self) -> SampleFormat {
        match self {
            Pcm::S16(_) => SampleFormat::S16,
            Pcm::F32(_) => SampleFormat::F32,
        }
    }

    fn put(&self, from: usize, to: usize, dst: &mut [u8]) -> usize {
        match self {
            Pcm::S16(v) => put_le(&v[from..to], dst, i16::to_le_bytes),
            Pcm::F32(v) => put_le(&v[from..to], dst, f32::to_le_bytes),
        }
    }
}

fn put_le<T: Copy, const N: usize>(src: &[T], dst: &mut [u8], f: fn(T) -> [u8; N]) -> usize {
    for (d, s) in dst.as_chunks_mut::<N>().0.iter_mut().zip(src) {
        *d = f(*s);
    }
    src.len() * N
}

trait DecBackend: Send {
    fn set_channels(&mut self, channels: usize);
    fn decode_into(&mut self, packet: &[u8], pcm: &mut Pcm) -> Result<u8, CodecError>;
    fn reset(&mut self);
}

impl DecBackend for crate::libopus::LibopusDecoder {
    fn set_channels(&mut self, channels: usize) {
        crate::libopus::LibopusDecoder::set_channels(self, channels)
    }
    fn decode_into(&mut self, packet: &[u8], pcm: &mut Pcm) -> Result<u8, CodecError> {
        match pcm {
            Pcm::S16(v) => self.decode_packet_into(packet, v),
            Pcm::F32(v) => self.decode_float_into(packet, v),
        }
    }
    fn reset(&mut self) {
        crate::libopus::LibopusDecoder::reset(self)
    }
}

impl DecBackend for crate::libopus::LibopusMsDecoder {
    fn set_channels(&mut self, _channels: usize) {} // fixed by the head it was built from
    fn decode_into(&mut self, packet: &[u8], pcm: &mut Pcm) -> Result<u8, CodecError> {
        match pcm {
            Pcm::S16(v) => self.decode_packet_into(packet, v),
            Pcm::F32(v) => self.decode_float_into(packet, v),
        }
    }
    fn reset(&mut self) {
        crate::libopus::LibopusMsDecoder::reset(self)
    }
}

#[allow(clippy::disallowed_methods)] // once per stream, from its head
fn make_multistream(layout: &Layout, rate: u32) -> Result<Box<dyn DecBackend>, CodecError> {
    let dec = crate::libopus::LibopusMsDecoder::at_rate(
        rate,
        layout.streams,
        layout.coupled,
        layout.mapping(),
    )?;
    Ok(Box::new(dec))
}

#[allow(clippy::disallowed_methods)] // one-time construction at registry/parse time
fn make_decoder(rate: u32) -> Box<dyn DecBackend> {
    Box::new(crate::libopus::LibopusDecoder::at_rate(rate))
}

fn decode_rate(takes: Option<&[Value]>) -> u32 {
    let Some(takes) = takes else { return OPUS_RATE };
    let taken = |r: &u32| takes.contains(&Value::Int(i64::from(*r)));
    DECODE_RATES.into_iter().find(taken).unwrap_or(OPUS_RATE)
}

pub struct OpusDec {
    dec: Box<dyn DecBackend>,
    rate: u32,
    /// Fixed once said, from `OpusHead` or the first packet.
    channels: usize,
    /// The delay when the container declares none (RFC 7845 section 4.2).
    pre_skip: u32,
    rebase: Rebase,
    output_gain_q7_8: i16,
    order: Option<[u8; MAX_CHANNELS]>,
    pending: Pcm,
    pending_pos: usize,
    at: Place,
    decoded: u64,
    dropped: u64,
}

impl Default for OpusDec {
    fn default() -> Self {
        Self::new()
    }
}

impl OpusDec {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    #[allow(clippy::disallowed_methods)] // one-time construction at registry/parse time
    pub fn new() -> Self {
        OpusDec {
            dec: make_decoder(OPUS_RATE),
            rate: OPUS_RATE,
            channels: 0,
            pre_skip: 0,
            rebase: Rebase::Open,
            output_gain_q7_8: 0,
            order: None,
            pending: Pcm::S16(Vec::new()),
            pending_pos: 0,
            at: Place::default(),
            decoded: 0,
            dropped: 0,
        }
    }

    fn at_rate(&self, n: u64) -> u64 {
        (n * u64::from(self.rate) + u64::from(OPUS_RATE / 2)) / u64::from(OPUS_RATE)
    }

    /// Steps over the carry before the presentation point, a whole carry counts as a drop.
    fn skip_before_point(&mut self, ctx: &Ctx) -> bool {
        let ch = self.channels.max(1);
        let frames = (self.pending.len() - self.pending_pos) / ch;
        let point = ctx.presentation_point();
        let skip = self.at.cut(frames, self.rate, point);
        self.pending_pos += skip * ch;
        if skip > 0 && skip == frames {
            log!(ctx, Level::Trace, "drop_before_point", frames = frames);
            ctx.count_drop();
        }
        self.pending_pos < self.pending.len()
    }

    /// Container stamps run ahead by the codec delay (RFC 9559 section 5.1.4.1.25, RFC 7845
    /// section 4.3), taken once, the container's if declared else the pre-skip. Behind a head
    /// the pre-skip is cut whatever the stamp (RFC 7845 section 4.2).
    fn place(&mut self, ctx: &Ctx) {
        let Some(stamped) = self.rebase.first_frame(self.rate) else { return };
        let declared = delay_frames(declared_delay(ctx, Self::IN), self.rate);
        let delay = if declared > 0 { declared } else { self.at_rate(u64::from(self.pre_skip)) };
        let (head, at) = (self.at.lead, place(stamped, delay));
        let lead = head.max(at.lead);
        self.at = Place { position: stamped.saturating_add(lead).saturating_sub(delay), lead };
        log!(
            ctx,
            Level::Debug,
            "place",
            stamped = stamped,
            delay = delay,
            declared = declared > 0,
            lead = lead
        );
    }

    /// Ogg has no time (RFC 3533 section 4), so the decoder owns the stream's time base.
    fn pts_now(&self) -> Timestamp {
        Timestamp::from_nanos(
            (u128::from(self.at.position) * 1_000_000_000 / u128::from(self.rate)) as u64,
        )
    }

    fn announce(&mut self, ctx: &mut Ctx, channels: usize) {
        if self.channels != 0 {
            return;
        }
        self.channels = channels.max(1);
        ctx.announce_format(
            Self::OUT,
            FAMILY,
            &[
                (F_RATE, ValueDesc::Int(i64::from(self.rate))),
                (F_CHANNELS, ValueDesc::Int(self.channels as i64)),
                (F_SAMPLE, ValueDesc::Id(self.pending.sample().caps_name())),
                (FIELD_LAYOUT, ValueDesc::Int(default_layout(self.channels as u32) as i64)),
            ],
        );
    }

    /// Bounded returns `Ok(false)` on a full pool, unbounded drains the EOS tail.
    #[allow(clippy::disallowed_methods)] // pool-backed, the pending Vec is a reused carry
    fn emit_pending(&mut self, ctx: &mut Ctx, bounded: bool) -> Result<bool, Error> {
        let _ = self.skip_before_point(ctx);
        while self.pending_pos < self.pending.len() {
            let mut buf = if bounded {
                match ctx.try_alloc() {
                    Some(b) => b,
                    None => return Ok(false), // pool full, backpressure
                }
            } else {
                ctx.alloc()
            };
            // Whole frames per buffer keep the stamps on the sample grid.
            let ch = self.channels.max(1);
            let per = (buf.memory.capacity() / (self.pending.sample().bytes() * ch)).max(1) * ch;
            let end = (self.pending_pos + per).min(self.pending.len());
            let n = self.pending.put(self.pending_pos, end, buf.memory.as_mut_full());
            buf.memory.set_len(n);
            buf.pts = self.pts_now();
            self.at.position += ((end - self.pending_pos) / self.channels.max(1)) as u64;
            buf.duration = self.pts_now().saturating_sub(buf.pts);
            ctx.out(Self::OUT).push(buf);
            self.pending_pos = end;
        }
        self.pending.clear();
        self.pending_pos = 0;
        Ok(true)
    }

    /// A stated duration shorter than the packet is padding the container cut, Matroska's
    /// DiscardPadding (RFC 9559 section 5.1.3.5.7), dropped from the end.
    fn cut_to(&mut self, ctx: &Ctx, duration: Timestamp) {
        let Some(ns) = duration.nanos() else { return };
        let ch = self.channels.max(1);
        let keep = (u128::from(ns) * u128::from(self.rate) + 500_000_000) / 1_000_000_000;
        let keep = (keep.min(usize::MAX as u128) as usize).saturating_mul(ch);
        if keep < self.pending.len() {
            log!(ctx, Level::Debug, "discard_padding", frames = (self.pending.len() - keep) / ch);
            self.pending.truncate(keep);
        }
    }

    fn decode_audio(&mut self, ctx: &mut Ctx, packet: &[u8]) -> Result<(), Error> {
        let decoded = self.dec.decode_into(packet, &mut self.pending).and_then(|ch| {
            let ch = usize::from(ch).max(1);
            // PCM of another width than announced would be read as other frames.
            match self.channels {
                0 => Ok(ch),
                n if n == ch => Ok(ch),
                _ => Err(CodecError::Channels { channels: ch }),
            }
        });
        let channels = match decoded {
            Ok(ch) => ch,
            // Warn and drop a corrupt packet, the next clean one resumes.
            Err(e) => {
                log!(
                    &*ctx,
                    Level::Debug,
                    "packet_dropped",
                    reason = e.reason(),
                    len = packet.len()
                );
                self.dropped += 1;
                let element = ctx.element();
                ctx.post(BusMessage::Warning {
                    element,
                    error: Error::element_failed(element, OpusError::PacketDropped(e)),
                });
                self.pending.clear();
                self.pending_pos = 0;
                self.rebase.dropped();
                return Ok(());
            }
        };
        self.pending_pos = 0;
        self.decoded += 1;
        self.announce(ctx, channels);
        if let Some(order) = &self.order {
            match &mut self.pending {
                Pcm::S16(v) => reorder(v, self.channels, order),
                Pcm::F32(v) => reorder(v, self.channels, order),
            }
        }
        self.place(ctx);
        match &mut self.pending {
            Pcm::S16(v) => apply_output_gain(v, self.output_gain_q7_8),
            Pcm::F32(v) => apply_output_gain_f32(v, self.output_gain_q7_8),
        }
        // The pre-skip is the lead of the place, `skip_before_point` cuts it.
        Ok(())
    }
}

impl Element for OpusDec {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    #[allow(clippy::disallowed_methods)] // once per run, the decoder at the rate picked
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        *self = OpusDec::new();
        self.rate = decode_rate(ctx.field_id(F_RATE).and_then(|f| ctx.src_takes(Self::OUT, f)));
        if link_samples(ctx, Self::OUT).has(SampleFormat::F32) {
            self.pending = Pcm::F32(Vec::new());
        }
        if self.rate != OPUS_RATE {
            self.dec = make_decoder(self.rate);
        }
        log!(
            &*ctx,
            Level::Info,
            "open",
            rate = self.rate,
            sample = self.pending.sample().caps_name()
        );
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.emit_pending(ctx, true)? {
            return Ok(Flow::Ok); // pool full, yield with the input buffered
        }
        while let Some(buf) = inputs.pop() {
            let data = buf.memory.data();
            if data.len() >= 8 && &data[..8] == OPUS_HEAD_MAGIC.as_slice() {
                let head = OpusHead::parse(data);
                if let Err(e) = &head {
                    log!(&*ctx, Level::Warn, "head_rejected", reason = e.reason());
                }
                if let Ok(head) = head {
                    log!(
                        &*ctx,
                        Level::Info,
                        "head",
                        channels = head.channel_count,
                        pre_skip = head.pre_skip,
                        family = head.mapping_family,
                        gain_q7_8 = head.output_gain_q7_8
                    );
                    // Family 255 and the ambisonic 2 and 3 (RFC 8486) have no order to announce.
                    if head.mapping_family > 1 {
                        return Err(Error::element_failed(
                            ctx.element(),
                            OpusError::UnsupportedChannelMapping {
                                family: head.mapping_family,
                                channels: head.channel_count,
                            },
                        ));
                    }
                    self.at.lead = self.at_rate(u64::from(head.pre_skip));
                    self.pre_skip = head.pre_skip as u32;
                    self.output_gain_q7_8 = head.output_gain_q7_8;
                    // Only while no width is said, family 1 needs the multistream decoder.
                    if self.channels == 0 {
                        match Layout::from_head(&head) {
                            Some(layout) => {
                                self.dec = make_multistream(&layout, self.rate).map_err(|e| {
                                    Error::element_failed(ctx.element(), OpusError::DecoderSetup(e))
                                })?;
                                self.order = canonical_order(usize::from(layout.channels));
                            }
                            None => self.dec.set_channels(head.channel_count as usize),
                        }
                        self.announce(ctx, head.channel_count as usize);
                    } else if usize::from(head.channel_count) != self.channels {
                        log!(
                            &*ctx,
                            Level::Warn,
                            "head_channels_ignored",
                            head = head.channel_count,
                            channels = self.channels
                        );
                    }
                }
                continue;
            }
            if data.len() >= 8 && &data[..8] == b"OpusTags" {
                continue; // RFC 7845 section 5.2 comment header
            }
            self.rebase.input(buf.pts);
            self.decode_audio(ctx, data)?;
            self.cut_to(ctx, buf.duration);
            if !self.emit_pending(ctx, true)? {
                return Ok(Flow::Ok); // pool filled mid-packet, carry the rest, yield
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            // The pre-skip is not re-armed, only a stamp or a repeated head cuts it again.
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", pending = self.pending.len() - self.pending_pos);
                self.dec.reset();
                self.pending.clear();
                self.pending_pos = 0;
                self.rebase = Rebase::Open;
                self.at.lead = 0;
            }
            // Opus has no decoder-buffered frames, only the carry drains.
            Event::Eos => {
                self.emit_pending(ctx, false)?;
                let level = if self.dropped > 0 { Level::Warn } else { Level::Info };
                log!(
                    &*ctx,
                    level,
                    "eos",
                    decoded = self.decoded,
                    dropped = self.dropped,
                    position = self.at.position
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.pending.clear();
        self.pending_pos = 0;
    }
}
