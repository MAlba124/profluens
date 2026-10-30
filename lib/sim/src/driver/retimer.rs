//! The fake retimer. It stands where a stretcher would, claims the timeline, and says at which
//! stream time each rate starts. What it decides is [`Retiming`], which the oracles rerun.

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::ctx::Ctx;
use profluens_core::element::{Element, ElementDesc, Flow};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::ring::Producer;
use profluens_core::time::{Rate, Timestamp};

use super::map;
use super::pads::{FakeError, OUT, retimer_desc};
use super::retime::{MAX_OUT, RetimeDecl, Retiming, stamp};
use crate::record::{Row, RowKind};
use crate::time::NONE;

pub const RETIMER_ROWS: u16 = u16::MAX - 2;

const SPARES: usize = MAX_OUT as usize - 1;

pub struct FakeRetimer {
    decl: RetimeDecl,
    inline: bool,
    state: Retiming,
    tap: Producer<Row>,
    spares: [Option<Buffer>; SPARES],
    fresh: bool,
    seek_gen: u32,
}

impl FakeRetimer {
    pub fn new(decl: RetimeDecl, inline: bool, tap: Producer<Row>) -> Self {
        let state = Retiming::new(decl);
        let spares = std::array::from_fn(|_| None);
        Self { decl, inline, state, tap, spares, fresh: true, seek_gen: 0 }
    }

    fn push(&mut self, ctx: &Ctx, row: Row) -> Result<(), Error> {
        let full =
            |_| Error::element_failed(ctx.element(), FakeError::TapFull { sink: RETIMER_ROWS });
        self.tap.try_push(Row { seek_gen: self.seek_gen, ..row }).map_err(full)
    }

    fn spared(&mut self, ctx: &mut Ctx, want: usize) -> bool {
        for slot in self.spares.iter_mut().take(want) {
            if slot.is_none() {
                *slot = ctx.try_alloc();
            }
        }
        self.spares.iter().take(want).all(Option::is_some)
    }

    fn copy(&mut self, buf: &Buffer, (k, n): (u32, u32), flags: BufferFlags) {
        let Some(copy) = self.spares.get_mut(k as usize - 1).and_then(Option::as_mut) else {
            return;
        };
        let bytes = buf.memory.data();
        let room = copy.memory.as_mut_full();
        let len = bytes.len().min(room.len());
        room[..len].copy_from_slice(&bytes[..len]);
        copy.memory.set_len(len);
        let (at, length) = stamp(buf.pts.0, buf.duration.0, k, n);
        (copy.pts, copy.dts, copy.duration) = (Timestamp(at), Timestamp(at), Timestamp(length));
        copy.flags = flags;
    }

    fn put_out(&mut self, ctx: &mut Ctx, mut buf: Buffer, n: u32) {
        let n = n.min(MAX_OUT);
        let mut plain = buf.flags;
        plain.remove(BufferFlags::DISCONT);
        for k in 1..n {
            self.copy(&buf, (k, n), plain);
        }
        if n == 0 {
            return;
        }
        let (at, length) = stamp(buf.pts.0, buf.duration.0, 0, n);
        (buf.pts, buf.dts, buf.duration) = (Timestamp(at), Timestamp(at), Timestamp(length));
        buf.flags = plain;
        if std::mem::take(&mut self.fresh) {
            buf.flags.insert(BufferFlags::DISCONT);
        }
        ctx.out(OUT).push(buf);
        for k in 1..n {
            if let Some(copy) = self.spares.get_mut(k as usize - 1).and_then(Option::take) {
                ctx.out(OUT).push(copy);
            }
        }
    }

    fn take(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<bool, Error> {
        let Some(front) = inputs.front() else {
            return Ok(false);
        };
        let (pts, duration, flags) = (front.pts, front.duration, front.flags);
        let Some(tag) = super::wire::read(front.memory.data()) else {
            return Err(Error::element_failed(ctx.element(), FakeError::Untagged));
        };
        // Decided on a copy of the state, which stands only once the buffer is taken.
        let mut next = self.state;
        let took = next.take();
        if !self.spared(ctx, (took.out as usize).saturating_sub(1).min(SPARES)) {
            return Ok(false);
        }
        let Some(buf) = inputs.pop() else {
            return Ok(false);
        };
        self.state = next;
        let word = took.rate.pack();
        if took.says {
            // A rate the model keeps is one core takes, so the else is never met.
            if let Ok(rate) = Rate::new(took.rate.num as i64, took.rate.den as i64) {
                ctx.retime(pts, rate);
            }
            self.push(
                ctx,
                Row { pts: pts.0, aux: word, ..Row::new(RowKind::Retime, RETIMER_ROWS, 0) },
            )?;
        }
        let row = Row {
            seq: tag.seq,
            pts: pts.0,
            dts: buf.dts.0,
            duration: duration.0,
            aux: word,
            size: tag.size,
            flags: flags.0 as u16,
            format: took.out.min(MAX_OUT) as u8,
            ..Row::new(RowKind::Retimed, RETIMER_ROWS, tag.stream as u16)
        };
        self.push(ctx, row)?;
        self.put_out(ctx, buf, took.out);
        Ok(true)
    }
}

impl Element for FakeRetimer {
    fn desc(&self) -> &'static ElementDesc {
        retimer_desc(self.inline, self.decl.latency)
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.claim_timeline()
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        for _ in 0..inputs.len() {
            if !self.take(ctx, &mut inputs)? {
                break;
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::RateAsked { rate } => {
                let asked = map::Rate { num: rate.num(), den: rate.den() };
                self.state.ask(asked);
                self.push(
                    ctx,
                    Row { aux: asked.pack(), ..Row::new(RowKind::RateAsked, RETIMER_ROWS, 0) },
                )
            }
            Event::FlushStart => {
                self.state.flush();
                self.fresh = true;
                self.seek_gen = ctx.applied_seek_gen() as u32;
                let to = ctx.presentation_point().map_or(NONE, |at| at.0);
                self.push(ctx, Row { aux: to, ..Row::new(RowKind::FlushStart, RETIMER_ROWS, 0) })
            }
            Event::Eos => self.push(ctx, Row::new(RowKind::Eos, RETIMER_ROWS, 0)),
            Event::FormatChange(format) => {
                ctx.forward_format(OUT, format.clone());
                Ok(())
            }
            Event::Segment { base, rate } => {
                ctx.push_event(OUT, Event::Segment { base: *base, rate: *rate });
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.spares = std::array::from_fn(|_| None);
    }
}
