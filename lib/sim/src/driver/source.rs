//! The fake source. Serves the declared media interleaved by pts, one tagged buffer at a time,
//! and fails the way its declaration says.

use std::sync::Arc;

use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::{Ctx, Disposition, Stream, StreamKind as Kind};
use profluens_core::element::{Element, ElementDesc, Flow};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::id::SrcPad;
use profluens_core::pipeline::{Addressed, Index, IndexEntry, Serves};
use profluens_core::time::Timestamp;

use super::feed::Feed;
use super::gop::{Mapped, Pictures};
use super::net::NetDecl;
use super::offers::{FAMILIES, FIELD, Offers, id_of};
use super::pads::{FakeError, MEDIA, SOURCE_1, SOURCE_2, SOURCE_STREAMS, supply};
use super::scenario::{MAX_STREAMS, SourceDecl};
use super::wire::{self, PAYLOAD, Tag};
use crate::media::{self, Cursor, EndKind, MediaSpec, ScheduleError, Step, StreamKind, mark};
use crate::prng::mix;
use crate::time::{MS, NONE, Ns};

const GAP_EVERY: Ns = 500 * MS;

#[derive(Clone, Copy)]
struct Lane {
    cursor: Cursor,
    next: Step,
    /// The release before `next`. The difference to the next release is a declared stall's hold.
    released: Ns,
    wait_until: Ns,
    fresh: bool,
    announced: u8,
    said: Ns,
}

enum Pick {
    Lane(usize),
    Failed(FakeError),
    Nothing,
}

#[derive(Clone, Copy)]
enum Reads {
    Time,
    Picture(Pictures, Mapped),
    Byte,
}

pub struct FakeSource {
    decl: SourceDecl,
    offers: Offers,
    spec: Arc<MediaSpec>,
    lanes: [Option<Lane>; MAX_STREAMS],
    seek_gen: u32,
    linked: [(u8, u8); MAX_STREAMS],
    reads: Reads,
    first: u8,
    served: u8,
    net: Option<Feed>,
    known: u64,
    whole: bool,
    put_out: u64,
    seed: u64,
}

impl FakeSource {
    pub fn new(
        decl: SourceDecl,
        offers: Offers,
        spec: Arc<MediaSpec>,
        seed: u64,
    ) -> Result<Self, ScheduleError> {
        let mut lanes = [None; MAX_STREAMS];
        for (stream, lane) in lanes.iter_mut().enumerate().take(decl.stream_count as usize) {
            let mut cursor = media::cursor(&spec, stream as u16, seed, 0)?;
            let next = media::step(&spec, &mut cursor);
            *lane = Some(Lane {
                cursor,
                next,
                released: 0,
                wait_until: NONE,
                fresh: true,
                announced: 0,
                said: 0,
            });
        }
        let linked = core::array::from_fn(|stream| (0, id_of(stream as u16, 0)));
        let (reads, served) = (Reads::Time, decl.stream_count.min(MAX_STREAMS as u8));
        let (known, whole, put_out) = (0, false, 0);
        Ok(Self {
            decl,
            offers,
            spec,
            lanes,
            seek_gen: 0,
            linked,
            reads,
            first: 0,
            served,
            net: None,
            known,
            whole,
            put_out,
            seed,
        })
    }

    pub fn over_net(mut self, net: NetDecl) -> Self {
        self.net = net.on.then(|| Feed::new(net));
        self
    }

    pub fn indexed(mut self, pictures: Pictures, mapped: Mapped) -> Self {
        self.reads = Reads::Picture(pictures, mapped);
        self
    }

    // One table a publication, as a demuxer republishes what it learned.
    #[allow(clippy::disallowed_methods)]
    fn publish(&self, ctx: &Ctx, pictures: &Pictures, keys: u64, complete: bool) {
        let key = |n| {
            let (time, byte) = pictures.key(n);
            IndexEntry::key(time, byte)
        };
        let duration = Timestamp(self.decl.streams[0].duration);
        let (entries, file_len) = ((0..keys).map(key).collect(), Some(pictures.count()));
        // Pictures in groups reach the group being presented, as a player's video does.
        let grouped = !pictures.spans();
        let index = Index { entries, file_len, duration, complete, grouped, ..Index::default() };
        ctx.publish_index(index);
    }

    fn learn(&mut self, ctx: &Ctx, seq: u64) {
        let Reads::Picture(pictures, mapped) = self.reads else {
            return;
        };
        self.put_out += 1;
        let keys = pictures.keys();
        match mapped {
            Mapped::Late { after } if !self.whole && self.put_out >= after as u64 => {
                self.whole = true;
                self.publish(ctx, &pictures, keys, true);
            }
            Mapped::Grows if self.known < keys && pictures.key(self.known).1 == seq => {
                self.known += 1;
                self.whole = self.known >= keys;
                self.publish(ctx, &pictures, self.known, self.whole);
            }
            _ => {}
        }
    }

    pub fn alone(mut self, stream: u8) -> Self {
        for (n, lane) in self.lanes.iter_mut().enumerate() {
            if n != stream as usize {
                *lane = None;
            }
        }
        (self.reads, self.first, self.served) = (Reads::Byte, stream, 1);
        self
    }

    fn pad_of(&self, n: usize) -> Option<SrcPad> {
        let pad = n.checked_sub(self.first as usize)?;
        (pad < self.served as usize).then_some(SrcPad(pad as u32))
    }

    fn format_of(&self, n: usize, seq: u64) -> u8 {
        match self.decl.format_change.get(n) {
            Some(change) if seq >= *change => self.offers.second[n],
            _ => self.linked[n].1,
        }
    }

    fn pick(&mut self, now: Ns, wake: &mut Ns) -> Pick {
        let mut best = (Pick::Nothing, NONE);
        for (n, lane) in self.lanes.iter_mut().enumerate() {
            let Some(lane) = lane else { continue };
            let slot = match lane.next {
                Step::Slot(slot) => slot,
                Step::End { kind: EndKind::Error, .. } => {
                    let buffer = lane.cursor.position();
                    return Pick::Failed(FakeError::DeclaredSource { stream: n as u16, buffer });
                }
                Step::End { .. } => continue,
            };
            let hold = slot.release.saturating_sub(lane.released);
            if slot.marks & mark::STALLED != 0 && hold > 0 && lane.wait_until == NONE {
                lane.wait_until = now.saturating_add(hold);
            }
            if lane.wait_until != NONE && now < lane.wait_until {
                *wake = (*wake).min(lane.wait_until);
                continue;
            }
            // A stream served ahead in the file comes as if its pts were that much less.
            let at = slot.pts.saturating_sub(self.decl.skew[n]);
            if at < best.1 || matches!(best.0, Pick::Nothing) {
                best = (Pick::Lane(n), at);
            }
        }
        best.0
    }

    fn len_of(&self, n: usize, seq: u64) -> usize {
        let extra = (PAYLOAD * (self.decl.swing.max(1) as usize - 1)) as u64;
        PAYLOAD + (mix(self.seed ^ (n as u64) << 56 ^ seq) % (extra + 1)) as usize
    }

    fn next_seq(&self, n: usize) -> Option<u64> {
        match self.lanes.get(n)?.as_ref()?.next {
            Step::Slot(slot) => Some(slot.seq),
            Step::End { .. } => None,
        }
    }

    fn say_gaps(&mut self, ctx: &mut Ctx, at: Ns) {
        if !self.decl.described {
            return;
        }
        for t in 0..MAX_STREAMS {
            let text = matches!(self.decl.streams[t].kind, StreamKind::Text { .. });
            let (Some(pad), true) = (self.pad_of(t), text) else {
                continue;
            };
            let Some(lane) = self.lanes[t].as_mut() else {
                continue;
            };
            let ahead = match lane.next {
                Step::Slot(slot) => slot.pts > at,
                Step::End { .. } => false,
            };
            // Behind its first cue, which carries its format, as a demuxer of mkv does.
            if ahead && !lane.fresh && at >= lane.said.saturating_add(GAP_EVERY) {
                ctx.push_event(pad, Event::Gap { until: Timestamp(at) });
                lane.said = at;
            }
        }
    }

    fn emit(&mut self, ctx: &mut Ctx, n: usize) -> Result<bool, Error> {
        let Some(Step::Slot(slot)) = self.lanes.get(n).and_then(|l| l.as_ref()).map(|l| l.next)
        else {
            return Ok(true);
        };
        let (format, family) = (self.format_of(n, slot.seq), self.linked[n].0);
        let len = self.len_of(n, slot.seq);
        let Some(pad) = self.pad_of(n) else {
            return Ok(true);
        };
        let Some(lane) = self.lanes.get_mut(n).and_then(Option::as_mut) else {
            return Ok(true);
        };
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(false);
        };
        if std::mem::take(&mut lane.fresh) {
            ctx.push_event(pad, Event::Segment { base: Timestamp(slot.pts), rate: 1.0 });
        }
        // Announced again after every flush. Core sends it on only if it changed.
        if lane.announced != format {
            let family = FAMILIES[(family as usize).min(FAMILIES.len() - 1)];
            ctx.announce_format(pad, family, &[(FIELD, ValueDesc::Int(format as i64))]);
            lane.announced = format;
        }
        let tag = Tag {
            seq: slot.seq,
            seek_gen: self.seek_gen,
            size: slot.size,
            stream: n as u8,
            format,
        };
        let bytes = buf.memory.as_mut_full();
        if !wire::encode_filled(&tag, bytes, len) {
            let capacity = bytes.len();
            return Err(Error::element_failed(ctx.element(), FakeError::SlotTooSmall { capacity }));
        }
        buf.memory.set_len(len);
        buf.pts = Timestamp(slot.pts);
        buf.dts = Timestamp(slot.dts);
        buf.duration = Timestamp(slot.duration);
        buf.flags = BufferFlags(slot.flags as u32);
        ctx.out(pad).push(buf);
        lane.released = slot.release;
        lane.wait_until = NONE;
        lane.said = slot.pts;
        lane.next = media::step(&self.spec, &mut lane.cursor);
        if n == 0 {
            self.learn(ctx, slot.seq);
        }
        self.say_gaps(ctx, slot.pts);
        Ok(true)
    }

    fn describe(&self, ctx: &mut Ctx) {
        for n in (0..MAX_STREAMS).filter(|n| self.pad_of(*n).is_some()) {
            let spec = &self.decl.streams[n];
            let kind = match spec.kind {
                StreamKind::Video { .. } => Kind::Video,
                StreamKind::Audio { .. } => Kind::Audio,
                StreamKind::Text { .. } => Kind::Text,
            };
            ctx.add_stream(Stream {
                kind,
                codec: super::offers::FAMILIES[0],
                language: "".into(),
                title: "".into(),
                default: true,
                forced: false,
                disposition: Disposition::MAIN,
                channels: 0,
                duration: Timestamp(spec.duration),
                sparse: kind.sparse(),
                name: if n == 0 { "a".into() } else { "b".into() },
                offers: &MEDIA,
            });
        }
    }

    fn bytes_in(&mut self, n: usize) -> bool {
        let len = self.next_seq(n).map_or(PAYLOAD, |seq| self.len_of(n, seq));
        self.net.as_mut().is_none_or(|feed| {
            feed.need(len);
            feed.ready()
        })
    }

    fn all_ended(&self) -> bool {
        self.lanes.iter().flatten().all(|lane| matches!(lane.next, Step::End { .. }))
    }

    fn seek(&mut self, ctx: &Ctx) {
        let target = ctx.seek_target();
        let to = target.map_or(0, |target| target.at.0);
        let byte = target.map_or(0, |target| target.to_byte);
        self.seek_gen = ctx.applied_seek_gen() as u32;
        for (n, lane) in self.lanes.iter_mut().enumerate() {
            let Some(lane) = lane else { continue };
            // A demuxer handed an estimate goes back to the keyframe of its group.
            let byte = match self.reads {
                Reads::Picture(p, mapped) if !matches!(mapped, Mapped::Whole) => p.resync(byte),
                _ => byte,
            };
            match (self.reads, n) {
                (Reads::Picture(..), 0) | (Reads::Byte, _) => {
                    media::seek_to(&self.spec, &mut lane.cursor, byte, 0)
                }
                (Reads::Picture(p, _), _) => {
                    media::seek(&self.spec, &mut lane.cursor, p.pts(byte.min(p.count())), 0)
                }
                (Reads::Time, _) => media::seek(&self.spec, &mut lane.cursor, to, 0),
            };
            lane.next = media::step(&self.spec, &mut lane.cursor);
            lane.released = 0;
            lane.wait_until = NONE;
            lane.fresh = true;
            lane.announced = 0;
            lane.said = to;
        }
    }
}

impl Element for FakeSource {
    fn desc(&self) -> &'static ElementDesc {
        let probed = |stream: usize| self.offers.srcs[stream].probed() as usize;
        let net = self.net.is_some() as usize;
        if self.decl.described {
            return &SOURCE_STREAMS;
        }
        match self.served {
            2 => &SOURCE_2[net][probed(0) | probed(1) << 1],
            _ => &SOURCE_1[net][probed((self.first as usize).min(MAX_STREAMS - 1))],
        }
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        // Never remote, the model holds a skip to the group being presented.
        ctx.publish_serves(Serves { by: Addressed::Byte, remote: false });
        if let Reads::Picture(pictures, mapped) = self.reads {
            match mapped {
                Mapped::Whole => self.publish(ctx, &pictures, pictures.keys(), true),
                Mapped::Partial => self.publish(ctx, &pictures, pictures.keys().div_ceil(2), false),
                Mapped::Late { .. } | Mapped::Grows => {}
            }
        }
        if self.decl.described {
            self.describe(ctx);
            return Ok(());
        }
        for stream in 0..MAX_STREAMS {
            let (Some(at), pad) = (self.pad_of(stream), &self.offers.srcs[stream]) else {
                continue;
            };
            if pad.probed() {
                supply(pad, |offers| ctx.offer_src(at, offers))?;
            }
        }
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let field = ctx.field_id(FIELD);
        let pads: [Option<SrcPad>; MAX_STREAMS] =
            core::array::from_fn(|stream| self.pad_of(stream));
        for (linked, pad) in self.linked.iter_mut().zip(pads) {
            let Some(format) = pad.and_then(|pad| ctx.negotiated_src(pad)) else {
                continue;
            };
            let family = ctx.family_name(format.family);
            linked.0 = FAMILIES.iter().position(|name| Some(*name) == family).unwrap_or(0) as u8;
            if let Some(Value::Int(id)) = field.and_then(|field| format.get(field)) {
                linked.1 = id as u8;
            }
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let now = ctx.now().nanos().unwrap_or(0);
        let mut wake = NONE;
        if let Some(feed) = self.net.as_mut() {
            feed.drain(ctx).map_err(|why| Error::element_failed(ctx.element(), why))?;
        }
        for _ in 0..self.decl.burst.max(1) {
            match self.pick(now, &mut wake) {
                Pick::Lane(n) if self.net.is_some() && !self.bytes_in(n) => break,
                Pick::Lane(n) if self.emit(ctx, n)? => {
                    if let Some(feed) = self.net.as_mut() {
                        feed.spend();
                    }
                }
                Pick::Failed(why) => return Err(Error::element_failed(ctx.element(), why)),
                _ => break,
            }
        }
        if self.all_ended() {
            if let Some(feed) = self.net.as_mut() {
                feed.close(ctx);
            }
            return Ok(Flow::Eos);
        }
        if let Some(feed) = self.net.as_mut() {
            feed.pump(ctx);
        }
        if wake != NONE {
            ctx.wake_at(Timestamp(wake));
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.seek(ctx);
            if let Some(feed) = self.net.as_mut() {
                feed.flush(ctx, self.seek_gen as u64);
            }
        }
        Ok(())
    }
}
