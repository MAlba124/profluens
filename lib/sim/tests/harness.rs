//! The element drivers, against an element small enough to predict.

use std::cell::Cell;

use pf_harness::Harness;
use pf_sim::corrupt::harness::{
    Fed, drive, drive_pieces, feed_bytes, input_buffer, pool_allowance, pool_shape,
};
use pf_sim::corrupt::{Case, Chunking, Violation};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::SrcPad;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static DESC: ElementDesc = ElementDesc {
    name: "echo",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Debug)]
struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("refused")
    }
}

impl std::error::Error for Refused {}

#[derive(Default)]
struct Echo {
    refuse: Option<u8>,
    babble: bool,
}

const OUT: SrcPad = SrcPad(0);

fn emit(ctx: &mut Ctx, bytes: &[u8]) {
    let mut out = ctx.alloc();
    out.memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
    out.memory.set_len(bytes.len());
    ctx.out(OUT).push(out);
}

impl Element for Echo {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(input) = inputs.pop() {
            let data = input.memory.data();
            if self.refuse.is_some_and(|byte| data.contains(&byte)) {
                return Err(Error::element_failed(ctx.element(), Refused));
            }
            emit(ctx, data);
        }
        if self.babble {
            emit(ctx, &[0]);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::Eos) {
            emit(ctx, &[0xEE]);
        }
        Ok(())
    }
}

const CHUNKINGS: [Chunking; 4] =
    [Chunking::Whole, Chunking::Fixed(1), Chunking::Fixed(7), Chunking::Random { max: 5, seed: 3 }];

fn case(data: &[u8], chunking: Chunking) -> Case {
    Case { base: "ramp", ops: Vec::new(), data: data.to_vec(), chunking, knob: 15 }
}

fn ramp(len: usize) -> Vec<u8> {
    (0..len).map(|i| i as u8).collect()
}

#[test]
fn feed_bytes_pushes_every_chunk_and_counts_what_comes_out() {
    for len in [0, 1, 100] {
        for chunking in CHUNKINGS {
            let case = case(&ramp(len), chunking);
            let chunks = chunking.count(&case.data);
            let fed = feed_bytes(Echo::default(), &case, Some(1), 0).expect("within its steps");
            let (steps, out_buffers) = (3 * chunks as u64, chunks + 1);
            let want = Fed { steps, out_buffers, out_bytes: len + 1, failed: false };
            assert_eq!(fed, want, "{len} bytes as {chunking:?}");
        }
    }
}

#[test]
fn feed_bytes_stops_at_an_element_error_without_the_tail() {
    let case = case(&ramp(100), Chunking::Fixed(10));
    let refusing = Echo { refuse: Some(55), ..Echo::default() };
    let fed = feed_bytes(refusing, &case, None, 0).expect("within its steps");
    assert!(fed.failed);
    assert_eq!((fed.out_buffers, fed.out_bytes), (5, 50));
}

#[test]
fn feed_bytes_reports_a_babbling_element_as_too_many_steps() {
    let case = case(&ramp(8), Chunking::Whole);
    let babbling = Echo { babble: true, ..Echo::default() };
    let limit = 16 + 4 * 8;
    let fed = feed_bytes(babbling, &case, None, 0);
    assert_eq!(fed, Err(Violation::Steps { taken: limit + 1, limit }));
}

#[test]
fn drive_pushes_every_piece_and_counts_what_comes_out() {
    for len in [0, 1, 100] {
        for chunking in CHUNKINGS {
            let data = ramp(len);
            let pieces = chunking.pieces(&data).count();
            for flush in [None, Some(0), Some(1)] {
                let mut harness = Harness::new(Echo::default());
                let fed = drive(&mut harness, &data, chunking, flush);
                let (steps, out_buffers) = (2 * pieces as u64, pieces + 1);
                let want = Fed { steps, out_buffers, out_bytes: len + 1, failed: false };
                assert_eq!(fed, want, "{len} bytes as {chunking:?}, flush {flush:?}");
            }
        }
    }
}

#[test]
fn drive_stops_at_an_element_error_without_the_tail() {
    let mut harness = Harness::new(Echo { refuse: Some(55), ..Echo::default() });
    let fed = drive(&mut harness, &ramp(100), Chunking::Fixed(10), None);
    assert!(fed.failed);
    assert_eq!((fed.out_buffers, fed.out_bytes), (5, 50));
}

#[test]
fn drive_pieces_stamps_each_buffer_once_in_order() {
    let data = ramp(30);
    let pieces: Vec<&[u8]> = data.chunks(7).collect();
    let next = Cell::new(0);
    let mut harness = Harness::new(Echo::default());
    let fed = drive_pieces(&mut harness, &pieces, None, &|index, buffer| {
        assert_eq!(index, next.replace(index + 1));
        assert_eq!(buffer.memory.data(), pieces[index]);
    });
    assert_eq!(next.get(), pieces.len());
    assert_eq!((fed.out_buffers, fed.out_bytes, fed.failed), (6, 31, false));
}

#[test]
fn drive_panics_on_a_babbling_element() {
    let babbling = std::panic::catch_unwind(|| {
        let mut harness = Harness::new(Echo { babble: true, ..Echo::default() });
        drive(&mut harness, &[1, 2, 3], Chunking::Whole, None)
    });
    let payload = babbling.expect_err("a livelock has to fail");
    let message = payload.downcast_ref::<String>().expect("a message");
    // Two calls for the one piece, three bytes and the fixed 64.
    assert_eq!(message, "element still producing after 69 process calls on 3 input bytes");
}

#[test]
fn every_knob_has_a_pool_shape_and_an_allowance_that_covers_it() {
    let mut shapes = Vec::new();
    for knob in 0..64 {
        let (slot, slots) = pool_shape(knob);
        assert_eq!((slot, slots), pool_shape(knob & 15));
        assert!(pool_allowance(knob) > slot * slots as usize);
        shapes.push((slot, slots));
    }
    shapes.sort_unstable();
    shapes.dedup();
    assert_eq!(shapes.len(), 16);
}

#[test]
fn an_input_buffer_holds_exactly_the_bytes() {
    for len in [0, 1, 4097] {
        let bytes = ramp(len);
        assert_eq!(input_buffer(&bytes).memory.data(), bytes);
    }
}
