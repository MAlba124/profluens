//! `audiostretch`, pitch-preserving rate change by PICOLA (Morita and Itakura, ICASSP 1986).

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
use profluens_core::time::{Rate, Timestamp};

use crate::convert::{convert_interleaved, converted_len};
use crate::error::{AudioErrorKind as K, failed};
use crate::format::{
    AudioFormat, FAMILY, FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE, SampleFormat, announce_raw,
};
use crate::layout::negotiated_layout;
use crate::trim::{position_at, time_at};

const MIN_PITCH_HZ: usize = 65;
const MAX_PITCH_HZ: usize = 400;
const AMDF_FREQ: usize = 4000;
const BYPASS_CHUNK: usize = 1024;

/// The slowest and the fastest rate, as `(num, den)`.
pub const RATE_MIN: (u16, u16) = (1, 4);
pub const RATE_MAX: (u16, u16) = (4, 1);

const WINDOW_LATENCY_NS: u64 = 3 * 1_000_000_000 / MIN_PITCH_HZ as u64;

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

static PROPS: [PropDesc; 1] = [PropDesc { name: "rate", allowed: Constraint::Any, live: false }];

// Cold path, once at construction.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "audiostretch",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc {
        min: Timestamp::ZERO,
        max: Timestamp::from_nanos(WINDOW_LATENCY_NS),
        is_live: false,
        jitter: Timestamp::ZERO,
    },
    make_default: Some(|| Box::new(AudioStretch::new(Rate::ONE))),
    io: false,
    couplings: &[Coupling::through(0, 0)],
};

struct Engine {
    fmt: AudioFormat,
    channels: usize,
    min_period: usize,
    max_period: usize,
    amdf_skip: usize,
    window: usize,
    buf: Vec<f32>,
    start: usize,
    copy_remaining: usize,
    prev_period: usize,
    prev_min_diff: f32,
    mono: Vec<f32>,
    mono_coarse: Vec<f32>,
    owed: i64,
    out: Vec<f32>,
    in_stage: Vec<u8>,
    out_f32: Vec<u8>,
    out_pcm: Vec<u8>,
    caps: [usize; 7],
}

impl Engine {
    // Cold path, once per stream. Sized for the worst-case round so `generate()` never allocates.
    #[allow(clippy::disallowed_methods)]
    fn new(fmt: AudioFormat) -> Self {
        let sr = fmt.sample_rate as usize;
        let ch = (fmt.channels as usize).max(1);
        let max_period = (sr / MIN_PITCH_HZ).max(2);
        let amdf_skip = (sr / AMDF_FREQ).max(1);
        let window = 3 * max_period;
        let buf_frames = window + 2 * BYPASS_CHUNK;
        let out_frames = window.max(2 * max_period).max(BYPASS_CHUNK);
        Self {
            fmt,
            channels: ch,
            min_period: (sr / MAX_PITCH_HZ).max(1),
            max_period,
            amdf_skip,
            window,
            buf: Vec::with_capacity(buf_frames * ch),
            start: 0,
            copy_remaining: 0,
            prev_period: 0,
            prev_min_diff: 0.0,
            mono: Vec::with_capacity(2 * max_period),
            mono_coarse: Vec::with_capacity(2 * max_period / amdf_skip + 2),
            owed: 0,
            out: Vec::with_capacity(out_frames * ch),
            in_stage: Vec::with_capacity(BYPASS_CHUNK * ch * 4),
            out_f32: Vec::with_capacity(out_frames * ch * 4),
            out_pcm: Vec::with_capacity(out_frames * ch * 4),
            caps: [0; 7],
        }
        .with_recorded_caps()
    }

    fn with_recorded_caps(mut self) -> Self {
        self.caps = self.capacities();
        self
    }

    fn capacities(&self) -> [usize; 7] {
        [
            self.buf.capacity(),
            self.mono.capacity(),
            self.mono_coarse.capacity(),
            self.out.capacity(),
            self.in_stage.capacity(),
            self.out_f32.capacity(),
            self.out_pcm.capacity(),
        ]
    }

    fn assert_no_growth(&self) {
        debug_assert_eq!(
            self.capacities(),
            self.caps,
            "audiostretch: a round grew one of the engine's fixed buffers, \
             [buf, mono, mono_coarse, out, in_stage, out_f32, out_pcm]"
        );
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.start = 0;
        self.copy_remaining = 0;
        self.prev_period = 0;
        self.prev_min_diff = 0.0;
        self.owed = 0;
        self.out.clear();
    }

    fn buffered_frames(&self) -> usize {
        (self.buf.len() - self.start) / self.channels
    }

    fn free_frames(&self) -> usize {
        (self.buf.capacity() - self.buf.len()) / self.channels
    }

    fn compact(&mut self) {
        if self.start == 0 {
            return;
        }
        let keep = self.buf.len() - self.start;
        self.buf.copy_within(self.start.., 0);
        self.buf.truncate(keep);
        self.start = 0;
    }

    fn append(&mut self, bytes: &[u8]) -> Option<usize> {
        let len = converted_len(self.fmt.format, SampleFormat::F32, self.channels, bytes.len())?;
        if len > self.in_stage.capacity() {
            return None;
        }
        self.in_stage.clear();
        self.in_stage.resize(len, 0);
        convert_interleaved(
            self.fmt.format,
            SampleFormat::F32,
            self.channels,
            bytes,
            &mut self.in_stage,
        )?;
        debug_assert!(len / 4 <= self.buf.capacity() - self.buf.len(), "engine buf would grow");
        for word in self.in_stage.as_chunks::<4>().0 {
            self.buf.push(f32::from_ne_bytes(*word));
        }
        Some(len / 4 / self.channels)
    }

    fn find_pitch_period(&mut self) -> usize {
        let ch = self.channels;
        let input = &self.buf[self.start..];
        self.mono.clear();
        self.mono.extend(
            input.chunks_exact(ch).take(2 * self.max_period).map(|f| f.iter().sum::<f32>()),
        );

        let energy: f32 = self.mono.iter().map(|s| s * s).sum();
        if energy < 1e-6 && self.prev_period != 0 {
            return self.prev_period;
        }

        let skip = self.amdf_skip;
        self.mono_coarse.clear();
        self.mono_coarse.extend(self.mono.iter().step_by(skip));

        let amdf = |signal: &[f32], lag: usize| -> f32 {
            let n = lag.min(signal.len().saturating_sub(lag));
            if n == 0 {
                return f32::MAX;
            }
            let diff: f32 =
                signal[..n].iter().zip(&signal[lag..lag + n]).map(|(a, b)| (a - b).abs()).sum();
            diff / n as f32
        };

        let (lo, hi) = (self.min_period / skip, self.max_period / skip);
        let mut best = lo.max(1);
        let mut best_diff = f32::MAX;
        for lag in lo.max(1)..=hi {
            let d = amdf(&self.mono_coarse, lag);
            if d < best_diff {
                best_diff = d;
                best = lag;
            }
        }

        let center = best * skip;
        let lo = center.saturating_sub(2 * skip).max(self.min_period);
        let hi = (center + 2 * skip).min(self.max_period);
        let mut best = lo;
        let mut best_diff = f32::MAX;
        for lag in lo..=hi {
            let d = amdf(&self.mono, lag);
            if d < best_diff {
                best_diff = d;
                best = lag;
            }
        }

        let chosen = if self.prev_period != 0 && best_diff > self.prev_min_diff {
            self.prev_period
        } else {
            best
        };
        self.prev_period = best;
        // A non-finite score from NaN input would freeze the hysteresis.
        self.prev_min_diff = if best_diff.is_finite() { best_diff } else { f32::MAX };
        chosen.clamp(1, self.max_period)
    }

    fn overlap_add(&mut self, frames: usize, from: usize, to: usize) {
        let ch = self.channels;
        let base = self.start;
        // Unreachable by the splice schedule, a guard so a change degrades to a short crossfade.
        let avail = self.buffered_frames().saturating_sub(from.max(to));
        let frames = frames.min(avail);
        for i in 0..frames {
            let t = i as f32 / frames as f32;
            for c in 0..ch {
                let a = self.buf[base + (from + i) * ch + c];
                let b = self.buf[base + (to + i) * ch + c];
                self.out.push(a * (1.0 - t) + b * t);
            }
        }
    }

    fn pass_through(&mut self, frames: usize) {
        let n = frames * self.channels;
        let s = self.start;
        self.out.extend_from_slice(&self.buf[s..s + n]);
        self.consume(frames);
    }

    fn consume(&mut self, frames: usize) {
        self.start += frames * self.channels;
    }

    fn generate(&mut self, rate: Rate, eos: bool) -> bool {
        let (num, den) = (i64::from(rate.num()), i64::from(rate.den()));
        let buffered = self.buffered_frames();

        if num == den {
            if buffered == 0 {
                return false;
            }
            self.pass_through(buffered.min(BYPASS_CHUNK));
            self.copy_remaining = 0;
            return true;
        }

        if self.copy_remaining > 0 {
            let want = self.copy_remaining.min(BYPASS_CHUNK);
            if buffered < want && !eos {
                return false;
            }
            let frames = buffered.min(self.copy_remaining).min(BYPASS_CHUNK);
            if frames == 0 {
                self.copy_remaining = 0;
                return true;
            }
            self.pass_through(frames);
            self.copy_remaining -= frames;
            return true;
        }

        if buffered < self.window {
            if !eos || buffered == 0 {
                return false;
            }
            self.pass_through(buffered);
            return true;
        }

        let period = self.find_pitch_period();
        let p = period as i64;
        let frames = |owed: i64, per: i64| (owed.max(0) / per) as usize;
        if num > den {
            if num >= 2 * den {
                let new_frames = frames(self.owed + p * den, num - den).clamp(1, period);
                self.overlap_add(new_frames, 0, period);
                self.consume(period + new_frames);
            } else {
                self.copy_remaining = frames(self.owed + p * (2 * den - num), num - den);
                self.overlap_add(period, 0, period);
                self.consume(2 * period);
            }
        } else {
            let (ch, s) = (self.channels, self.start);
            self.out.extend_from_slice(&self.buf[s..s + period * ch]);
            if 2 * num < den {
                let new_frames = frames(p * num - self.owed, den - num).clamp(1, period);
                self.overlap_add(new_frames, period, 0);
                self.consume(new_frames);
            } else {
                self.copy_remaining = frames(p * (2 * num - den) - self.owed, den - num);
                self.overlap_add(period, period, 0);
                self.consume(period);
            }
        }
        true
    }

    fn encode_round(&mut self) -> Result<(), K> {
        self.out_f32.clear();
        for s in &self.out {
            self.out_f32.extend_from_slice(&s.to_ne_bytes());
        }
        let len =
            converted_len(SampleFormat::F32, self.fmt.format, self.channels, self.out_f32.len())
                .ok_or(K::RaggedFrame { len: self.out_f32.len() })?;
        self.out_pcm.clear();
        self.out_pcm.resize(len, 0);
        convert_interleaved(
            SampleFormat::F32,
            self.fmt.format,
            self.channels,
            &self.out_f32,
            &mut self.out_pcm,
        )
        .ok_or(K::Convert { from: SampleFormat::F32, to: self.fmt.format })?;
        Ok(())
    }
}

/// Pitch-preserving playback-rate change.
pub struct AudioStretch {
    rate: Rate,
    asked: Rate,
    input: Option<AudioFormat>,
    fed: bool,
    layout: Option<u64>,
    refused: Option<AudioFormat>,
    engine: Option<Engine>,
    in_stride: usize,
    carry: Vec<u8>,
    next_in: Option<u64>,
    restate: bool,
    resyncs: u64,
}

/// Whether the element plays `rate`, [`RATE_MIN`] to [`RATE_MAX`].
pub fn rate_supported(rate: Rate) -> bool {
    let (num, den) = (u32::from(rate.num()), u32::from(rate.den()));
    num * u32::from(RATE_MIN.1) >= den * u32::from(RATE_MIN.0)
        && num * u32::from(RATE_MAX.1) <= den * u32::from(RATE_MAX.0)
}

fn rate_from_text(text: &str) -> Option<Rate> {
    let (whole, part) = text.split_once('.').unwrap_or((text, ""));
    let digits = whole.len() + part.len();
    if digits == 0 || digits > 9 || !whole.bytes().chain(part.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num = whole.bytes().chain(part.bytes()).fold(0i64, |n, b| n * 10 + i64::from(b - b'0'));
    Rate::new(num, 10i64.pow(part.len() as u32)).ok()
}

impl AudioStretch {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    /// A stretcher at `rate` (one is bypass), its format discovered from upstream caps.
    // Cold path, once at construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new(rate: Rate) -> Self {
        let rate = if rate_supported(rate) { rate } else { Rate::ONE };
        Self {
            rate,
            asked: rate,
            input: None,
            layout: None,
            refused: None,
            fed: false,
            engine: None,
            in_stride: 0,
            carry: Vec::new(),
            next_in: None,
            restate: true,
            resyncs: 0,
        }
    }

    /// A stretcher with the input format pinned at construction.
    pub fn with_input(input: AudioFormat, rate: Rate) -> Self {
        let mut s = Self::new(rate);
        s.set_input(input);
        s
    }

    pub fn rate(&self) -> Rate {
        self.rate
    }

    pub fn input_format(&self) -> Option<AudioFormat> {
        self.input
    }

    fn rate_from_value(ctx: &Ctx, v: Value) -> Option<Rate> {
        match v {
            Value::Int(n) => Rate::new(n, 1).ok(),
            Value::Rat(n, d) => Rate::new(i64::from(n), i64::from(d)).ok(),
            Value::Id(id) => ctx.value_name(id).and_then(rate_from_text),
        }
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
        self.carry.reserve(self.in_stride.max(1));
        self.engine = Some(Engine::new(input));
    }

    fn unlearn(&mut self) {
        self.input = None;
        self.engine = None;
        self.refused = None;
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
                rate = rate,
                channels = channels,
                sample = fmt.caps_name(),
                speed_milli = u32::from(self.rate.num()) * 1000 / u32::from(self.rate.den())
            );
            self.announce_output(ctx);
        }
    }

    fn announce_output(&self, ctx: &mut Ctx) {
        let Some(inp) = self.input else { return };
        announce_raw(ctx, Self::OUT, inp, self.layout);
    }

    fn sample_rate(&self) -> u32 {
        self.input.map_or(0, |f| f.sample_rate)
    }

    fn head(&self) -> Option<u64> {
        let buffered = self.engine.as_ref().map_or(0, Engine::buffered_frames) as u64;
        self.next_in.map(|next| next.saturating_sub(buffered))
    }

    fn stamp(&self, index: Option<u64>) -> Timestamp {
        index.map_or(Timestamp::NONE, |index| time_at(index, self.sample_rate()))
    }

    fn is_idle(&self) -> bool {
        self.carry.is_empty() && self.engine.as_ref().is_none_or(|e| e.buffered_frames() == 0)
    }

    fn place(&mut self, ctx: &mut Ctx, pts: Timestamp) -> Result<(), Error> {
        let rate = self.sample_rate();
        let at = pts.is_some().then(|| position_at(pts, rate));
        let slack = u64::from(rate / BREAK_TOLERANCE_HZ);
        match (self.next_in, at) {
            (Some(next), Some(at)) if next.abs_diff(at) > slack => {
                if self.resyncs == 0 {
                    log!(&*ctx, Level::Warn, "resync", expected = next, at = at);
                }
                self.resyncs += 1;
                self.drain(ctx, true)?;
                self.carry.clear();
                if let Some(engine) = self.engine.as_mut() {
                    engine.reset();
                }
                self.next_in = Some(at);
            }
            (None, Some(_)) => self.next_in = at,
            _ => {}
        }
        if self.restate && self.next_in.is_some() {
            self.restate = false;
            ctx.retime(self.stamp(self.head()), self.rate);
        }
        Ok(())
    }

    fn switch(&mut self, ctx: &mut Ctx) {
        if self.asked == self.rate {
            return;
        }
        let mut ahead = 0;
        if let Some(engine) = self.engine.as_mut() {
            let per = i64::from(self.rate.num()) - i64::from(self.rate.den());
            if self.asked == Rate::ONE && per != 0 {
                ahead = (engine.owed / per).max(0);
                engine.owed -= ahead * per;
            }
            engine.owed = engine.owed * i64::from(self.asked.num()) / i64::from(self.rate.num());
            engine.copy_remaining = 0;
        }
        log!(
            &*ctx,
            Level::Debug,
            "rate_switch",
            num = self.asked.num(),
            den = self.asked.den(),
            ahead = ahead
        );
        self.rate = self.asked;
        if !self.restate {
            ctx.retime(self.stamp(self.head().map(|head| head + ahead as u64)), self.rate);
        }
    }

    fn feed(&mut self, ctx: &mut Ctx, bytes: &[u8]) -> Result<(), Error> {
        let stride = self.in_stride;
        if stride == 0 {
            return Err(failed(ctx, DESC.name, K::InputStrideUnknown));
        }
        debug_assert_eq!(bytes.len() % stride, 0, "feed takes whole frames");
        let mut off = 0;
        while off < bytes.len() {
            self.drain(ctx, false)?;
            let taken = {
                let Some(engine) = self.engine.as_mut() else {
                    return Err(failed(ctx, DESC.name, K::NoEngine));
                };
                if engine.free_frames() < BYPASS_CHUNK {
                    engine.compact();
                }
                let room = engine.free_frames().min(BYPASS_CHUNK);
                if room == 0 {
                    return Err(failed(ctx, DESC.name, K::NoRoom));
                }
                let take = room.min((bytes.len() - off) / stride);
                let from = engine.fmt.format;
                engine.append(&bytes[off..off + take * stride]).ok_or_else(|| {
                    failed(ctx, DESC.name, K::Convert { from, to: SampleFormat::F32 })
                })?;
                take
            };
            self.next_in = self.next_in.map(|next| next + taken as u64);
            off += taken * stride;
        }
        self.drain(ctx, false)
    }

    fn drain(&mut self, ctx: &mut Ctx, eos: bool) -> Result<(), Error> {
        loop {
            self.switch(ctx);
            let (rate, head) = (self.rate, self.head());
            let Some(engine) = self.engine.as_mut() else {
                return Ok(());
            };
            engine.out.clear();
            let start = engine.start;
            let progressed = engine.generate(rate, eos);
            engine.assert_no_growth();
            if !progressed {
                return Ok(());
            }
            let taken = (engine.start - start) / engine.channels;
            let made = engine.out.len() / engine.channels;
            engine.owed +=
                taken as i64 * i64::from(rate.den()) - made as i64 * i64::from(rate.num());
            self.flush_round(ctx, head, taken)?;
        }
    }

    fn flush_round(&mut self, ctx: &mut Ctx, head: Option<u64>, taken: usize) -> Result<(), Error> {
        let pcm = {
            let Some(engine) = self.engine.as_mut() else {
                return Ok(());
            };
            if engine.out.is_empty() {
                return Ok(());
            }
            engine.encode_round().map_err(|kind| failed(ctx, DESC.name, kind))?;
            engine.assert_no_growth();
            std::mem::take(&mut engine.out_pcm)
        };
        let r = self.push_pcm(ctx, &pcm, head, taken);
        if let Some(engine) = self.engine.as_mut() {
            engine.out_pcm = pcm;
            engine.out_pcm.clear();
        }
        r
    }

    fn push_pcm(
        &mut self,
        ctx: &mut Ctx,
        bytes: &[u8],
        head: Option<u64>,
        taken: usize,
    ) -> Result<(), Error> {
        let stride = self.in_stride;
        if stride == 0 {
            return Err(failed(ctx, DESC.name, K::OutputStrideUnknown));
        }
        let made = (bytes.len() / stride).max(1) as u64;
        let at = |off: usize| head.map(|head| head + (off / stride) as u64 * taken as u64 / made);
        let mut off = 0;
        while off < bytes.len() {
            let Some(mut buf) = ctx.alloc_exact(bytes.len() - off) else {
                return Err(ctx.exact_refused(bytes.len() - off));
            };
            let cap = buf.memory.capacity();
            let frames = cap / stride;
            if frames == 0 {
                return Err(failed(ctx, DESC.name, K::SlotTooSmall { capacity: cap, stride }));
            }
            let n = (frames * stride).min(bytes.len() - off);
            buf.memory.as_mut_full()[..n].copy_from_slice(&bytes[off..off + n]);
            buf.memory.set_len(n);
            buf.pts = self.stamp(at(off));
            buf.duration = self.stamp(at(off + n)).saturating_sub(buf.pts);
            ctx.out(Self::OUT).push(buf);
            off += n;
        }
        Ok(())
    }

    fn asked_for(&mut self, ctx: &mut Ctx, rate: Rate) {
        if rate_supported(rate) {
            log!(&*ctx, Level::Debug, "rate_asked", num = rate.num(), den = rate.den());
            self.asked = rate;
            return;
        }
        let kind = K::UnsupportedRate { num: rate.num(), den: rate.den() };
        ctx.post(BusMessage::Warning {
            element: ctx.element(),
            error: failed(ctx, DESC.name, kind),
        });
        self.switch(ctx);
        ctx.retime(self.stamp(self.head()), self.rate);
    }
}

const BREAK_TOLERANCE_HZ: u32 = 1000;

impl Element for AudioStretch {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.claim_timeline()?;
        let parked = ctx.prop("rate").and_then(|v| Self::rate_from_value(ctx, v));
        if let Some(rate) = parked.filter(|rate| rate_supported(*rate)) {
            (self.rate, self.asked) = (rate, rate);
        }
        self.announce_output(ctx);
        self.fed = false;
        self.learn_from_sink(ctx);
        self.carry.clear();
        (self.next_in, self.restate, self.resyncs) = (None, true, 0);
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
            self.place(ctx, buf.pts)?;
            if self.is_idle() {
                self.switch(ctx);
            }

            let len = buf.memory.data().len();
            if self.rate == Rate::ONE && self.is_idle() && len % self.in_stride == 0 {
                self.next_in = self.next_in.map(|next| next + (len / self.in_stride) as u64);
                ctx.out(Self::OUT).push(buf);
                continue;
            }

            let mut data = buf.memory.data();
            if !self.carry.is_empty() {
                let need = self.in_stride - self.carry.len();
                let take = need.min(data.len());
                self.carry.extend_from_slice(&data[..take]);
                data = &data[take..];
                if self.carry.len() < self.in_stride {
                    continue; // still short of a frame
                }
                let frame = std::mem::take(&mut self.carry);
                let r = self.feed(ctx, &frame);
                self.carry = frame;
                self.carry.clear();
                r?;
            }
            let whole = data.len() - (data.len() % self.in_stride);
            self.feed(ctx, &data[..whole])?;
            self.carry.extend_from_slice(&data[whole..]);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::RateAsked { rate } => self.asked_for(ctx, *rate),
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", carry = self.carry.len());
                self.carry.clear();
                if let Some(engine) = self.engine.as_mut() {
                    engine.reset();
                }
                (self.next_in, self.restate, self.rate) = (None, true, self.asked);
            }
            Event::Eos => {
                self.drain(ctx, true)?;
                if !self.carry.is_empty() {
                    log!(&*ctx, Level::Warn, "eos_partial_frame", bytes = self.carry.len());
                }
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    next_in = self.next_in.unwrap_or(0),
                    resyncs = self.resyncs
                );
                self.carry.clear();
            }
            Event::FormatChange(_) => {
                log!(&*ctx, Level::Debug, "format_change", fed = self.fed);
                if !self.fed {
                    self.unlearn();
                }
                self.learn_from_sink(ctx);
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
        self.engine = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(num: i64, den: i64) -> Rate {
        Rate::new(num, den).unwrap()
    }

    #[test]
    fn a_rate_outside_the_range_is_not_played_and_starts_the_element_at_one() {
        assert_eq!(AudioStretch::new(rate(3, 2)).rate(), rate(3, 2));
        assert_eq!(AudioStretch::new(RATE_MIN_RATE()).rate(), rate(1, 4));
        assert_eq!(AudioStretch::new(rate(4, 1)).rate(), rate(4, 1));
        assert_eq!(AudioStretch::new(rate(1, 5)).rate(), Rate::ONE);
        assert_eq!(AudioStretch::new(rate(401, 100)).rate(), Rate::ONE);
        for (num, den, plays) in [(1, 4, true), (4, 1, true), (249, 1000, false), (64, 1, false)] {
            assert_eq!(rate_supported(rate(num, den)), plays, "{num}/{den}");
        }
    }

    #[allow(non_snake_case)]
    fn RATE_MIN_RATE() -> Rate {
        rate(i64::from(RATE_MIN.0), i64::from(RATE_MIN.1))
    }

    #[test]
    fn decimal_text_is_a_rate_digit_by_digit_and_never_through_a_float() {
        assert_eq!(rate_from_text("1.5"), Some(rate(3, 2)));
        assert_eq!(rate_from_text("2"), Some(rate(2, 1)));
        assert_eq!(rate_from_text("0.75"), Some(rate(3, 4)));
        assert_eq!(rate_from_text("1.1"), Some(rate(11, 10)), "eleven tenths, not 1.10000002");
        assert_eq!(rate_from_text(".5"), Some(rate(1, 2)));
        assert_eq!(rate_from_text("1."), Some(Rate::ONE));
        for bad in ["", ".", "1.5x", "-1", "1e3", "1.2.3", "0", "0.0", "1234567890", " 1"] {
            assert_eq!(rate_from_text(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn engine_geometry_matches_the_original() {
        let e = Engine::new(AudioFormat::new(48_000, 2, SampleFormat::F32));
        assert_eq!(e.min_period, 48_000 / 400);
        assert_eq!(e.max_period, 48_000 / 65);
        assert_eq!(e.amdf_skip, 48_000 / 4_000);
        assert_eq!(e.window, 3 * e.max_period);
    }

    const HZ: usize = 8_000;

    fn voiced(frames: usize, from: usize) -> Vec<u8> {
        let sample = |n: usize| {
            let phase = (n * 140 % HZ) as f32 / HZ as f32;
            let wobble = ((n / (HZ / 10)) % 7) as f32 * 0.05;
            (1.0 - 2.0 * phase) * (0.5 + wobble)
        };
        (from..from + frames).flat_map(|n| sample(n).to_ne_bytes()).collect()
    }

    fn worst_offset(rates: &[Rate], seconds: usize, every: usize) -> (i64, usize) {
        let mut e = Engine::new(AudioFormat::new(HZ as u32, 1, SampleFormat::F32));
        let (mut fed, mut rounds, mut at, mut worst) = (0, 0usize, 0, 0i64);
        let mut playing = rates[0];
        while fed < seconds * HZ {
            if e.free_frames() < BYPASS_CHUNK {
                e.compact();
            }
            let take = e.free_frames().min(BYPASS_CHUNK);
            e.append(&voiced(take, fed)).unwrap();
            fed += take;
            loop {
                let next = rates[(rounds / every) % rates.len()];
                if next != playing {
                    e.owed = e.owed * i64::from(next.num()) / i64::from(playing.num());
                    (e.copy_remaining, playing, at) = (0, next, at + 1);
                }
                e.out.clear();
                let start = e.start;
                if !e.generate(playing, false) {
                    break;
                }
                let (taken, made) = ((e.start - start) as i64, e.out.len() as i64);
                e.owed += taken * i64::from(playing.den()) - made * i64::from(playing.num());
                worst = worst.max((e.owed / i64::from(playing.num())).abs());
                rounds += 1;
            }
        }
        (worst, at)
    }

    #[test]
    fn the_output_is_never_more_than_a_splice_from_the_rate_however_long_it_runs() {
        let period = (HZ / MIN_PITCH_HZ) as i64;
        for (num, den) in
            [(3, 2), (3, 4), (2, 1), (4, 1), (1, 2), (1, 4), (11, 10), (99, 100), (7, 3)]
        {
            let (short, _) = worst_offset(&[rate(num, den)], 5, usize::MAX);
            assert!(short <= 2 * period, "{num}/{den} is {short} frames off");
            if [(3, 2), (3, 4), (4, 1), (1, 4)].contains(&(num, den)) {
                let (long, _) = worst_offset(&[rate(num, den)], 300, usize::MAX);
                assert!(long <= short.max(period), "{num}/{den} grew from {short} to {long}");
            }
        }
    }

    #[test]
    fn a_hundred_changes_of_rate_leave_no_more_offset_than_one() {
        let period = (HZ / MIN_PITCH_HZ) as i64;
        let rates = [rate(3, 2), Rate::ONE, rate(3, 4), rate(2, 1), rate(1, 2), rate(5, 4)];
        let (worst, changes) = worst_offset(&rates, 60, 5);
        assert!(changes > 100, "{changes} changes");
        assert!(worst <= 2 * period, "{worst} frames off after {changes} changes");
    }
}
