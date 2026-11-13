// Test fixtures.
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::Event::{Output, Release};
use crate::av1::write::{
    Frame as EncFrame, Sequence, Tiles, frame_header, sequence_header, tile_group,
};

const SEQ: Sequence = Sequence {
    level_idx: 8,
    tier: 0,
    width: 640,
    height: 352,
    ten_bit: true,
    enable_cdef: true,
    color: None,
    full_range: false,
};

fn key() -> EncFrame {
    EncFrame {
        key: true,
        order_hint: 0,
        refresh_frame_flags: 0xff,
        primary_ref_frame: 7,
        ref_frame_idx: [0; 7],
        base_q_idx: 90,
        tx_mode_select: true,
    }
}

fn inter(order_hint: u8, refresh: u8, refs: [u8; 7]) -> EncFrame {
    EncFrame {
        key: false,
        order_hint,
        refresh_frame_flags: refresh,
        primary_ref_frame: 0,
        ref_frame_idx: refs,
        base_q_idx: 90,
        tx_mode_select: true,
    }
}

/// A temporal unit of a sequence header and the frames, each one tile.
fn tu(frames: &[EncFrame]) -> Vec<u8> {
    let tiles = Tiles::single(SEQ.width, SEQ.height).unwrap();
    let mut v = sequence_header(&SEQ, 2);
    for f in frames {
        v.extend(frame_header(&SEQ, f, &tiles, 2).bytes);
        v.extend(tile_group(&[1, 2, 3], 2));
    }
    v
}

/// One frame through `begin` and `end` in `slot` unless skipped, the fate and events of both.
fn run(d: &mut Dpb, h: &FrameHeader, slot: u16) -> (Fate, Vec<Event>) {
    let mut ev = Events::new();
    let fate = d.begin(h, &mut ev).unwrap();
    let mut all = ev.as_slice().to_vec();
    if fate != Fate::Skip {
        d.end(h, fate, Slot(slot), &mut ev);
        all.extend_from_slice(ev.as_slice());
    }
    (fate, all)
}

/// Every frame of `tu`, decoded into slots from `slot` on.
fn feed(d: &mut Dpb, tu: &[u8], mut slot: u16) -> Vec<(Fate, Vec<Event>)> {
    let mut at = 0;
    let mut out = Vec::new();
    while let Some(f) = d.next_frame(tu, &mut at).unwrap() {
        out.push(run(d, &f.header, slot));
        slot += 1;
    }
    out
}

/// The frame headers of `tu` as a parser takes them.
fn headers(tu: &[u8]) -> Vec<FrameHeader> {
    let mut d = Dpb::new();
    let mut at = 0;
    let mut out = Vec::new();
    while let Some(f) = d.next_frame(tu, &mut at).unwrap() {
        out.push(f.header);
    }
    out
}

fn held(d: &Dpb) -> Vec<Option<u16>> {
    d.slots().iter().map(|s| s.map(|r| r.slot.0)).collect()
}

#[test]
fn frames_go_out_in_decode_order_and_leave_their_slots() {
    let mut d = Dpb::new();
    let r = feed(
        &mut d,
        &tu(&[key(), inter(1, 0x01, [0; 7]), inter(2, 0xfe, [0; 7]), inter(3, 0, [1; 7])]),
        0,
    );
    use Fate::Shown;
    assert_eq!(
        r,
        [
            (Shown, vec![Output(Slot(0))]),
            (Shown, vec![Output(Slot(1))]),
            // the key frame leaves its last slots
            (Shown, vec![Output(Slot(2)), Release(Slot(0))]),
            // no slot takes it
            (Shown, vec![Output(Slot(3)), Release(Slot(3))]),
        ]
    );
    assert_eq!(held(&d), [Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2)]);
    // the parser's slots took the last frame once the unit ran out
    let hints: Vec<u8> = d.stream().refs.iter().map(|r| r.order_hint).collect();
    assert_eq!(hints, [1, 2, 2, 2, 2, 2, 2, 2]);
    let refs = d.refs(&headers(&tu(&[key(), inter(3, 0, [1; 7])]))[1]).unwrap();
    assert_eq!(refs.map(|r| r.slot.0), [2; NUM_REFS]);
}

#[test]
fn frames_before_the_first_key_frame_are_skipped() {
    let h = headers(&tu(&[key(), inter(1, 0x01, [0; 7])]));
    let mut d = Dpb::new();
    assert_eq!(run(&mut d, &h[1], 0), (Fate::Skip, vec![]));
    assert_eq!(run(&mut d, &h[0], 1), (Fate::Shown, vec![Output(Slot(1))]));
    assert_eq!(run(&mut d, &h[1], 2), (Fate::Shown, vec![Output(Slot(2))]));
    // a parse error waits for the next key frame
    let mut at = 0;
    assert!(d.next_frame(&[0xff; 4], &mut at).is_err());
    assert_eq!(run(&mut d, &h[1], 3), (Fate::Skip, vec![]));
    let r = feed(&mut d, &tu(&[key(), inter(1, 0x01, [0; 7])]), 4);
    assert_eq!(r[0], (Fate::Shown, vec![Output(Slot(4)), Release(Slot(2)), Release(Slot(1))]));
    assert_eq!(r[1], (Fate::Shown, vec![Output(Slot(5))]));
}

#[test]
fn a_hidden_frame_goes_out_when_shown_again() {
    let h = headers(&tu(&[key(), inter(1, 0x02, [0; 7])]));
    let mut d = Dpb::new();
    run(&mut d, &h[0], 0);
    let mut hidden = h[1];
    (hidden.show_frame, hidden.showable_frame) = (false, true);
    assert_eq!(run(&mut d, &hidden, 1), (Fate::Hidden, vec![]));
    let mut again = h[1];
    again.show_existing_frame = true;
    again.frame_to_show_map_idx = 1;
    again.refresh_frame_flags = 0;
    assert_eq!(run(&mut d, &again, 9), (Fate::Skip, vec![Output(Slot(1))]));
    assert_eq!(held(&d)[1], Some(1));
    // a shown key frame is not showable, nor a hidden frame no slot takes
    again.frame_to_show_map_idx = 0;
    let mut ev = Events::new();
    assert_eq!(d.begin(&again, &mut ev), Err(DpbError::NotShowable));
    hidden.refresh_frame_flags = 0;
    assert_eq!(run(&mut d, &hidden, 2), (Fate::Hidden, vec![Release(Slot(2))]));
}

#[test]
fn a_key_frame_shown_again_takes_every_slot_once() {
    let h = headers(&tu(&[key(), inter(1, 0x02, [0; 7])]));
    let mut d = Dpb::new();
    run(&mut d, &h[0], 0);
    // a hidden key frame into slot 2
    let mut hidden_key = h[0];
    hidden_key.show_frame = false;
    hidden_key.showable_frame = true;
    hidden_key.refresh_frame_flags = 0x04;
    assert_eq!(run(&mut d, &hidden_key, 1), (Fate::Hidden, vec![]));
    // shown again after a seek landed on it
    d.awaiting_key = true;
    let mut again = h[0];
    again.show_existing_frame = true;
    again.frame_to_show_map_idx = 2;
    assert_eq!(again.refresh_frame_flags, 0xff);
    assert_eq!(run(&mut d, &again, 9), (Fate::Skip, vec![Output(Slot(1)), Release(Slot(0))]));
    assert_eq!(held(&d), [Some(1); NUM_SLOTS]);
    assert!(d.slots().iter().flatten().all(|r| !r.showable));
    assert_eq!(run(&mut d, &h[1], 2).0, Fate::Shown);
    let mut ev = Events::new();
    assert_eq!(d.begin(&again, &mut ev), Err(DpbError::NotShowable));
}

#[test]
fn an_empty_slot_is_a_missing_reference_and_changes_nothing() {
    let h = headers(&tu(&[key(), inter(1, 0x02, [0, 0, 0, 3, 0, 0, 0])]));
    let mut d = Dpb::new();
    run(&mut d, &h[0], 0);
    d.slots[3] = None;
    let mut again = h[1];
    again.show_existing_frame = true;
    again.frame_to_show_map_idx = 3;
    let mut ev = Events::new();
    assert_eq!(d.begin(&h[1], &mut ev), Err(DpbError::MissingReference));
    assert_eq!(d.begin(&again, &mut ev), Err(DpbError::MissingReference));
    assert!(ev.as_slice().is_empty());
    assert_eq!(held(&d)[..3], [Some(0); 3]);
}

#[test]
fn discard_releases_each_frame_once_and_resets_the_parser() {
    let mut d = Dpb::new();
    feed(&mut d, &tu(&[key(), inter(1, 0x0f, [0; 7])]), 0);
    let mut ev = Events::new();
    d.discard(&mut ev);
    assert_eq!(ev.as_slice(), [Release(Slot(1)), Release(Slot(0))]);
    assert_eq!(held(&d), [None; NUM_SLOTS]);
    assert!(d.stream().refs.iter().all(|r| !r.valid));
    let h = headers(&tu(&[key(), inter(1, 0x02, [0; 7])]));
    assert_eq!(run(&mut d, &h[1], 2), (Fate::Skip, vec![]));
}

#[test]
fn after_a_failed_frame_frames_parse_and_skip_until_a_key() {
    let mut d = Dpb::new();
    feed(&mut d, &tu(&[key()]), 0);
    d.await_key();
    let r = feed(&mut d, &tu(&[inter(1, 0x01, [0; 7])]), 1);
    assert_eq!(r, [(Fate::Skip, vec![])]);
    assert_eq!(held(&d), [Some(0); NUM_SLOTS]);
    // the parser took the skipped frame
    assert_eq!(d.stream().refs[0].order_hint, 1);
    let r = feed(&mut d, &tu(&[key()]), 2);
    assert_eq!(r, [(Fate::Shown, vec![Output(Slot(2)), Release(Slot(0))])]);
}

#[test]
fn awaiting_key_follows_the_wait() {
    let mut d = Dpb::new();
    assert!(d.awaiting_key());
    let h = headers(&tu(&[key(), inter(1, 0x01, [0; 7])]));
    assert_eq!(run(&mut d, &h[1], 0).0, Fate::Skip);
    assert!(d.awaiting_key());
    run(&mut d, &h[0], 1);
    assert!(!d.awaiting_key());
    d.await_key();
    assert!(d.awaiting_key());
    run(&mut d, &h[0], 2);
    let mut at = 0;
    assert!(d.next_frame(&[0xff; 4], &mut at).is_err());
    assert!(d.awaiting_key());
}

#[test]
fn drop_frames_empties_the_slots_and_keeps_the_parser() {
    let mut d = Dpb::new();
    feed(&mut d, &tu(&[key(), inter(1, 0x0f, [0; 7])]), 0);
    let mut ev = Events::new();
    d.drop_frames(&mut ev);
    assert_eq!(ev.as_slice(), [Release(Slot(1)), Release(Slot(0))]);
    assert_eq!(held(&d), [None; NUM_SLOTS]);
    assert!(!d.awaiting_key());
    // the next unit parses against the parser's slots, the inter frame took 0 to 3
    let unit = tu(&[inter(2, 0x10, [3; 7]), key()]);
    let mut at = 0;
    let f = d.next_frame(&unit, &mut at).unwrap().unwrap();
    assert!(d.stream().refs.iter().all(|r| r.valid));
    assert_eq!(d.stream().refs[3].order_hint, 1);
    assert_eq!(d.begin(&f.header, &mut ev), Err(DpbError::MissingReference));
    let f = d.next_frame(&unit, &mut at).unwrap().unwrap();
    assert_eq!(run(&mut d, &f.header, 5), (Fate::Shown, vec![Output(Slot(5))]));
    assert_eq!(held(&d), [Some(5); NUM_SLOTS]);
}

#[test]
fn the_sequence_header_of_a_unit() {
    // a temporal delimiter and a metadata OBU ahead of it
    let mut unit = vec![0x12, 0x00, 5 << 3 | 0x02, 2, 1, 0x80];
    assert_eq!(sequence_of(&unit), None);
    unit.extend(sequence_header(&SEQ, 2));
    let s = sequence_of(&unit).expect("a sequence header");
    assert_eq!((s.max_frame_width, s.max_frame_height, s.color.bit_depth), (640, 352, 10));
    // a broken OBU before it hides it, as the parser stops there
    let mut bad = vec![0x80];
    bad.extend(sequence_header(&SEQ, 2));
    assert_eq!(sequence_of(&bad), None);
}

#[test]
fn a_skipped_frame_ends_with_nothing() {
    let h = headers(&tu(&[key(), inter(1, 0x02, [0; 7])]));
    let mut d = Dpb::new();
    run(&mut d, &h[0], 0);
    let mut hidden = h[1];
    (hidden.show_frame, hidden.showable_frame) = (false, true);
    assert_eq!(run(&mut d, &hidden, 1), (Fate::Hidden, vec![]));
    let mut again = h[1];
    again.show_existing_frame = true;
    again.frame_to_show_map_idx = 1;
    again.refresh_frame_flags = 0;
    let mut ev = Events::new();
    assert_eq!(d.begin(&again, &mut ev), Ok(Fate::Skip));
    assert_eq!(ev.as_slice(), [Output(Slot(1))]);
    d.end(&again, Fate::Skip, Slot(9), &mut ev);
    assert_eq!(ev.as_slice(), []);
    assert_eq!(held(&d), [Some(0), Some(1), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0)]);
    // before the first key frame likewise
    let mut d = Dpb::new();
    assert_eq!(d.begin(&h[1], &mut ev), Ok(Fate::Skip));
    d.end(&h[1], Fate::Skip, Slot(9), &mut ev);
    assert_eq!(ev.as_slice(), []);
    assert_eq!(held(&d), [None; NUM_SLOTS]);
}

#[test]
fn an_abandoned_frame_empties_its_slots() {
    let h = headers(&tu(&[
        key(),
        inter(1, 0x02, [0; 7]),
        inter(2, 0, [0, 1, 0, 0, 0, 0, 0]),
        inter(3, 0, [0; 7]),
    ]));
    let mut d = Dpb::new();
    run(&mut d, &h[0], 0);
    let mut ev = Events::new();
    assert_eq!(d.begin(&h[1], &mut ev), Ok(Fate::Shown));
    d.abandon(&h[1], &mut ev);
    assert_eq!(ev.as_slice(), []);
    assert_eq!(held(&d)[1], None);
    assert!(!d.awaiting_key());
    // a frame naming the emptied slot is refused, one naming the others decodes
    assert_eq!(d.begin(&h[2], &mut ev), Err(DpbError::MissingReference));
    assert_eq!(run(&mut d, &h[3], 1), (Fate::Shown, vec![Output(Slot(1)), Release(Slot(1))]));
    // what no slot holds any more is released
    let mut all = h[3];
    all.refresh_frame_flags = 0xff;
    assert_eq!(d.begin(&all, &mut ev), Ok(Fate::Shown));
    d.abandon(&all, &mut ev);
    assert_eq!(ev.as_slice(), [Release(Slot(0))]);
    assert_eq!(held(&d), [None; NUM_SLOTS]);
}
