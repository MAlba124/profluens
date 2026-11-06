//! `flacdec`, the incremental FLAC decoder element. The PCM format comes from STREAMINFO and
//! is announced at runtime through [`Ctx::announce_format`].

use profluens_audio::bytes_in::ByteIn;
use profluens_audio::{DELAY_FIELDS, Place, Rebase};
use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::{Event, TagList};
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::pipeline::{Index, IndexEntry};
use profluens_core::time::Timestamp;

use pf_tags::flacmeta;

use crate::decoder::StreamDecoder;
use crate::error::FlacError;
use crate::index::{Grown, samples_ns, table_entries};

const FAMILY: &str = "audio/raw";
const F_RATE: &str = "rate";
const F_CHANNELS: &str = "channels";
const F_SAMPLE: &str = "sample";

/// Small so the first audio reaches the sink after about one frame's decode.
const FRAMES_PER_BATCH: u32 = 2;

static SAMPLE_VALUES: [ValueDesc; 4] =
    [ValueDesc::Id("s8"), ValueDesc::Id("s16"), ValueDesc::Id("s24"), ValueDesc::Id("s32")];

static SRC_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: F_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: F_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: F_SAMPLE, allowed: ConstraintDesc::Set(&SAMPLE_VALUES), preferred: None },
];
// `bytes` second so a byte sink like `filesink` still links.
static SRC_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: FAMILY, fields: &SRC_FIELDS }, OfferDesc::any("bytes")];
// `flac` from a demuxer or `bytes` from a file source. Naming `delay` lets it read a demuxer's.
static SINK_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: "flac", fields: &DELAY_FIELDS }, OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
pub(crate) static SRCS: [PadDesc; 1] = [PadDesc {
    name: "src",
    offers: &SRC_OFFERS, // format is data-dependent, announced at runtime
}];

// Boxed once at registry or parse time.
#[allow(clippy::disallowed_methods)]
pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "flacdec",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(FlacDec::new())),
    io: false,
    couplings: &[],
};

/// The narrowest of 8, 16, 24 and 32 bits that holds `bits`, with its name, bytes and the
/// left shift to full scale.
fn container(bits: u32) -> (&'static str, usize, u32) {
    let (name, bytes) = match bits {
        0..=8 => ("s8", 1),
        9..=16 => ("s16", 2),
        17..=24 => ("s24", 3),
        _ => ("s32", 4),
    };
    (name, bytes, (bytes as u32 * 8).saturating_sub(bits))
}

/// What carries the FLAC bytes. The Ogg mapping prefixes the first packet with 9 bytes.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Framing {
    #[default]
    Native,
    OggHead,
    OggBody,
    /// The headers again, as a demuxer reading from byte 0 behind a seek sends them, dropped up
    /// to the first frame.
    OggAgain,
}

#[derive(Default)]
pub struct FlacDec {
    dec: StreamDecoder,
    bytes_per_sample: usize,
    shift: u32,
    /// Bounded to one frame, no frame is pulled until it drains.
    pending: Vec<i32>,
    pending_pos: usize,
    rate: u32,
    channels: usize,
    at: Place,
    rebase: Rebase,
    indexing: Option<Option<u64>>,
    index: Indexed,
    input: ByteIn,
    /// Decoder errors already logged, the first is a Warn and the rest a count at EOS.
    errors: u64,
    framing: Framing,
}

#[derive(Default)]
enum Indexed {
    #[default]
    Off,
    Head,
    Early,
    Grows(Grown),
    Done,
}

impl FlacDec {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self::default()
    }

    /// Takes FLAC-in-Ogg packets, for `oggflacdec`.
    pub(crate) fn ogg() -> Self {
        Self { framing: Framing::OggHead, ..Self::default() }
    }

    /// Publish the file's seek index. `len` is the file's.
    pub fn indexing(mut self, len: Option<u64>) -> Self {
        self.indexing = Some(len);
        self
    }

    fn publish(
        &self,
        ctx: &mut Ctx,
        rate: u32,
        total: u64,
        entries: Vec<IndexEntry>,
        complete: bool,
        say: bool,
    ) {
        let ns = (total > 0).then(|| samples_ns(total, rate)).flatten();
        if let (true, Some(ns)) = (say, ns) {
            ctx.set_duration(ns);
        }
        let duration = ns.map_or(Timestamp::NONE, Timestamp);
        ctx.publish_index(Index {
            entries,
            file_len: self.indexing.flatten(),
            duration,
            complete,
            ..Index::default()
        });
    }

    // An empty Vec allocates nothing.
    #[allow(clippy::disallowed_methods)]
    fn index_head(&mut self, ctx: &mut Ctx) {
        if !matches!(self.index, Indexed::Head) || self.dec.info().is_some() {
            return;
        }
        let head = self.dec.buffered();
        if flacmeta::audio_start(head).is_some() {
            return;
        }
        if let Some(si) = flacmeta::stream_info(head) {
            log!(&*ctx, Level::Debug, "index_early", total = si.total_samples);
            self.publish(ctx, si.sample_rate, si.total_samples, Vec::new(), false, true);
            self.index = Indexed::Early;
        }
    }

    // An empty Vec allocates nothing.
    #[allow(clippy::disallowed_methods)]
    fn index_table(&mut self, ctx: &mut Ctx) {
        let early = match self.index {
            Indexed::Head => false,
            Indexed::Early => true,
            _ => return,
        };
        let (Some(info), Some(start)) = (self.dec.info().copied(), self.dec.audio_start()) else {
            return;
        };
        let entries =
            table_entries(self.dec.seek_table(), start, info.sample_rate, self.indexing.flatten());
        log!(&*ctx, Level::Debug, "index_table", entries = entries.len(), early = early);
        if !entries.is_empty() {
            self.publish(ctx, info.sample_rate, info.total_samples, entries, true, !early);
            self.index = Indexed::Done;
            return;
        }
        if !early {
            self.publish(ctx, info.sample_rate, info.total_samples, Vec::new(), false, true);
        }
        self.index = Indexed::Grows(Grown::default());
    }

    fn index_frame(&mut self, ctx: &mut Ctx) {
        let Indexed::Grows(grown) = &mut self.index else {
            return;
        };
        let Some((sample, byte)) = self.dec.last_frame() else {
            return;
        };
        let Some(time) = samples_ns(sample, self.rate) else {
            return;
        };
        if grown.frame(time, byte) {
            self.publish_grown(ctx);
        }
    }

    fn publish_grown(&mut self, ctx: &mut Ctx) {
        let Indexed::Grows(grown) = &mut self.index else {
            return;
        };
        if !grown.unpublished() {
            return;
        }
        grown.published = grown.entries.len();
        log!(&*ctx, Level::Trace, "index_grown", entries = grown.published);
        let entries = grown.entries.clone();
        let (rate, total) = self.dec.info().map_or((0, 0), |i| (i.sample_rate, i.total_samples));
        self.publish(ctx, rate, total, entries, false, false);
    }

    /// Emit the carry into pool buffers. Bounded, it returns `Ok(false)` when the pool is full.
    /// Unbounded never fails, for the tail at EOS.
    fn emit_pending(&mut self, ctx: &mut Ctx, bounded: bool) -> Result<bool, Error> {
        let _ = self.skip_before_point(ctx);
        let (bytes, shift) = (self.bytes_per_sample, self.shift);
        while self.pending_pos < self.pending.len() {
            debug_assert!(bytes > 0, "flacdec: emit before header decoded");
            let mut buf = if bounded {
                match ctx.try_alloc() {
                    Some(b) => b,
                    None => return Ok(false), // pool full, backpressure
                }
            } else {
                ctx.alloc()
            };
            let per = (buf.memory.capacity() / bytes).max(1); // whole samples per buffer
            let end = (self.pending_pos + per).min(self.pending.len());
            let dst = buf.memory.as_mut_full();
            let mut n = 0;
            for &s in &self.pending[self.pending_pos..end] {
                dst[n..n + bytes].copy_from_slice(&(s << shift).to_le_bytes()[..bytes]);
                n += bytes;
            }
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

    /// Drop the carry before the presentation point. Returns whether anything is left.
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

    /// A FLAC frame has no codec delay (RFC 9639 section 9), so a packet stamp is its first sample.
    fn frame_ready(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.pending_pos = 0;
        self.take_info(ctx)?; // first frame, so the header is known
        self.index_table(ctx);
        self.index_frame(ctx);
        if let Some(at) = self.rebase.place_first(ctx, Self::IN, self.rate) {
            log!(&*ctx, Level::Debug, "place", position = at.position, lead = at.lead);
            self.at = at;
        }
        Ok(())
    }

    fn pts_now(&self) -> Timestamp {
        if self.rate == 0 {
            return Timestamp::NONE;
        }
        Timestamp::from_nanos(
            (u128::from(self.at.position) * 1_000_000_000 / u128::from(self.rate)) as u64,
        )
    }

    fn take_info(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.bytes_per_sample != 0 {
            return Ok(());
        }
        let info = self
            .dec
            .info()
            .ok_or_else(|| Error::element_failed(ctx.element(), FlacError::NoStreamInfo))?;
        let (name, bytes, shift) = container(info.bits_per_sample);
        log!(
            &*ctx,
            Level::Info,
            "streaminfo",
            rate = info.sample_rate,
            channels = info.channels,
            bits = info.bits_per_sample,
            total = info.total_samples
        );
        (self.bytes_per_sample, self.shift) = (bytes, shift);
        self.rate = info.sample_rate;
        self.channels = info.channels as usize;
        ctx.announce_format(
            Self::OUT,
            FAMILY,
            &[
                (F_RATE, ValueDesc::Int(info.sample_rate as i64)),
                (F_CHANNELS, ValueDesc::Int(info.channels as i64)),
                (F_SAMPLE, ValueDesc::Id(name)),
            ],
        );
        self.emit_tags(ctx);
        Ok(())
    }

    fn emit_tags(&mut self, ctx: &mut Ctx) {
        let ft = self.dec.tags().clone();
        if ft.is_empty() {
            return;
        }
        log!(
            &*ctx,
            Level::Debug,
            "tags",
            comments = ft.comments.len(),
            pictures = ft.pictures.len()
        );
        let mut tags = TagList::new();
        for (key, value) in &ft.comments {
            tags.add(key, value);
        }
        for (mime, data) in &ft.pictures {
            // A picture past the exact cap is left out, the comments still go.
            let Some(mut buf) = ctx.alloc_exact(data.len()) else { continue };
            buf.memory.as_mut_full()[..data.len()].copy_from_slice(data);
            buf.memory.set_len(data.len());
            tags.add_picture(mime, buf.memory.clone());
        }
        let element = ctx.element();
        ctx.push_event(Self::OUT, Event::Tags(tags.clone()));
        ctx.post(BusMessage::Tags { element, tags });
    }
}

impl FlacDec {
    /// Warn on the first decoder error, the rest are counted for EOS.
    fn note_errors(&mut self, ctx: &Ctx) {
        let now = self.dec.errors();
        if now > self.errors && self.errors == 0 {
            log!(
                ctx,
                Level::Warn,
                "corrupt_skipped",
                position = self.at.position,
                rate = self.rate
            );
        }
        self.errors = now;
    }

    fn pass(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<Flow, Error> {
        if !self.emit_pending(ctx, true)? {
            // Pool full. Yield without consuming input so the sink drains.
            ctx.hold_end();
            return Ok(Flow::Ok);
        }
        let mut emitted = 0u32;
        loop {
            let pulled = self.dec.pull_into(&mut self.pending);
            self.note_errors(ctx);
            match pulled {
                Ok(Some(())) => {
                    self.frame_ready(ctx)?;
                    // Frames before the presentation point do not use up the batch.
                    let produced = self.skip_before_point(ctx);
                    if !self.emit_pending(ctx, true)? {
                        ctx.hold_end();
                        return Ok(Flow::Ok); // pool filled mid-frame, carry the rest, yield
                    }
                    emitted += u32::from(produced);
                    if emitted >= FRAMES_PER_BATCH {
                        // Frames may be buffered with no input left to wake this element.
                        ctx.hold_end();
                        return Ok(Flow::Ok);
                    }
                }
                Ok(None) => match self.input.next(ctx, inputs) {
                    Some(buf) => {
                        let mut data = buf.memory.data();
                        match self.framing {
                            Framing::OggHead => {
                                data = crate::oggflac::native(ctx, data)?;
                                self.framing = Framing::OggBody;
                            }
                            Framing::OggBody if crate::oggflac::is_mapping(data) => {
                                log!(&*ctx, Level::Debug, "heads_again", bytes = data.len());
                                self.framing = Framing::OggAgain;
                                continue;
                            }
                            Framing::OggAgain if !crate::oggflac::is_frame(data) => continue,
                            Framing::OggAgain => self.framing = Framing::OggBody,
                            Framing::Native | Framing::OggBody => {}
                        }
                        self.rebase.input(buf.pts);
                        self.dec.push(data); // `buf` recycles on drop
                        self.index_head(ctx);
                    }
                    None => return Ok(Flow::Ok),
                },
                Err(e) => return Err(Error::element_failed(ctx.element(), FlacError::Decode(e))),
            }
        }
    }
}

impl FlacDec {
    fn log_eos(&self, ctx: &Ctx) {
        let left = self.dec.buffered().len();
        if self.bytes_per_sample == 0 {
            // Nothing decoded and the stream ends without an error.
            let level =
                if left > 0 || self.dec.info().is_some() { Level::Error } else { Level::Warn };
            log!(ctx, level, "eos_before_audio", buffered = left);
            return;
        }
        if left > 0 {
            log!(ctx, Level::Warn, "truncated_tail", bytes = left);
        }
        let errors = self.dec.errors();
        let level = if errors > 0 { Level::Warn } else { Level::Info };
        log!(ctx, level, "eos", position = self.at.position, errors = errors);
    }
}

impl Element for FlacDec {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.indexing.is_some() {
            ctx.will_index();
        }
        self.dec = StreamDecoder::new();
        self.bytes_per_sample = 0;
        self.shift = 0;
        self.pending.clear();
        self.pending_pos = 0;
        self.rate = 0;
        self.channels = 0;
        self.at = Place::default();
        self.rebase = Rebase::Open;
        self.index = if self.indexing.is_some() { Indexed::Head } else { Indexed::Off };
        self.errors = 0;
        // A new run opens with the mapping packet.
        if matches!(self.framing, Framing::OggBody | Framing::OggAgain) {
            self.framing = Framing::OggHead;
        }
        self.input.start(ctx, self.indexing.flatten());
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let flow = self.pass(ctx, &mut inputs);
        self.input.ask(ctx);
        flow
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::Eos) {
            self.publish_grown(ctx);
            self.log_eos(ctx);
        }
        if matches!(event, Event::FlushStart) {
            let to_byte = ctx.seek_target().map_or(0, |t| t.to_byte);
            log!(&*ctx, Level::Debug, "flush", to_byte = to_byte);
            self.dec.seek_reset();
            self.dec.restart_at(to_byte);
            self.publish_grown(ctx);
            self.pending.clear();
            self.pending_pos = 0;
            self.rebase = Rebase::Open;
            self.at.lead = 0;
            return Ok(());
        }
        // End of stream. What is left is a truncated tail, which is dropped.
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.dec = StreamDecoder::new();
        self.bytes_per_sample = 0;
        self.shift = 0;
        self.pending.clear();
        self.pending_pos = 0;
        self.rate = 0;
        self.channels = 0;
        self.at = Place::default();
        self.rebase = Rebase::Open;
        self.index = Indexed::Off;
    }
}

#[cfg(test)]
mod tests {
    use super::container;

    #[test]
    fn every_depth_goes_out_in_the_narrowest_container_at_full_scale() {
        assert_eq!(container(8), ("s8", 1, 0));
        assert_eq!(container(12), ("s16", 2, 4));
        assert_eq!(container(16), ("s16", 2, 0));
        assert_eq!(container(20), ("s24", 3, 4));
        assert_eq!(container(24), ("s24", 3, 0));
        assert_eq!(container(32), ("s32", 4, 0));
        for bits in 4..=32 {
            let (name, bytes, shift) = container(bits);
            assert_eq!(bits + shift, bytes as u32 * 8, "{bits} bits");
            assert!(shift < 8 && name == ["s8", "s16", "s24", "s32"][bytes - 1], "{bits} bits");
            let (lo, hi) = (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
            let top = 1i64 << (bytes * 8 - 1);
            assert_eq!(
                (lo << shift, (hi << shift) + (1 << shift) - 1),
                (-top, top - 1),
                "{bits} bits"
            );
        }
    }
}
