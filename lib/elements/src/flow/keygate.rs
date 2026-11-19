//! `keygate`, lets one keyframe a seek through to a decoder, sealed by an end of sequence so the
//! decoder gives its picture at once instead of after the frames it reorders behind it, and
//! holds the rest so nothing is decoded that the next seek flushes.

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

static OFFERS: [OfferDesc; 1] = [OfferDesc::wildcard()];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

static DESC: ElementDesc = ElementDesc {
    name: "keygate",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[Coupling::through(0, 0)],
};

/// Where the gate is between seeks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// No seek yet, everything goes through, a demuxer may need to read on to be seekable.
    Through,
    /// A seek's flush came, its first keyframe goes through.
    Open,
    /// The seek's keyframe went, the rest waits for the next seek.
    Shut,
}

/// What the gate does with the buffer at its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    Forward,
    /// The seek's keyframe, sealed with an end of sequence.
    Seal,
    Drop,
    Hold,
}

impl Gate {
    /// `early` for a keyframe before the seek's stamp, a demuxer that restarts at its cluster
    /// meets a scene cut's keyframe ahead of the one at the stamp. It is dropped and the gate
    /// waits for the stamp's own.
    fn pass(&mut self, key: bool, head: bool, early: bool) -> Pass {
        match (*self, key, head) {
            (Gate::Through, ..) => Pass::Forward,
            (Gate::Shut, ..) => Pass::Hold,
            (Gate::Open, _, true) => Pass::Forward,
            (Gate::Open, true, false) if early => Pass::Drop,
            (Gate::Open, true, false) => {
                *self = Gate::Shut;
                Pass::Seal
            }
            (Gate::Open, false, false) => Pass::Drop,
        }
    }
}

pub struct KeyGate {
    eos: &'static [u8],
    tolerance_ns: u64,
    gate: Gate,
    /// The stamp the last seek presents from, ns.
    point: Option<u64>,
}

impl KeyGate {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    /// A gate sealing each seek's keyframe with the end of sequence NAL `eos`, a keyframe more
    /// than `tolerance_ns` before the seek's stamp dropped as early.
    pub fn new(eos: &'static [u8], tolerance_ns: u64) -> Self {
        KeyGate { eos, tolerance_ns, gate: Gate::Through, point: None }
    }

    /// `b` with the end of sequence after its last NAL, in place when its slot has room.
    fn sealed(&self, ctx: &mut Ctx, mut b: Buffer) -> Result<Buffer, Error> {
        let (n, eos) = (b.memory.len(), self.eos);
        let end = n + eos.len();
        if !b.memory.is_unique() || b.memory.capacity() < end {
            b = ctx.copy_with_room(&b, eos.len()).ok_or_else(|| ctx.exact_refused(end))?;
        }
        b.memory.as_mut_full()[n..end].copy_from_slice(eos);
        b.memory.set_len(end);
        Ok(b)
    }
}

impl Element for KeyGate {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(b) = inputs.front() {
            let key = b.flags.contains(BufferFlags::KEYFRAME);
            let head = b.flags.contains(BufferFlags::HEADER);
            let early = self
                .point
                .zip(b.pts.nanos())
                .is_some_and(|(point, pts)| pts.saturating_add(self.tolerance_ns) < point);
            let pass = self.gate.pass(key, head, early);
            if pass == Pass::Hold {
                break;
            }
            let Some(b) = inputs.pop() else { break };
            let out = match pass {
                Pass::Forward => b,
                Pass::Seal => self.sealed(ctx, b)?,
                Pass::Drop | Pass::Hold => {
                    ctx.count_drop();
                    continue;
                }
            };
            let came =
                ctx.negotiated_sink(Self::IN).filter(|f| Some(*f) != ctx.negotiated_src(Self::OUT));
            if let Some(f) = came.cloned() {
                ctx.forward_format(Self::OUT, f);
            }
            ctx.out(Self::OUT).push(out);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FlushStart | Event::SkippedTo { .. } = event
            && let Some(point) = ctx.presentation_point().and_then(Timestamp::nanos)
        {
            self.gate = Gate::Open;
            self.point = Some(point);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use profluens_core::batch::Batch;
    use profluens_core::bus::Bus;
    use profluens_core::id::{ElementId, FormatId};
    use profluens_core::memory::Pool;

    const HEVC_EOS: [u8; 6] = [0, 0, 0, 1, 0x48, 0x01];
    const AU: [u8; 8] = [0, 0, 0, 1, 0x2a, 0x01, 0xaf, 0x08];
    const TOL_NS: u64 = 50_000_000;

    fn ctx(pool: &Pool) -> (Ctx, impl Sized) {
        let (bus, rx) = Bus::channel();
        (Ctx::new(pool.clone(), bus, ElementId(0), FormatId(0), 0), rx)
    }

    fn unit(pool: &Pool, ms: u64, flags: BufferFlags) -> Buffer {
        let mut memory = pool.acquire();
        memory.as_mut_full()[..AU.len()].copy_from_slice(&AU);
        memory.set_len(AU.len());
        Buffer {
            memory,
            pts: Timestamp(ms * 1_000_000),
            dts: Timestamp(ms * 1_000_000),
            duration: Timestamp(40_000_000),
            flags,
            format: FormatId(0),
            sync: None,
        }
    }

    #[test]
    fn the_gate_lets_one_keyframe_a_seek_through() {
        let mut g = Gate::Through;
        assert_eq!(g.pass(false, false, false), Pass::Forward);
        assert_eq!(g.pass(true, false, false), Pass::Forward);
        for _ in 0..2 {
            g = Gate::Open;
            assert_eq!(g.pass(false, true, false), Pass::Forward, "the head");
            assert_eq!(g.pass(false, false, false), Pass::Drop, "before the keyframe");
            assert_eq!(g.pass(true, false, true), Pass::Drop, "a keyframe before the stamp");
            assert_eq!(g.pass(true, false, false), Pass::Seal);
            assert_eq!(g.pass(false, false, false), Pass::Hold);
            assert_eq!(g.pass(true, false, false), Pass::Hold, "the next keyframe waits too");
        }
    }

    /// In its own slot or copied out of one the demuxer shares, and the frame behind it stays
    /// at the input.
    #[test]
    fn the_gate_seals_the_seeks_keyframe_and_holds_the_rest() {
        let pool = Pool::new(64);
        let (mut ctx, _rx) = ctx(&pool);
        let sealed = [0, 0, 0, 1, 0x2a, 0x01, 0xaf, 0x08, 0, 0, 0, 1, 0x48, 0x01];
        let mut gate = KeyGate::new(&HEVC_EOS, TOL_NS);
        for shared in [false, true] {
            ctx.set_seek_target(1, Timestamp(2_000_000_000), None);
            gate.event(&mut ctx, &Event::FlushStart).expect("an event");
            let key = unit(&pool, 2_000, BufferFlags::KEYFRAME);
            let kept = shared.then(|| key.memory.clone());
            let mut batch = Batch::new(FormatId(0));
            batch.push(unit(&pool, 2_000, BufferFlags::DELTA));
            batch.push(key);
            batch.push(unit(&pool, 2_000, BufferFlags::DELTA));
            gate.process(&mut ctx, Inputs::owned(&mut batch)).expect("a pass");
            assert_eq!(batch.len(), 1, "the frame behind the keyframe waits");
            let mut out = ctx.take_output(KeyGate::OUT);
            let b = out.pop_front().expect("the keyframe");
            assert_eq!(b.memory.data(), &sealed, "shared {shared}");
            assert!(b.flags.contains(BufferFlags::KEYFRAME));
            assert_eq!(b.pts, Timestamp(2_000_000_000));
            assert!(out.pop_front().is_none());
            if let Some(m) = kept {
                assert_eq!(m.data(), &sealed[..8], "the shared slot untouched");
            }
        }
    }

    /// A seek into a cluster with two keyframes, a scene cut 376 ms after the cluster's first,
    /// seals the one at its stamp.
    #[test]
    fn the_gate_passes_the_keyframe_at_the_stamp_not_its_clusters_first() {
        let pool = Pool::new(64);
        let (mut ctx, _rx) = ctx(&pool);
        let mut gate = KeyGate::new(&HEVC_EOS, TOL_NS);
        ctx.set_seek_target(1, Timestamp(9_297_789_000_000), None);
        gate.event(&mut ctx, &Event::FlushStart).expect("an event");
        let mut batch = Batch::new(FormatId(0));
        batch.push(unit(&pool, 9_297_413, BufferFlags::KEYFRAME));
        batch.push(unit(&pool, 9_297_455, BufferFlags::DELTA));
        batch.push(unit(&pool, 9_297_789, BufferFlags::KEYFRAME));
        batch.push(unit(&pool, 9_297_830, BufferFlags::DELTA));
        gate.process(&mut ctx, Inputs::owned(&mut batch)).expect("a pass");
        let mut out = ctx.take_output(KeyGate::OUT);
        let b = out.pop_front().expect("a keyframe");
        assert_eq!(b.pts, Timestamp(9_297_789_000_000));
        assert!(out.pop_front().is_none());
        assert_eq!(batch.len(), 1, "the frame behind it waits");
    }
}
