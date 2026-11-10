//! Threadless single-element test rig with a mock clock. It is the inline caller,
//! not a scheduler, so there are no rings, thread groups or seek generations.

use std::sync::Arc;

use profluens_core::batch::Batch;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::{Bus, BusMessage, BusSender};
use profluens_core::clock::MockClock;
use profluens_core::ctx::{Ctx, SeekTarget};
use profluens_core::element::{Element, Flow};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{FixedFormat, Value, Vocabulary};
use profluens_core::id::{Interner, SinkPad, SrcPad};
use profluens_core::memory::{Pool, PoolConfig};
use profluens_core::props::PropTable;
use profluens_core::sched::{after_process, process_once, resolve_announcements};
use profluens_core::time::Timestamp;

/// Format id of an un-negotiated pad, the pipeline's `BYTES` sentinel.
const BYTES: profluens_core::id::FormatId = profluens_core::id::FormatId(0);

const DEFAULT_SLOT: usize = 64 * 1024;

const DEFAULT_SLOTS: u32 = 1024;

/// Wall time wakes an element may ask for while it holds the end, under `real_wakes`.
const WAKE_BUDGET: u32 = 5000;
const MAX_NAP: std::time::Duration = std::time::Duration::from_millis(10);

/// A threadless, mock-clocked wrapper around one [`Element`].
pub struct Harness {
    element: Box<dyn Element>,
    ctx: Ctx,
    clock: MockClock,
    vocabulary: Arc<Vocabulary>,
    bus: Bus,
    pool: Pool,
    announced: Option<FixedFormat>,
    format_changes: u32,
    started: bool,
    off_pool: Pool,
    sink_pad: Option<SinkPad>,
    real_wakes: bool,
    props: Option<Arc<PropTable>>,
}

impl Harness {
    pub fn new(element: impl Element + 'static) -> Self {
        Self::with_pool(element, DEFAULT_SLOT, DEFAULT_SLOTS)
    }

    pub fn with_slot_size(element: impl Element + 'static, slot_size: usize) -> Self {
        Self::with_pool(element, slot_size, DEFAULT_SLOTS)
    }

    /// A small `max_slots` exercises backpressure, the element's `try_alloc` returns `None`.
    pub fn with_pool(element: impl Element + 'static, slot_size: usize, max_slots: u32) -> Self {
        Self::with_pool_config(element, PoolConfig::heap(slot_size, max_slots))
    }

    // Cold path, once per harness.
    #[allow(clippy::disallowed_methods)]
    pub fn with_pool_config(element: impl Element + 'static, config: PoolConfig) -> Self {
        let slot_size = config.slot_size;
        let element: Box<dyn Element> = Box::new(element);
        let desc = element.desc();

        let sink_pad = desc.sinks.first().map(|_| SinkPad(0));

        let (mut formats, mut fields, mut values) =
            (Interner::new(), Interner::new(), Interner::new());
        for pad in desc.sinks.iter().chain(desc.srcs) {
            for offer in pad.offers {
                formats.intern(offer.family);
                for fd in offer.fields {
                    fields.intern(fd.field);
                    intern_constraint_values(&fd.allowed, &mut values);
                    if let Some(vd) = fd.preferred {
                        intern_value_desc(&vd, &mut values);
                    }
                }
            }
        }
        let vocabulary = Arc::new(Vocabulary { formats, fields, values });

        let pool = Pool::with_config(config);
        let (bus_tx, bus) = Bus::channel();
        let mut ctx = new_ctx(&pool, &bus_tx);
        ctx.configure_pads(desc.srcs.len(), desc.sinks.len());
        ctx.set_vocabulary(Arc::clone(&vocabulary));
        let clock = MockClock::new();
        ctx.set_clock(Arc::new(clock.clone()), Arc::new(std::sync::atomic::AtomicI64::new(0)));
        let props = (!desc.props.is_empty()).then(|| Arc::new(PropTable::new(desc.props.len())));
        if let Some(table) = &props {
            ctx.set_props(desc.props, Arc::clone(table));
        }

        Self {
            element,
            ctx,
            clock,
            vocabulary,
            bus,
            pool,
            announced: None,
            format_changes: 0,
            started: false,
            off_pool: Pool::new(slot_size),
            sink_pad,
            real_wakes: false,
            props,
        }
    }

    /// Set a declared property, read by the element at its start. `false` for an undeclared one.
    pub fn set_prop(&mut self, name: &str, v: Value) -> bool {
        let Some(table) = &self.props else {
            return false;
        };
        match self.element.desc().props.iter().position(|p| p.name == name) {
            Some(idx) => {
                table.set(idx, v);
                true
            }
            None => false,
        }
    }

    /// While the end is held, a `wake_within` sleeps that long as the scheduler does and is not an
    /// idle pass. For elements polling hardware that finishes on its own time.
    pub fn real_wakes(mut self) -> Self {
        self.real_wakes = true;
        self
    }

    pub fn fix_sink_format(
        &mut self,
        pad: SinkPad,
        family: &str,
        fields: &[(&'static str, profluens_core::format::ValueDesc)],
    ) {
        let fixed = self.intern_fixed(family, fields);
        self.ctx.set_negotiated_sink(pad, fixed);
    }

    /// Installs a new sink format and delivers its `FormatChange`, as a renegotiation midstream.
    pub fn change_sink_format(
        &mut self,
        pad: SinkPad,
        family: &str,
        fields: &[(&'static str, profluens_core::format::ValueDesc)],
    ) -> Result<(), Error> {
        let fixed = self.intern_fixed(family, fields);
        self.ctx.set_negotiated_sink(pad, fixed.clone());
        self.start()?;
        self.element.event(&mut self.ctx, &Event::FormatChange(fixed))
    }

    pub fn fix_src_format(
        &mut self,
        pad: SrcPad,
        family: &str,
        fields: &[(&'static str, profluens_core::format::ValueDesc)],
    ) {
        let fixed = self.intern_fixed(family, fields);
        self.ctx.set_negotiated_src(pad, fixed);
    }

    pub fn fix_src_takes(
        &mut self,
        pad: SrcPad,
        field: &'static str,
        values: &[profluens_core::format::ValueDesc],
    ) {
        use profluens_core::format::{Value, ValueDesc};
        let vocab = make_mut_vocabulary(&mut self.vocabulary);
        vocab.fields.intern(field);
        for vd in values {
            intern_value_desc(vd, &mut vocab.values);
        }
        self.ctx.set_vocabulary(Arc::clone(&self.vocabulary));
        let v = &self.vocabulary;
        let field = v.field_id(field).expect("interned");
        let values: Vec<Value> = values
            .iter()
            .map(|vd| match vd {
                ValueDesc::Int(n) => Value::Int(*n),
                ValueDesc::Rat(n, d) => Value::Rat(*n, *d),
                ValueDesc::Id(s) => Value::Id(v.value_id(s).expect("interned")),
            })
            .collect();
        self.ctx.set_src_takes(pad, field, &values);
    }

    fn intern_fixed(
        &mut self,
        family: &str,
        fields: &[(&'static str, profluens_core::format::ValueDesc)],
    ) -> FixedFormat {
        let vocab = make_mut_vocabulary(&mut self.vocabulary);
        vocab.formats.intern(family);
        for (name, vd) in fields {
            vocab.fields.intern(name);
            intern_value_desc(vd, &mut vocab.values);
        }
        self.ctx.set_vocabulary(Arc::clone(&self.vocabulary));

        self.vocabulary
            .build_fixed(family, fields)
            .expect("fix format: family/field/value must be internable")
    }

    pub fn start(&mut self) -> Result<(), Error> {
        if self.started {
            return Ok(());
        }
        self.element.start(&mut self.ctx)?;
        self.started = true;
        Ok(())
    }

    pub fn stop(&mut self) {
        if std::mem::take(&mut self.started) {
            self.element.stop(&mut self.ctx);
        }
    }

    pub fn push(&mut self, buf: Buffer) -> Result<Flow, Error> {
        self.start()?;
        let mut batch = Batch::new(BYTES);
        batch.push(buf);
        self.ctx.input_append(&mut batch);
        self.run_process()
    }

    pub fn push_batch(&mut self, mut batch: Batch) -> Result<Flow, Error> {
        self.start()?;
        self.ctx.input_append(&mut batch);
        self.run_process()
    }

    /// Deliver an event. A `FormatChange` also installs its format on the sink pad as the scheduler
    /// does.
    pub fn push_event(&mut self, event: Event) -> Result<(), Error> {
        self.start()?;
        if let (Event::FormatChange(f), Some(pad)) = (&event, self.sink_pad) {
            self.ctx.set_negotiated_sink(pad, f.clone());
        }
        self.element.event(&mut self.ctx, &event)
    }

    pub fn skip_to(&mut self, to_time: Timestamp) -> Result<(), Error> {
        self.ctx.set_presentation(to_time);
        self.push_event(Event::SkippedTo { to_time })
    }

    pub fn seek(&mut self, to_byte: u64, to_time: Timestamp) -> Result<(), Error> {
        self.seek_to(to_time, Some(SeekTarget { to_byte, at: to_time }))
    }

    pub fn seek_to(&mut self, to_time: Timestamp, target: Option<SeekTarget>) -> Result<(), Error> {
        self.ctx.set_seek_target(self.ctx.applied_seek_gen() + 1, to_time, target);
        self.push_event(Event::FlushStart)?;
        self.ctx.discard_buffers();
        Ok(())
    }

    pub fn seek_preview(&mut self, to_byte: u64, to_time: Timestamp) -> Result<(), Error> {
        self.ctx.set_seek_target(
            self.ctx.applied_seek_gen() + 1,
            to_time,
            Some(SeekTarget { to_byte, at: to_time }),
        );
        self.ctx.arm_preview();
        self.push_event(Event::FlushStart)?;
        self.ctx.discard_buffers();
        Ok(())
    }

    pub fn crank(&mut self) -> Result<Flow, Error> {
        self.start()?;
        self.run_process()
    }

    pub fn pull(&mut self, pad: SrcPad) -> Option<Buffer> {
        let mut batch = self.ctx.take_output(pad);
        let head = batch.pop_front();
        if !batch.is_empty() {
            self.ctx.output_on(pad).append(&mut batch);
        }
        head
    }

    pub fn announced(&self) -> Option<FixedFormat> {
        self.announced.clone()
    }

    pub fn format_changes(&self) -> u32 {
        self.format_changes
    }

    pub fn clock(&self) -> &MockClock {
        &self.clock
    }

    pub fn input_len(&self) -> usize {
        self.ctx.input_len()
    }

    /// The wake requests made since the last call. `NONE` and `Duration::MAX` mean no request.
    pub fn take_wakes(&mut self) -> (Timestamp, std::time::Duration) {
        let until = self.ctx.take_wake_at().unwrap_or(Timestamp::NONE);
        (until, self.ctx.take_wake_within())
    }

    pub fn vocabulary(&self) -> &Vocabulary {
        &self.vocabulary
    }

    pub fn pool_stats(&self) -> profluens_core::memory::PoolStats {
        self.pool.stats()
    }

    /// A buffer holding `bytes`. Panics past the exact cap, a test's own input.
    #[allow(clippy::expect_used)]
    pub fn alloc(&mut self, bytes: &[u8]) -> Buffer {
        let mut memory =
            self.pool.acquire_exact(bytes.len()).expect("test input under the exact cap");
        memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
        memory.set_len(bytes.len());
        Buffer {
            memory,
            pts: Timestamp::NONE,
            dts: Timestamp::NONE,
            duration: Timestamp::NONE,
            flags: BufferFlags::empty(),
            format: BYTES,
            sync: None,
        }
    }

    /// End the stream as the scheduler does and return every buffer put out, FIFO.
    pub fn eos(&mut self) -> Result<Vec<Buffer>, Error> {
        let mut out = self.drain_outputs();
        self.run_held(&mut out)?;
        self.push_event(Event::Eos)?;
        self.drain_announcement();
        out.extend(self.drain_outputs());
        self.run_held(&mut out)?;
        Ok(out)
    }

    /// Run `process()` while the element holds the end, copying outputs off its pool so slots
    /// return.
    fn run_held(&mut self, out: &mut Vec<Buffer>) -> Result<(), Error> {
        let (mut idle, mut woken) = (0, 0);
        while self.ctx.take_end_held() && idle < 4 {
            self.run_process()?;
            let produced = self.drain_outputs();
            let waited = self.real_wakes && woken < WAKE_BUDGET && self.nap();
            woken += u32::from(waited);
            idle = match (produced.is_empty(), waited) {
                (false, _) => 0,
                (true, true) => idle,
                (true, false) => idle + 1,
            };
            for buf in produced {
                let Some(mut copy) = self.off_pool.acquire_exact(buf.memory.len()) else {
                    continue;
                };
                let n = buf.memory.len();
                copy.as_mut_full()[..n].copy_from_slice(buf.memory.data());
                copy.set_len(n);
                out.push(Buffer { memory: copy, ..buf });
            }
        }
        Ok(())
    }

    /// Sleeps out a `wake_within` the element asked for, `false` without one.
    // A test rig standing in for the scheduler's timed wait.
    #[allow(clippy::disallowed_methods)]
    fn nap(&mut self) -> bool {
        let within = self.ctx.take_wake_within();
        if within == std::time::Duration::MAX {
            return false;
        }
        std::thread::sleep(within.min(MAX_NAP));
        true
    }

    // Test-only collection helper.
    #[allow(clippy::disallowed_methods)]
    pub fn bus_messages(&mut self) -> Vec<BusMessage> {
        let mut out = Vec::new();
        while let Some(m) = self.bus.try_recv() {
            out.push(m);
        }
        out
    }

    pub fn prop(&self, name: &str) -> Option<profluens_core::format::Value> {
        self.ctx.prop(name)
    }

    // Test-only collection helper.
    #[allow(clippy::disallowed_methods)]
    pub fn drain_outputs(&mut self) -> Vec<Buffer> {
        let mut batch = self.ctx.take_output(SrcPad(0));
        let mut out = Vec::new();
        while let Some(b) = batch.pop_front() {
            out.push(b);
        }
        out
    }

    /// Unlike the scheduler this also resolves announcements after a failed `process()`.
    fn run_process(&mut self) -> Result<Flow, Error> {
        self.ctx.take_end_held();
        let flow = process_once(&mut *self.element, &mut self.ctx, true);
        let (announced, changes) = (&mut self.announced, &mut self.format_changes);
        after_process(&mut self.ctx, &self.vocabulary, |_, _, _, f| {
            *announced = Some(f);
            *changes += 1;
        });
        flow
    }

    fn drain_announcement(&mut self) {
        let (announced, changes) = (&mut self.announced, &mut self.format_changes);
        resolve_announcements(&mut self.ctx, &self.vocabulary, |_, _, _, f| {
            *announced = Some(f);
            *changes += 1;
        });
    }
}

fn new_ctx(pool: &Pool, bus: &BusSender) -> Ctx {
    Ctx::new(pool.clone(), bus.clone(), profluens_core::id::ElementId(0), BYTES, 1)
}

fn intern_constraint_values(c: &profluens_core::format::ConstraintDesc, values: &mut Interner) {
    use profluens_core::format::ConstraintDesc;
    match c {
        ConstraintDesc::Any => {}
        ConstraintDesc::Eq(v) => intern_value_desc(v, values),
        ConstraintDesc::Range { min, max, step } => {
            intern_value_desc(min, values);
            intern_value_desc(max, values);
            intern_value_desc(step, values);
        }
        ConstraintDesc::Set(vs) => {
            for v in *vs {
                intern_value_desc(v, values);
            }
        }
    }
}

fn intern_value_desc(v: &profluens_core::format::ValueDesc, values: &mut Interner) {
    if let profluens_core::format::ValueDesc::Id(s) = v {
        values.intern(s);
    }
}

/// The `Ctx` holds a second `Arc`, so clone the small tables into a unique one.
fn make_mut_vocabulary(this: &mut Arc<Vocabulary>) -> &mut Vocabulary {
    let cloned = Vocabulary {
        formats: this.formats.clone(),
        fields: this.fields.clone(),
        values: this.values.clone(),
    };
    *this = Arc::new(cloned);
    Arc::get_mut(this).expect("freshly-created Arc is unique")
}

#[cfg(test)]
mod tests {
    use super::*;
    use profluens_core::batch::Inputs;
    use profluens_core::element::{ElementDesc, InputPolicy, LatencyDesc, PadDesc, SchedHint};
    use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};

    static SAMPLE_VALUES: [ValueDesc; 2] = [ValueDesc::Id("s16"), ValueDesc::Id("s24")];
    static SRC_FIELDS: [FieldDesc; 3] = [
        FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc { field: "channels", allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc {
            field: "sample",
            allowed: ConstraintDesc::Set(&SAMPLE_VALUES),
            preferred: None,
        },
    ];
    static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &SRC_FIELDS }];
    static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
    static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
    static SRCS: [PadDesc; 2] = [
        PadDesc { name: "src", offers: &SRC_OFFERS },
        PadDesc { name: "aux", offers: &SINK_OFFERS },
    ];
    static DESC: ElementDesc = ElementDesc {
        name: "doubler",
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

    #[derive(Default)]
    struct Doubler {
        bogus: bool,
    }

    impl Doubler {
        const IN: SinkPad = SinkPad(0);
        const OUT: SrcPad = SrcPad(0);
        const AUX: SrcPad = SrcPad(1);
    }

    impl Element for Doubler {
        fn desc(&self) -> &'static ElementDesc {
            &DESC
        }
        fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
            if self.bogus {
                ctx.announce_format(Self::OUT, "video/never-offered", &[]);
                return Ok(());
            }
            let fields = [
                ("rate", ValueDesc::Int(48_000)),
                ("channels", ValueDesc::Int(2)),
                ("sample", ValueDesc::Id("s16")),
            ];
            ctx.announce_format(Self::OUT, "audio/raw", &fields);
            Ok(())
        }
        fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
            while let Some(inb) = inputs.pop() {
                let data = inb.memory.data();
                let n = data.len();
                let mut out = ctx.alloc();
                let dst = out.memory.as_mut_full();
                for (i, &b) in data.iter().enumerate() {
                    dst[i] = b.wrapping_mul(2);
                }
                out.memory.set_len(n);
                out.pts = ctx.now();
                ctx.out(Self::OUT).push(out);
            }
            Ok(Flow::Ok)
        }
        fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
            if matches!(event, Event::Eos) {
                let mut out = ctx.alloc();
                out.memory.as_mut_full()[0] = 0xEE;
                out.memory.set_len(1);
                ctx.out(Self::OUT).push(out);
                let mut aux = ctx.alloc();
                aux.memory.as_mut_full()[0] = 0xAA;
                aux.memory.set_len(1);
                ctx.out(Self::AUX).push(aux);
            }
            Ok(())
        }
    }

    #[test]
    fn push_pull_doubles_bytes() {
        let mut h = Harness::new(Doubler::default());
        let buf = h.alloc(&[1, 2, 3]);
        let flow = h.push(buf).unwrap();
        assert_eq!(flow, Flow::Ok);
        let out = h.pull(Doubler::OUT).expect("one output buffer");
        assert_eq!(out.memory.data(), &[2, 4, 6], "each byte doubled");
        assert!(h.pull(Doubler::OUT).is_none(), "exactly one buffer");
    }

    #[test]
    fn announce_resolves_through_the_vocabulary() {
        let mut h = Harness::new(Doubler::default());
        let buf = h.alloc(&[9]);
        let _ = h.push(buf).unwrap();
        let f = h.announced().expect("the element announced audio/raw");
        let rate = h.vocabulary.field_id("rate").unwrap();
        let sample = h.vocabulary.field_id("sample").unwrap();
        let s16 = h.vocabulary.value_id("s16").unwrap();
        assert_eq!(f.get(rate), Some(Value::Int(48_000)));
        assert_eq!(f.get(sample), Some(Value::Id(s16)));
        assert_eq!(h.vocabulary.family_name(f.family), Some("audio/raw"));
    }

    #[test]
    fn fix_format_installs_a_negotiated_edge_on_its_side_only() {
        let mut h = Harness::new(Doubler::default());
        h.fix_src_format(Doubler::OUT, "audio/raw", &[("rate", ValueDesc::Int(44_100))]);
        let rate = h.vocabulary.field_id("rate").unwrap();
        let neg = h.ctx.negotiated_src(Doubler::OUT).expect("format fixed on src");
        assert_eq!(neg.get(rate), Some(Value::Int(44_100)));
        assert!(h.ctx.negotiated_sink(Doubler::IN).is_none(), "the sink side is its own table");
        h.fix_sink_format(Doubler::IN, "bytes", &[]);
        assert!(h.ctx.negotiated_sink(Doubler::IN).is_some());
        let neg = h.ctx.negotiated_src(Doubler::OUT).expect("still fixed on src");
        assert_eq!(neg.get(rate), Some(Value::Int(44_100)), "and the src side kept its own");
    }

    #[test]
    fn clock_advance_drives_running_time_on_pts() {
        let mut h = Harness::new(Doubler::default());
        h.clock().advance(Timestamp::from_millis(10));
        let buf = h.alloc(&[1]);
        let _ = h.push(buf).unwrap();
        let out = h.pull(Doubler::OUT).unwrap();
        assert_eq!(out.pts, Timestamp::from_millis(10), "pts stamped with clock now()");
    }

    #[test]
    fn a_timed_element_shows_its_backlog_and_its_wakes() {
        static TIMED: ElementDesc = ElementDesc {
            name: "timed",
            sinks: &SINKS,
            srcs: &[],
            props: &[],
            sched: SchedHint::Active,
            inputs: InputPolicy::Single,
            latency: LatencyDesc::NONE,
            make_default: None,
            io: false,
            couplings: &[],
        };
        struct Timed;
        impl Element for Timed {
            fn desc(&self) -> &'static ElementDesc {
                &TIMED
            }
            fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
                ctx.wake_within(std::time::Duration::from_millis(8));
                while let Some(front) = inputs.front() {
                    if !ctx.due(front.pts) {
                        break;
                    }
                    let _ = inputs.pop();
                }
                Ok(Flow::Ok)
            }
        }
        let mut h = Harness::new(Timed);
        let none = (Timestamp::NONE, std::time::Duration::MAX);
        assert_eq!((h.input_len(), h.take_wakes()), (0, none));
        let mut early = h.alloc(&[1]);
        early.pts = Timestamp::from_millis(10);
        h.push(early).unwrap();
        let within = std::time::Duration::from_millis(8);
        assert_eq!((h.input_len(), h.take_wakes()), (1, (Timestamp::from_millis(10), within)));
        assert_eq!(h.take_wakes(), none, "taken");
        h.clock().advance(Timestamp::from_millis(10));
        h.crank().unwrap();
        assert_eq!((h.input_len(), h.take_wakes()), (0, (Timestamp::NONE, within)));
    }

    #[test]
    fn eos_flushes_the_tail_marker() {
        let mut h = Harness::new(Doubler::default());
        let buf = h.alloc(&[1]);
        let _ = h.push(buf).unwrap();
        assert_eq!(h.pull(Doubler::OUT).unwrap().memory.data(), &[2]);
        let tail = h.eos().unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].memory.data(), &[0xEE], "EOS tail marker flushed");
    }

    #[test]
    fn pull_reads_the_pad_it_is_given() {
        let mut h = Harness::new(Doubler::default());
        let buf = h.alloc(&[1]);
        let _ = h.push(buf).unwrap();
        assert!(h.pull(Doubler::AUX).is_none(), "data went to OUT only");
        h.push_event(Event::Eos).unwrap();
        assert_eq!(h.pull(Doubler::AUX).unwrap().memory.data(), &[0xAA]);
        assert!(h.pull(Doubler::AUX).is_none());
        assert_eq!(h.pull(Doubler::OUT).unwrap().memory.data(), &[2]);
        assert_eq!(h.pull(Doubler::OUT).unwrap().memory.data(), &[0xEE]);
    }

    #[test]
    fn crank_runs_process_without_input() {
        let mut h = Harness::new(Doubler::default());
        let flow = h.crank().unwrap();
        assert_eq!(flow, Flow::Ok);
        assert!(h.pull(Doubler::OUT).is_none(), "no input ⇒ no output");
        assert!(h.announced().is_some(), "but the announce still fired");
    }

    #[test]
    fn unresolvable_announcement_warns_once_and_caches_nothing() {
        let mut h = Harness::new(Doubler { bogus: true });
        h.crank().unwrap();
        assert!(h.announced().is_none(), "an unresolvable name reaches no sink");
        let warnings = h
            .bus_messages()
            .into_iter()
            .filter(|m| matches!(m, BusMessage::Warning { .. }))
            .count();
        assert_eq!(warnings, 1, "the shared resolver posts the scheduler's Warning");
    }
}
