#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_open::{Caps, Kind, Meta, OpenAt, Opened, Want};
use profluens_core::fetch::{Need, drive_slice_log};

/// The first read, picked by the knob's low bits.
const WINDOWS: [usize; 8] = [1, 12, 100, 1024, 4096, 65536, 256 << 10, 4 << 20];
/// No single read of an open asks more, the largest cap is the head's.
const MAX_NEED: usize = 128 << 20;

// Input is a knob byte then the file. Bits 0-2 pick the window, bit 3 the index, bit 4 the
// meta, bit 5 a tail cap of 0, bit 6 the HDR10+ key read. Every span lies in the arena, the head
// is the file's start, the meta's spans lie in what holds them and no read asks past the caps
// or for nothing.
fuzz_target!(|data: &[u8]| {
    let Some((&knob, file)) = data.split_first() else { return };
    let want = Want { index: knob & 8 != 0, meta: knob & 16 != 0, hdr10plus: knob & 64 != 0 };
    let caps = Caps {
        window: WINDOWS[usize::from(knob & 7)],
        max_tail: if knob & 32 != 0 { 0 } else { 4 << 20 },
        ..Caps::default()
    };
    let mut log = Vec::new();
    let got = drive_slice_log(&mut OpenAt::new(file.len() as u64, want, caps), file, &mut log);
    assert!(log.iter().all(|n: &Need| n.len <= MAX_NEED && n.len > 0), "{log:?}");
    if let Ok(o) = got {
        check(&o, file, want);
    }
});

fn check(o: &Opened, file: &[u8], want: Want) {
    // an MP4 head has a moov behind the mdat appended
    let start = o.kind == Kind::Mp4 || file.starts_with(&o.head);
    assert!(start, "a head that is not the file's start");
    assert!(!o.tracks.is_empty(), "an open without tracks");
    for t in &o.tracks {
        for s in [t.head, t.language, t.title] {
            assert!(s.range().end <= o.arena.len(), "{s:?} past the arena");
        }
        assert!(t.par.0 > 0 && t.par.1 > 0, "{t:?}");
        let _ = (o.language(t), o.title(t), o.codec_head(t));
    }
    match &o.meta {
        Meta::None => {}
        Meta::Moov(r) => assert!(r.end <= o.head.len() && o.kind == Kind::Mp4),
        Meta::Ogg(s) => assert!(s.range().end <= o.arena.len() && o.kind == Kind::Ogg),
        Meta::Mkv(_) => assert_eq!(o.kind, Kind::Mkv),
        Meta::Bare { raw, .. } => {
            assert!(raw.comments.iter().chain(&raw.pictures).all(|r| r.end <= raw.head.len()));
        }
    }
    assert!(want.meta || o.meta == Meta::None, "meta read unwanted");
}
