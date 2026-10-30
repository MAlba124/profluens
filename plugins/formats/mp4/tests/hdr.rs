// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use profluens_video::color::{Matrix, Primaries, Range, TransferFn};

#[test]
fn an_hvc1_entry_states_its_colr() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../video/hdr/tests/fixtures/hdr10_x265.mp4");
    let bytes = std::fs::read(path).expect("the fixture");
    let reader = pf_mp4::Mp4Reader::new(&bytes).expect("the tables");
    let e = &reader.tracks()[0].entry;
    assert_eq!(e.family, "h265/annexb");
    let c = e.colour;
    assert_eq!(
        (c.matrix, c.transfer, c.primaries, c.range),
        (Some(Matrix::Bt2020), Some(TransferFn::Pq), Some(Primaries::Bt2020), Some(Range::Limited))
    );
    assert!(c.hdr.is_empty(), "ffmpeg writes no mdcv or clli here");
    assert_eq!(c.dovi, None);
}
