//! Regression test. Waiting at a growing file's frontier must cost no CPU. Its own binary,
//! so process CPU counts only this pipeline. Fixture setup does blocking IO.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;
use profluens_elements::io::GrowingFileSrc;

/// Process CPU time in ms, which unlike wall time ignores machine load.
fn process_cpu_ms() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("read /proc/self/stat");
    let after = stat.rsplit(')').next().expect("stat has a comm field");
    let fields: Vec<&str> = after.split_whitespace().collect();
    let utime: u64 = fields[11].parse().expect("utime");
    let stime: u64 = fields[12].parse().expect("stime");
    // USER_HZ is 100 on every Linux ABI this crate targets.
    (utime + stime) * 1000 / 100
}

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

static COUNT_DESC: ElementDesc = ElementDesc {
    name: "growingcountsink",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct CountSink(Arc<Mutex<u64>>);

impl CountSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for CountSink {
    fn desc(&self) -> &'static ElementDesc {
        &COUNT_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut n = self.0.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            *n += buf.memory.len() as u64;
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn waiting_at_the_frontier_costs_no_cpu() {
    const HAVE: usize = 64_000;
    let path = std::env::temp_dir().join(format!("pf_growing_cpu_{}.bin", std::process::id()));
    File::create(&path).expect("create");
    let data: Vec<u8> = (0..HAVE).map(|i| (i as u64).wrapping_mul(0x9E37_79B9) as u8).collect();
    {
        let mut f = OpenOptions::new().append(true).open(&path).expect("append open");
        f.write_all(&data).expect("write");
        f.sync_data().expect("sync");
    }

    let (src, frontier) = GrowingFileSrc::new(&path);
    let received = Arc::new(Mutex::new(0u64));
    let mut p = pf_reactor::pipeline();
    let s = p.add(src);
    let k = p.add(CountSink(Arc::clone(&received)));
    p.link(s.src(GrowingFileSrc::OUT), k.sink(CountSink::IN)).expect("link");
    frontier.advance(HAVE as u64);

    let cpu_before = process_cpu_ms();
    let t0 = std::time::Instant::now();
    let watcher = Arc::clone(&received);
    let run = std::thread::spawn(move || p.run());

    for _ in 0..500 {
        if *watcher.lock().unwrap() == HAVE as u64 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(1000));
    frontier.finish(HAVE as u64);
    run.join().expect("joined").expect("run");

    let wall = t0.elapsed();
    let cpu = process_cpu_ms().saturating_sub(cpu_before);
    assert_eq!(*received.lock().unwrap(), HAVE as u64, "every published byte was delivered");
    assert!(wall >= Duration::from_millis(900), "the frontier wait really happened: {wall:?}");
    // A spin burns > 1000 ms. Parked groups leave only ~100 wakeups per group.
    assert!(cpu < 400, "the frontier wait burned {cpu} ms CPU over a ~{wall:?} run, spinning?");
    let _ = std::fs::remove_file(&path);
}
