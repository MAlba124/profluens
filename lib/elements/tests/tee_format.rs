//! `tee` re-announces a format change on every branch it feeds, and no other pad.

use pf_harness::Harness;
use profluens_elements::flow::Tee;

fn changes_after_one_format_change(n: usize) -> u32 {
    let mut h = Harness::new(Tee::new(n));
    h.change_sink_format(Tee::IN, "bytes", &[]).expect("format change");
    h.crank().expect("crank");
    h.format_changes()
}

#[test]
fn a_format_change_reaches_every_branch() {
    for n in 1..=8 {
        assert_eq!(changes_after_one_format_change(n), n as u32, "{n} branches");
    }
}
