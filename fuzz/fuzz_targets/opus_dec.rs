//! pf-opus `OpusHead`, its family 1 layout and the `opusdec` element over libopus. Input is a
//! knob byte then records, each a `u16` LE length and that many bytes, a short last record
//! taking the rest. A record is an `OpusHead`, an `OpusTags` or an Opus packet.
//! Knob bit 0 picks `f32` over `s16`, bits 1-3 the rate (0-4 an entry of `DECODE_RATES`, else
//! none stated), bit 4 declares a 312 sample container delay, bits 5-7 the batch ceiling of the
//! grouped run. Every record that parses as a head round trips through the writer here, and
//! the element puts out the same PCM fed one packet at a time as fed in batches, at most the
//! TOC's duration per packet, in whole frames of the channel count the head gives.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::hash::{DefaultHasher, Hasher};
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use pf_opus::OpusDec;
use pf_opus::head::{OPUS_HEAD_MAGIC, OPUS_HEAD_MIN_LEN, OpusHead, OpusHeadError};
use pf_opus::libopus::{DECODE_RATES, LibopusMsDecoder};
use pf_opus::multistream::{Layout, MAX_CHANNELS, canonical_order, reorder};
use pf_opus::{apply_output_gain, apply_output_gain_f32};
use pf_sim::prng::Prng;
use profluens_core::batch::Batch;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::Bus;
use profluens_core::ctx::Ctx;
use profluens_core::element::Element;
use profluens_core::event::Event;
use profluens_core::format::{FixedFormat, Value, ValueDesc, Vocabulary};
use profluens_core::id::{ElementId, FieldId, FormatId, Interner, SinkPad, SrcPad};
use profluens_core::memory::Pool;
use profluens_core::sched::{after_process, process_once};
use profluens_core::time::Timestamp;

/// 120 ms at 48 kHz, RFC 6716 section 3.2.5.
const MAX_SAMPLES: usize = 5760;
const MAX_PACKETS: usize = 48;
/// Channels past this skip the direct multistream decode, libopus state grows per stream.
const MS_CHANNELS: usize = 16;
const CEILINGS: [usize; 8] = [usize::MAX, 1, 2, 3, 4, 8, 16, 64];
const DELAY_NS: i64 = 6_500_000;

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let packets: Vec<&[u8]> = records(rest).take(MAX_PACKETS).collect();
    let mut heads = 0;
    for (i, p) in packets.iter().enumerate() {
        if let Some(h) = head(p) {
            heads += 1;
            if heads <= 2 {
                direct(&h, &packets[i + 1..]);
            }
        }
    }
    if packets.is_empty() {
        return;
    }
    let cfg = Cfg::of(knob);
    let one = run(&packets, &cfg, 1, 0);
    let grouped = run(&packets, &cfg, CEILINGS[usize::from(knob >> 5)], u64::from(knob));
    assert!(one.out == grouped.out, "batches decoded other than one packet at a time");
    assert_eq!(one.failed, grouped.failed, "an error in one feeding only");
    assert_eq!(one.channels, grouped.channels, "a channel count in one feeding only");
    check(&packets, &cfg, &one);
    if say() {
        let frames: usize = one.per_packet.iter().sum();
        eprintln!(
            "opus records {} heads {heads} channels {:?} failed {} out {} frames {frames}",
            packets.len(),
            one.channels,
            one.failed,
            one.out.len()
        );
    }
});

fn records(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let (len, rest) = match data.split_first_chunk::<2>() {
            Some((len, rest)) => (usize::from(u16::from_le_bytes(*len)), rest),
            None if data.is_empty() => return None,
            None => (data.len(), std::mem::take(&mut data)),
        };
        let (rec, tail) = rest.split_at(len.min(rest.len()));
        data = tail;
        Some(rec)
    })
}

fn say() -> bool {
    static SAY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}

fn write_head(h: &OpusHead) -> Vec<u8> {
    let mut out = OPUS_HEAD_MAGIC.to_vec();
    out.extend_from_slice(&[h.version, h.channel_count]);
    out.extend_from_slice(&h.pre_skip.to_le_bytes());
    out.extend_from_slice(&h.input_sample_rate.to_le_bytes());
    out.extend_from_slice(&h.output_gain_q7_8.to_le_bytes());
    out.push(h.mapping_family);
    if h.mapping_family != 0 {
        out.extend_from_slice(&[h.stream_count, h.coupled_count]);
        out.extend_from_slice(h.mapping());
    }
    out
}

fn is_head(p: &[u8]) -> bool {
    p.len() >= 8 && p[..8] == OPUS_HEAD_MAGIC[..]
}

fn is_audio(p: &[u8]) -> bool {
    !is_head(p) && !(p.len() >= 8 && &p[..8] == b"OpusTags")
}

/// Parses `p` and holds the result to RFC 7845 section 5.1, `None` when it is no head.
fn head(p: &[u8]) -> Option<OpusHead> {
    let got = OpusHead::parse(p);
    assert_eq!(got, OpusHead::parse(p), "a head parse is not deterministic");
    let h = match got {
        Ok(h) => h,
        Err(e) => {
            refused(p, e);
            return None;
        }
    };
    let n = usize::from(h.channel_count);
    assert!(n >= 1 && h.version >> 4 == 0, "{h:?}");
    assert_eq!(h.mapping().len(), n);
    let decoded = u16::from(h.stream_count) + u16::from(h.coupled_count);
    assert!(h.stream_count >= 1 && h.coupled_count <= h.stream_count && decoded <= 255);
    assert!(h.mapping().iter().all(|&m| m == 255 || u16::from(m) < decoded), "{h:?}");
    match h.mapping_family {
        0 => {
            assert!(n <= 2 && h.stream_count == 1 && usize::from(h.coupled_count) == n - 1);
            assert!(h.mapping().iter().enumerate().all(|(i, &m)| usize::from(m) == i));
        }
        1 => assert!(n <= MAX_CHANNELS, "{n} channels in family 1"),
        _ => {}
    }
    let raw = write_head(&h);
    assert_eq!(raw[..], p[..raw.len()], "the head writes back other than it read");
    assert_eq!(OpusHead::parse(&raw), Ok(h), "a written head reads back otherwise");
    layout(&h);
    gain(p, h.output_gain_q7_8);
    Some(h)
}

fn refused(p: &[u8], e: OpusHeadError) {
    use OpusHeadError as E;
    match e {
        E::TooShort { got, need } => assert!(got == p.len() && got < need, "{e:?}"),
        E::BadMagic => assert!(p[..8] != OPUS_HEAD_MAGIC[..]),
        E::IncompatibleVersion { version } => assert!(version >> 4 != 0 && p[8] == version),
        E::ZeroChannels => assert_eq!(p[9], 0),
        E::ChannelCountForFamily { family, channels } => {
            assert!(family <= 1 && channels == p[9] && family == p[18]);
        }
        E::ZeroStreams => assert_eq!(p[OPUS_HEAD_MIN_LEN], 0),
        E::CoupledExceedsStreams { streams, coupled } => assert!(coupled > streams),
        E::TooManyDecodedChannels { streams, coupled } => {
            assert!(u16::from(streams) + u16::from(coupled) > 255);
        }
        E::MappingIndexOutOfRange { output_channel, index, decoded_channels } => {
            assert!(index != 255 && index >= decoded_channels);
            assert_eq!(p[OPUS_HEAD_MIN_LEN + 2 + usize::from(output_channel)], index);
        }
    }
}

fn layout(h: &OpusHead) {
    let Some(l) = Layout::from_head(h) else {
        assert!(h.mapping_family != 1, "a family 1 head without a layout");
        return;
    };
    assert_eq!(h.mapping_family, 1);
    assert_eq!(
        (l.channels, l.streams, l.coupled),
        (h.channel_count, h.stream_count, h.coupled_count)
    );
    assert_eq!(l.mapping(), h.mapping());
    let n = usize::from(l.channels);
    let Some(order) = canonical_order(n) else {
        return;
    };
    let mut seen = [false; MAX_CHANNELS];
    for &o in &order[..n] {
        assert!(usize::from(o) < n && !seen[usize::from(o)], "{order:?} is no permutation");
        seen[usize::from(o)] = true;
    }
    let mut pcm: Vec<u16> = (0..2 * n as u16).collect();
    reorder(&mut pcm, n, &order);
    for (c, &o) in order[..n].iter().enumerate() {
        assert_eq!((pcm[c], pcm[n + c]), (u16::from(o), (n + usize::from(o)) as u16));
    }
}

/// Gain keeps the sign and scales away from or towards zero as its own sign says.
fn gain(p: &[u8], q: i16) {
    let src: Vec<i16> = p.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect();
    let mut s = src.clone();
    apply_output_gain(&mut s, q);
    let mut f: Vec<f32> = src.iter().map(|&v| f32::from(v) / 32768.0).collect();
    apply_output_gain_f32(&mut f, q);
    for ((&a, &b), &x) in src.iter().zip(&s).zip(&f) {
        let (a32, b32) = (i32::from(a), i32::from(b));
        assert!(a32.signum() * b32.signum() >= 0 && x.is_finite(), "{a} became {b} at {q}");
        if q > 0 {
            assert!(b32.abs() >= a32.abs().min(i32::from(i16::MAX)), "{a} became {b} at {q}");
        } else {
            assert!(b32.abs() <= a32.abs(), "{a} became {b} at {q}");
        }
    }
}

/// 48 kHz samples the TOC (RFC 6716 section 3.1) says `p` holds, `None` when unreadable.
fn duration(p: &[u8]) -> Option<usize> {
    let toc = *p.first()?;
    let config = usize::from(toc >> 3);
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][config & 3],
        12..=15 => [480, 960][config & 1],
        _ => [120, 240, 480, 960][config & 3],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => usize::from(*p.get(1)? & 0x3F),
    };
    Some(frame * frames).filter(|&n| n > 0 && n <= MAX_SAMPLES)
}

/// libopus multistream straight from the head, any family.
fn direct(h: &OpusHead, after: &[&[u8]]) {
    let n = usize::from(h.channel_count);
    if n > MS_CHANNELS {
        return;
    }
    let mut dec = match LibopusMsDecoder::new(h.stream_count, h.coupled_count, h.mapping()) {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut pcm = Vec::new();
    for p in after.iter().filter(|p| is_audio(p)).take(8) {
        let Ok(ch) = dec.decode_packet_into(p, &mut pcm) else {
            continue;
        };
        assert_eq!(usize::from(ch), n, "the multistream width is not the head's");
        assert!(pcm.len().is_multiple_of(n));
        let got = pcm.len() / n;
        assert_eq!(Some(got), duration(p), "{got} samples from a packet of {:?}", duration(p));
    }
}

struct Cfg {
    f32: bool,
    rate: Option<u32>,
    delay: bool,
}

impl Cfg {
    fn of(knob: u8) -> Cfg {
        Cfg {
            f32: knob & 1 != 0,
            rate: DECODE_RATES.get(usize::from(knob >> 1 & 7)).copied(),
            delay: knob & 0x10 != 0,
        }
    }

    fn rate(&self) -> u32 {
        self.rate.unwrap_or(48_000)
    }

    fn sample_bytes(&self) -> usize {
        if self.f32 { 4 } else { 2 }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Out {
    pts: Timestamp,
    duration: Timestamp,
    len: usize,
    hash: u64,
}

struct Ran {
    out: Vec<Out>,
    /// Frames put out on each packet's push, only meaningful one packet at a time.
    per_packet: Vec<usize>,
    channels: Option<i64>,
    rate: Option<i64>,
    failed: bool,
}

fn check(packets: &[&[u8]], cfg: &Cfg, ran: &Ran) {
    let rate = cfg.rate();
    let mut want = None;
    let mut audio_first = false;
    let mut fails = (false, false);
    for p in packets {
        if is_audio(p) {
            audio_first |= want.is_none();
            continue;
        }
        let Some(h) = is_head(p).then(|| OpusHead::parse(p).ok()).flatten() else {
            continue;
        };
        if h.mapping_family > 1 {
            fails.0 = true;
            break;
        }
        fails.1 |= h.mapping_family == 1;
        want.get_or_insert(i64::from(h.channel_count));
    }
    assert!(!fails.0 || ran.failed, "a family past 1 decoded");
    assert!(!ran.failed || fails.0 || fails.1, "the element failed on no head");
    if let Some(ch) = ran.channels {
        let fits = match want {
            Some(w) if !audio_first => ch == w,
            Some(w) => ch == w || (1..=2).contains(&ch),
            None => (1..=2).contains(&ch),
        };
        assert!(fits, "{ch} channels announced, the head says {want:?}");
        assert_eq!(ran.rate, Some(i64::from(rate)), "announced other than the rate taken");
    }
    assert!(ran.out.is_empty() || ran.channels.is_some(), "PCM before a format");
    let frame = ran.channels.unwrap_or(1) as usize * cfg.sample_bytes();
    for o in &ran.out {
        assert!(o.len > 0 && o.len.is_multiple_of(frame), "a buffer of {} bytes", o.len);
    }
    for w in ran.out.windows(2) {
        assert_eq!(w[0].pts.saturating_add(w[0].duration), w[1].pts, "stamps not contiguous");
    }
    for (p, &got) in packets.iter().zip(&ran.per_packet) {
        let most = if is_audio(p) { duration(p).unwrap_or(0) * rate as usize / 48_000 } else { 0 };
        if say() {
            let kind = if is_head(p) {
                "head"
            } else if is_audio(p) {
                "audio"
            } else {
                "tags"
            };
            eprintln!("{kind} len {} toc {:02x?} most {most} got {got}", p.len(), p.first());
        }
        assert!(got <= most, "{got} frames from a packet of {most}");
    }
}

fn run(packets: &[&[u8]], cfg: &Cfg, ceiling: usize, seed: u64) -> Ran {
    let mut rig = Rig::new(cfg);
    let mut rng = Prng::new(seed);
    let mut at = 0;
    while at < packets.len() && !rig.ran.failed {
        let n = rng.range(1, ceiling.min(packets.len() - at) as u64 + 1) as usize;
        let before = rig.frames;
        rig.push(&packets[at..at + n], at);
        if n == 1 {
            rig.ran.per_packet.push(rig.frames - before);
        }
        at += n;
    }
    if !rig.ran.failed {
        rig.eos();
    }
    rig.ran
}

struct Rig {
    el: OpusDec,
    ctx: Ctx,
    vocab: Arc<Vocabulary>,
    pool: Pool,
    _bus: Bus,
    f32: bool,
    frames: usize,
    ran: Ran,
}

impl Rig {
    fn new(cfg: &Cfg) -> Self {
        let mut v = Vocabulary {
            formats: Interner::new(),
            fields: Interner::new(),
            values: Interner::new(),
        };
        for f in ["bytes", "opus", "audio/raw"] {
            v.formats.intern(f);
        }
        for f in ["rate", "channels", "sample", "layout", "delay"] {
            v.fields.intern(f);
        }
        for s in ["s16", "f32"] {
            v.values.intern(s);
        }
        let fields: &[(&'static str, ValueDesc)] =
            if cfg.delay { &[("delay", ValueDesc::Int(DELAY_NS))] } else { &[] };
        let fixed = v.build_fixed("opus", fields).expect("an interned family");
        let vocab = Arc::new(v);
        let pool = Pool::new(16 << 10);
        let (tx, bus) = Bus::channel();
        let mut ctx = Ctx::new(pool.clone(), tx, ElementId(0), FormatId(0), 1);
        ctx.configure_pads(1, 1);
        ctx.set_vocabulary(Arc::clone(&vocab));
        ctx.set_negotiated_sink(SinkPad(0), fixed);
        let field = |n: &str| vocab.field_id(n).expect("an interned field");
        if let Some(rate) = cfg.rate {
            ctx.set_src_takes(SrcPad(0), field("rate"), &[Value::Int(i64::from(rate))]);
        }
        let sample = vocab.value_id(if cfg.f32 { "f32" } else { "s16" }).expect("a value");
        ctx.set_src_takes(SrcPad(0), field("sample"), &[Value::Id(sample)]);
        let mut el = OpusDec::new();
        el.start(&mut ctx).expect("start");
        let ran = Ran {
            out: Vec::new(),
            per_packet: Vec::new(),
            channels: None,
            rate: None,
            failed: false,
        };
        let mut rig = Self { el, ctx, vocab, pool, _bus: bus, f32: cfg.f32, frames: 0, ran };
        rig.settle();
        rig
    }

    fn push(&mut self, group: &[&[u8]], first: usize) {
        let mut batch = Batch::new(FormatId(0));
        for (i, data) in group.iter().enumerate() {
            let mut memory = self.pool.acquire_exact(data.len().max(1)).expect("exact");
            memory.as_mut_full()[..data.len()].copy_from_slice(data);
            memory.set_len(data.len());
            batch.push(Buffer {
                memory,
                pts: Timestamp::from_millis(20 * (first + i) as u64),
                dts: Timestamp::NONE,
                duration: Timestamp::from_millis(20),
                flags: BufferFlags::empty(),
                format: FormatId(0),
                sync: None,
            });
        }
        self.ctx.input_append(&mut batch);
        // An unbounded pool never holds input back, the cap only guards a stall.
        for _ in 0..=group.len() {
            if process_once(&mut self.el, &mut self.ctx, true).is_err() {
                self.ran.failed = true;
            }
            self.settle();
            if self.ran.failed || self.ctx.input_len() == 0 {
                break;
            }
        }
        assert!(self.ran.failed || self.ctx.input_len() == 0, "the element left input behind");
    }

    fn eos(&mut self) {
        self.el.event(&mut self.ctx, &Event::Eos).expect("EOS refused");
        self.settle();
    }

    fn settle(&mut self) {
        let vocab = Arc::clone(&self.vocab);
        let (ch, rate, sample) =
            (vocab.field_id("channels"), vocab.field_id("rate"), vocab.field_id("sample"));
        let want = vocab.value_id(if self.f32 { "f32" } else { "s16" });
        let mut got: Option<FixedFormat> = None;
        after_process(&mut self.ctx, &vocab, |_, _, _, f| got = Some(f));
        if let Some(f) = got {
            let int = |id: Option<FieldId>| match id.and_then(|id| f.get(id)) {
                Some(Value::Int(n)) => Some(n),
                _ => None,
            };
            assert!(self.ran.channels.is_none(), "the format announced twice");
            assert_eq!(sample.and_then(|id| f.get(id)), want.map(Value::Id), "another sample");
            self.ran.channels = int(ch);
            self.ran.rate = int(rate);
        }
        let ch = self.ran.channels.unwrap_or(1).max(1) as usize;
        let mut batch = self.ctx.take_output(SrcPad(0));
        while let Some(b) = batch.pop_front() {
            let p = b.memory.data();
            if self.f32 {
                for s in p.as_chunks::<4>().0 {
                    assert!(f32::from_le_bytes(*s).is_finite(), "a sample not finite");
                }
            }
            self.frames += p.len() / (ch * if self.f32 { 4 } else { 2 });
            let mut h = DefaultHasher::new();
            h.write(p);
            self.ran.out.push(Out {
                pts: b.pts,
                duration: b.duration,
                len: p.len(),
                hash: h.finish(),
            });
        }
    }
}
