use super::*;
use profluens_video::bits::BitWriter;

const STEP: u64 = 40;

/// A VOL of vop_time_increment_resolution 25, five bits of time increment.
fn vol() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.flag(false);
    w.u(1, 8);
    w.flag(false);
    w.u(1, 4);
    w.flag(false);
    w.u(0, 2);
    w.flag(true);
    w.u(25, 16);
    w.flag(true);
    w.flag(false);
    w.u(0, 7);
    let mut out = vec![0, 0, 1, 0x20];
    out.extend_from_slice(w.as_bytes());
    out
}

/// A VOP of coding type `t` (0 I, 1 P, 2 B), coded or an N-VOP, its body `extra` bytes.
fn vop(t: u32, coded: bool, extra: usize) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u(t, 2);
    w.flag(false);
    w.flag(true);
    w.u(3, 5);
    w.flag(true);
    w.flag(coded);
    w.u(0x7F, 7);
    let mut out = vec![0, 0, 1, 0xB6];
    out.extend_from_slice(w.as_bytes());
    out.resize(out.len() + extra, 0x55);
    out
}

fn packed(a: u32, b: u32) -> Vec<u8> {
    [vop(a, true, 8), vop(b, true, 4)].concat()
}

/// The chunks at their times through a retimer, each unit's stamp in decode order with the
/// acts the chunks gave, units out as they are stamped.
fn run(head: &[u8], chunks: &[(Vec<u8>, u64)]) -> (Vec<u64>, Vec<Act>) {
    let mut r = Retime::new(Order::Vop, STEP, head);
    let (mut stamps, mut all, mut acts) = (Vec::new(), Vec::new(), Vec::new());
    for (c, at) in chunks {
        acts.clear();
        r.chunk(c, *at, &mut acts);
        all.extend_from_slice(&acts);
        stamps.extend(std::iter::from_fn(|| r.take()));
    }
    acts.clear();
    r.finish(&mut acts);
    all.extend_from_slice(&acts);
    stamps.extend(std::iter::from_fn(|| r.take()));
    assert_eq!(r.held(), 0);
    (stamps, all)
}

#[test]
fn the_vol_sizes_the_time_increment() {
    assert_eq!(vol_time_bits(&vol(), 0), Some(5));
}

/// B-VOPs stored in decode order show before the I or P they follow, each picture taking the
/// k-th chunk time.
#[test]
fn b_vops_take_the_times_of_the_pictures_they_show_between() {
    let kinds = [0, 1, 2, 2, 1, 2, 2, 0, 2, 2];
    let chunks: Vec<_> =
        kinds.iter().enumerate().map(|(i, &t)| (vop(t, true, 6), i as u64 * STEP)).collect();
    let (stamps, acts) = run(&vol(), &chunks);
    let want: Vec<u64> = [0, 3, 1, 2, 6, 4, 5, 9, 7, 8].iter().map(|k| k * STEP).collect();
    assert_eq!(stamps, want);
    assert!(acts.iter().all(|a| matches!(a, Act::Unit { .. })));
    assert_eq!(acts[0], Act::Unit { from: 0, to: chunks[0].0.len(), key: Some(true) });
    assert_eq!(acts[1], Act::Unit { from: 0, to: chunks[1].0.len(), key: Some(false) });
}

/// A stream without B-VOPs keeps its chunk times.
#[test]
fn a_stream_of_i_and_p_vops_keeps_its_times() {
    let chunks: Vec<_> =
        (0..6).map(|i| (vop(if i == 0 { 0 } else { 1 }, true, 6), i * STEP)).collect();
    let (stamps, _) = run(&vol(), &chunks);
    assert_eq!(stamps, (0..6).map(|i| i * STEP).collect::<Vec<_>>());
}

/// XviD's packed stream, a P and a B in one chunk, the B-VOPs a chunk late, an N-VOP where the
/// P shows. Unpacked, each picture shows at the chunk time VfW showed it at.
#[test]
fn packed_b_vops_are_unpacked_and_shown_where_their_chunks_stood() {
    let t = |k: u64| k * STEP;
    let chunks = vec![
        ([vol(), vop(0, true, 20)].concat(), t(0)),
        (packed(1, 2), t(3)),
        (vop(2, true, 3), t(4)),
        (vop(1, false, 0), t(5)),
        (packed(1, 2), t(6)),
        (vop(2, true, 3), t(7)),
        (vop(1, false, 0), t(8)),
    ];
    let (stamps, acts) = run(&[], &chunks);
    // I0, P3, B1, B2, P6, B4, B5 in decode order
    assert_eq!(stamps, [0, 5, 3, 4, 8, 6, 7].map(t));
    let p = vop(1, true, 8).len();
    assert_eq!(
        &acts[..5],
        [
            Act::Unit { from: 0, to: chunks[0].0.len(), key: Some(true) },
            Act::Unit { from: 0, to: p, key: Some(false) },
            Act::Stash { from: p, to: chunks[1].0.len() },
            Act::Unstash,
            Act::Stash { from: 0, to: chunks[2].0.len() },
        ]
    );
    assert_eq!(acts[5], Act::Unstash, "the N-VOP gives way to the B-VOP");
    assert_eq!(acts.len(), 11);
}

/// DivX holds the packed B-VOP's place with a stuffing byte, a stash with no chunk after it
/// goes at the end, and one whose placeholder never came is dropped.
#[test]
fn placeholders_without_a_vop_hold_a_place_and_a_stash_left_goes() {
    let t = |k: u64| k * STEP;
    let chunks = vec![
        ([vol(), vop(0, true, 20)].concat(), t(0)),
        (packed(1, 2), t(1)),
        (vec![0x7F], t(2)),
        (packed(1, 2), t(3)),
        (packed(1, 2), t(4)),
    ];
    let (stamps, acts) = run(&[], &chunks);
    assert!(acts.contains(&Act::Drop), "{acts:?}");
    // I, P, B, P (its B dropped), P, B in decode order, the last B at the end
    assert_eq!(stamps.len(), 6);
    assert_eq!(&stamps[..3], [0, 2, 1].map(t));
    assert_eq!(acts.last(), Some(&Act::Unstash));
}

/// Without a VOL an N-VOP is told by its size, a repeat of the last picture is dropped.
#[test]
fn an_n_vop_outside_a_packed_stream_is_dropped() {
    let chunks = vec![(vop(0, true, 20), 0), (vop(1, false, 0), STEP), (vop(1, true, 9), 2 * STEP)];
    let (stamps, acts) = run(&[], &chunks);
    assert_eq!(stamps, [0, 2 * STEP]);
    assert_eq!(acts.len(), 2);
}

/// A seek forgets what waits, the next run's units are stamped from its chunks alone.
#[test]
fn a_reset_forgets_the_run_before() {
    let mut r = Retime::new(Order::Vop, STEP, &vol());
    let mut acts = Vec::new();
    r.chunk(&vop(0, true, 6), 0, &mut acts);
    r.chunk(&vop(1, true, 6), STEP, &mut acts);
    assert_eq!(r.take(), Some(0));
    assert_eq!(r.held(), 1);
    r.reset();
    assert_eq!(r.held(), 0);
    r.chunk(&vop(0, true, 6), 10 * STEP, &mut acts);
    r.finish(&mut acts);
    assert_eq!(r.take(), Some(10 * STEP));
}

/// The video chunks of an AVI, in file order.
fn chunks_of(path: &str) -> Vec<Vec<u8>> {
    let d = std::fs::read(path).unwrap();
    let mut at = d.windows(4).position(|w| w == b"movi").unwrap() + 4;
    let mut out = Vec::new();
    while at + 8 <= d.len() {
        let id = &d[at..at + 4];
        let n = u32::from_le_bytes([d[at + 4], d[at + 5], d[at + 6], d[at + 7]]) as usize;
        if id == b"idx1" || id == b"LIST" {
            break;
        }
        if &id[2..] == b"dc" && n > 0 {
            out.push(d[at + 8..at + 8 + n].to_vec());
        }
        at += 8 + n + (n & 1);
    }
    out
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// x264's B-frames in an AVI stamped as the same encode in Matroska is, by picture order count.
#[test]
fn h264_pictures_are_stamped_in_presentation_order_from_their_poc() {
    let chunks = chunks_of(&fixture("h264_bf2.avi"));
    let mut r = Retime::new(Order::Poc, STEP, &[]);
    let (mut stamps, mut acts) = (Vec::new(), Vec::new());
    for (i, c) in chunks.iter().enumerate() {
        r.chunk(c, i as u64 * STEP, &mut acts);
        stamps.extend(std::iter::from_fn(|| r.take()));
    }
    r.finish(&mut acts);
    stamps.extend(std::iter::from_fn(|| r.take()));
    // ffprobe's packet pts of the same encode muxed to Matroska, ms
    let want = [
        0, 120, 40, 80, 240, 160, 200, 360, 280, 320, 440, 400, 480, 600, 520, 560, 720, 640, 680,
        840, 760, 800, 920, 880, 960,
    ];
    assert_eq!(stamps, want);
    assert!(acts.iter().all(|a| matches!(a, Act::Unit { key: None, .. })));
}

/// ffmpeg's MPEG-4 part 2 with two B-VOPs, an open GOP at the second I-VOP.
#[test]
fn mpeg4_vops_are_stamped_as_the_same_encode_in_matroska() {
    let chunks = chunks_of(&fixture("mpeg4_bf2.avi"));
    let timed: Vec<_> = chunks.into_iter().enumerate().map(|(i, c)| (c, i as u64 * STEP)).collect();
    let (stamps, _) = run(&[], &timed);
    let want = [
        0, 120, 40, 80, 240, 160, 200, 360, 280, 320, 480, 400, 440, 600, 520, 560, 720, 640, 680,
        840, 760, 800, 960, 880, 920,
    ];
    assert_eq!(stamps, want);
}

/// XviD's packed file, every coded VOP once, in presentation order by their stamps.
#[test]
fn an_xvid_packed_file_unpacks_to_each_picture_once() {
    let d = std::fs::read(fixture("xvid_bf2.avi")).unwrap();
    let movi = d.windows(4).position(|w| w == b"movi").unwrap() + 4;
    // an empty chunk takes no time on the demuxer's clock
    let mut timed = Vec::new();
    let mut at = movi;
    while at + 8 <= d.len() && &d[at..at + 4] != b"idx1" {
        let n = u32::from_le_bytes([d[at + 4], d[at + 5], d[at + 6], d[at + 7]]) as usize;
        if n > 0 {
            timed.push((d[at + 8..at + 8 + n].to_vec(), timed.len() as u64 * STEP));
        }
        at += 8 + n + (n & 1);
    }
    let (stamps, acts) = run(&[], &timed);
    let units = acts.iter().filter(|a| matches!(a, Act::Unit { .. } | Act::Unstash)).count();
    assert_eq!(stamps.len(), units);
    // VfW's one chunk a frame never let the last B-VOP out
    assert_eq!(units, 24, "every coded picture once");
    let mut sorted = stamps.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), stamps.len(), "no two at one time, {stamps:?}");
    assert_eq!(&stamps[..4], [0, 3, 1, 2].map(|k| k * STEP));
}

/// A first unit that never shows, a corrupt stream's, holds no more than `MOST_HELD`.
#[test]
fn a_unit_that_never_shows_holds_a_bounded_queue() {
    let mut r = Retime::new(Order::Vop, STEP, &vol());
    let mut acts = Vec::new();
    r.chunk(&vop(0, true, 6), 0, &mut acts);
    let mut out = 0;
    for i in 1..200u64 {
        r.chunk(&vop(2, true, 6), i * STEP, &mut acts);
        out += std::iter::from_fn(|| r.take()).count();
        assert!(r.held() <= MOST_HELD + 1, "{}", r.held());
    }
    assert!(out > 150);
}

/// A chunk without a VOP has nothing to order by, it goes as it came, a lone stuffing byte
/// with no B-VOP waiting goes nowhere.
#[test]
fn a_chunk_without_a_vop_goes_as_it_came() {
    let chunks = vec![(vec![7u8; 30], 0), (vec![0x7F], STEP), (vec![8u8; 30], 2 * STEP)];
    let (stamps, acts) = run(&[], &chunks);
    assert_eq!(stamps, [0, 2 * STEP]);
    assert_eq!(acts, [Act::Unit { from: 0, to: 30, key: None }; 2]);
}
