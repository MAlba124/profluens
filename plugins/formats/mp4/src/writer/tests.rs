// Tests own their staging Vecs.
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::reader::Mp4Reader;

const AVCC: [u8; 39] = [
    0x01, 0x42, 0xC0, 0x0A, 0xFF, 0xE1, 0x00, 0x17, 0x67, 0x42, 0xC0, 0x0A, 0xD9, 0x02, 0x0D, 0xB0,
    0x11, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x1E, 0x0F, 0x12, 0x26, 0x48, 0x01,
    0x00, 0x05, 0x68, 0xCB, 0x83, 0xCB, 0x20,
];
const ASC: [u8; 2] = [0x11, 0x90];

/// A picture's stamps in decode order with two B-frames, 15 a second, I0 P3 B1 B2 P6 B4 B5.
fn video_pts() -> Vec<i64> {
    [0, 3, 1, 2, 6, 4, 5, 9, 7, 8].iter().map(|&f| f * 6000).collect()
}

fn heads() -> [TrackHead; 2] {
    [
        TrackHead {
            id: 1,
            handler: Handler::Video,
            entry: Entry::Avc(AVCC.to_vec()),
            timescale: VIDEO_TIMESCALE,
            width: 128,
            height: 96,
            par: (0, 0),
            channels: 0,
            rate: 0,
        },
        TrackHead {
            id: 2,
            handler: Handler::Audio,
            entry: Entry::Aac(ASC.to_vec()),
            timescale: 48_000,
            width: 0,
            height: 0,
            par: (0, 0),
            channels: 2,
            rate: 48_000,
        },
    ]
}

fn payload(track: u8, i: usize, len: usize) -> Vec<u8> {
    (0..len).map(|k| track.wrapping_mul(31).wrapping_add(i as u8).wrapping_add(k as u8)).collect()
}

#[test]
fn progressive_round_trips_through_the_reader() {
    let heads = heads();
    let mut tables = [Table::new(1).unwrap(), Table::new(2).unwrap()];
    let mut file = Vec::new();
    ftyp(&mut file, false);
    let patch = mdat_head(&mut file);
    let vpts = video_pts();
    let apts: Vec<i64> = (0..30)
        .map(|i| ticks(((i * 1024) as f64 / 48.0).round() as i64 * 1_000_000, 48_000))
        .collect();
    let mut want: [Vec<(Vec<u8>, bool)>; 2] = [Vec::new(), Vec::new()];
    let (mut v, mut a) = (0usize, 0usize);
    while v < vpts.len() || a < apts.len() {
        for k in 0..3 {
            if v >= vpts.len() {
                break;
            }
            let bytes = payload(1, v, 40 + v);
            let sync = v % 7 == 0;
            let chunk = (k == 0).then_some(file.len() as u64);
            tables[0]
                .push(1, Sample { size: bytes.len() as u32, pts: vpts[v], hint: 0, sync }, chunk)
                .unwrap();
            file.extend_from_slice(&bytes);
            want[0].push((bytes, sync));
            v += 1;
        }
        for k in 0..10 {
            if a >= apts.len() {
                break;
            }
            let bytes = payload(2, a, 7);
            let chunk = (k == 0).then_some(file.len() as u64);
            tables[1]
                .push(2, Sample { size: 7, pts: apts[a], hint: 1024, sync: true }, chunk)
                .unwrap();
            file.extend_from_slice(&bytes);
            want[1].push((bytes, true));
            a += 1;
        }
    }
    let mdat_len = file.len() as u64 - (patch as u64 - 8);
    file[patch..patch + 8].copy_from_slice(&mdat_len.to_be_bytes());

    let mut dts = [vec![0i64; tables[0].len()], vec![0i64; tables[1].len()]];
    let shift = decode_times(
        Handler::Video,
        VIDEO_TIMESCALE,
        &tables[0].pts,
        &tables[0].hints,
        None,
        None,
        &mut dts[0],
    );
    assert_eq!(shift, 6000, "one frame of reorder");
    decode_times(Handler::Audio, 48_000, &tables[1].pts, &tables[1].hints, None, None, &mut dts[1]);
    assert!(
        dts[1].windows(2).all(|w| w[1] - w[0] == 1024),
        "ms-rounded stamps snap to whole frames"
    );
    let (ve, vd) = progressive_edit(&heads[0], &tables[0], &dts[0], 0, 0);
    let (ae, ad) = progressive_edit(&heads[1], &tables[1], &dts[1], 1024, 0);
    assert_eq!((ve.media_time, ae.media_time), (6000, 1024));
    let tracks = [
        TrackMoov {
            head: &heads[0],
            edit: ve,
            table: Some(&tables[0]),
            dts: &dts[0],
            media_duration: vd,
        },
        TrackMoov {
            head: &heads[1],
            edit: ae,
            table: Some(&tables[1]),
            dts: &dts[1],
            media_duration: ad,
        },
    ];
    moov(&mut file, &tracks, false);

    let mut r = Mp4Reader::new(&file).expect("the written moov resolves");
    assert_eq!(r.tracks().len(), 2);
    assert_eq!(r.tracks()[0].entry.config_record, AVCC);
    assert_eq!((r.tracks()[0].width, r.tracks()[0].height), (128, 96));
    assert_eq!(r.tracks()[1].entry.config_record, ASC);
    assert_eq!((r.tracks()[1].entry.channels, r.tracks()[1].entry.sample_rate), (2, 48_000));
    assert_eq!(r.tracks()[1].media_time, 1024, "the priming is the audio edit");
    r.push_bytes(&file);
    let mut got: [Vec<(Vec<u8>, bool, u64)>; 2] = [Vec::new(), Vec::new()];
    while let Some(s) = r.next_sample() {
        let ns = r.tracks()[s.track_index].stamp_ns(s.pts);
        got[s.track_index].push((s.payload.data().to_vec(), s.sync, ns));
    }
    for t in 0..2 {
        assert_eq!(got[t].len(), want[t].len(), "track {t} sample count");
        for (i, (g, w)) in got[t].iter().zip(&want[t]).enumerate() {
            assert_eq!((&g.0, g.1), (&w.0, w.1), "track {t} sample {i}");
        }
    }
    for (i, g) in got[0].iter().enumerate() {
        assert_eq!(g.2, vpts[i] as u64 * 1_000_000_000 / 90_000, "video pts {i}");
    }
    for (i, g) in got[1].iter().enumerate() {
        assert_eq!(
            g.2,
            (i as u64 * 1024) * 1_000_000_000 / 48_000,
            "audio stamp {i} on the frame grid"
        );
    }
}

#[test]
fn fragments_round_trip_through_the_reader() {
    let heads = heads();
    let mut file = Vec::new();
    ftyp(&mut file, true);
    let tracks = [
        TrackMoov {
            head: &heads[0],
            edit: Edit { media_time: 6000, ..Edit::default() },
            table: None,
            dts: &[],
            media_duration: 0,
        },
        TrackMoov {
            head: &heads[1],
            edit: Edit::default(),
            table: None,
            dts: &[],
            media_duration: 0,
        },
    ];
    moov(&mut file, &tracks, true);
    let init_len = file.len();
    let vpts = video_pts();
    let mut want = Vec::new();
    for (seq, range) in [(1u32, 0..7usize), (2, 7..10)] {
        let pts = &vpts[range.clone()];
        let hints = vec![0u32; pts.len()];
        let mut dts = vec![0i64; pts.len()];
        decode_times(Handler::Video, VIDEO_TIMESCALE, pts, &hints, Some(6000), None, &mut dts);
        let durations: Vec<u32> =
            (0..pts.len()).map(|i| duration_at(&dts, &hints, i, 6000)).collect();
        let cts: Vec<i64> = pts.iter().zip(&dts).map(|(p, d)| p - d).collect();
        let sync: Vec<bool> = (0..pts.len()).map(|i| i == 0).collect();
        let bytes: Vec<Vec<u8>> = range.clone().map(|i| payload(1, i, 30 + i)).collect();
        let sizes: Vec<u32> = bytes.iter().map(|b| b.len() as u32).collect();
        let run = Run {
            track: 1,
            base_dts: (dts[0] + 6000) as u64,
            sizes: &sizes,
            durations: &durations,
            cts: &cts,
            sync: &sync,
        };
        let payload_len = moof(&mut file, seq, &[run]);
        assert_eq!(payload_len, sizes.iter().map(|&s| u64::from(s)).sum::<u64>());
        for (b, s) in bytes.iter().zip(&sync) {
            file.extend_from_slice(b);
            want.push((b.clone(), *s));
        }
    }
    let mut r = Mp4Reader::new(&file[..init_len]).expect("the init segment resolves");
    assert!(r.is_fragmented());
    r.push_bytes(&file);
    let mut got = Vec::new();
    while let Some(s) = r.next_sample() {
        got.push((s.payload.data().to_vec(), s.sync, r.ticks_to_ns(0, s.pts)));
    }
    assert_eq!(got.len(), want.len());
    for (i, g) in got.iter().enumerate() {
        assert_eq!((&g.0, g.1), (&want[i].0, want[i].1), "sample {i}");
        assert_eq!(g.2, vpts[i] as u64 * 1_000_000_000 / 90_000, "pts {i}");
    }
}

#[test]
fn opus_head_becomes_dops() {
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
    assert_eq!(
        dops_from_opus_head(&head).unwrap(),
        vec![0, 2, 0x01, 0x38, 0, 0, 0xBB, 0x80, 0, 0, 0]
    );
    assert_eq!(dops(2, 312, 48_000), vec![0, 2, 0x01, 0x38, 0, 0, 0xBB, 0x80, 0, 0, 0]);
    assert!(dops_from_opus_head(&head[..12]).is_none());
}

#[test]
fn decode_times_clamp_and_snap() {
    let mut dts = [0i64; 4];
    decode_times(Handler::Text, 1000, &[0, 500, 400, 900], &[0; 4], None, Some((-1, 0)), &mut dts);
    assert_eq!(dts, [0, 500, 501, 900]);
    decode_times(
        Handler::Audio,
        48_000,
        &[0, 1024, 9000, 10_025],
        &[1024; 4],
        None,
        None,
        &mut dts,
    );
    assert_eq!(dts, [0, 1024, 9000, 10_024]);
    decode_times(
        Handler::Audio,
        48_000,
        &[2047, 3073],
        &[1024; 2],
        None,
        Some((1024, 1024)),
        &mut dts[..2],
    );
    assert_eq!(dts[..2], [2048, 3072]);
    assert_eq!(ticks(21_333_333, 48_000), 1024);
    assert_eq!(rescale(90_000, 90_000, 1000), 1000);
}

#[test]
fn vp9_entry_reads_back_with_its_vpcc() {
    // Profile 0, level 3.0, 8 bit 4:2:0 collocated, full range, BT.709.
    let vpcc = [0, 30, 0x83, 1, 1, 1, 0, 0];
    let h = TrackHead {
        id: 1,
        handler: Handler::Video,
        entry: Entry::Vp9(vpcc.to_vec()),
        timescale: VIDEO_TIMESCALE,
        width: 640,
        height: 360,
        par: (0, 0),
        channels: 0,
        rate: 0,
    };
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    sample_entry(&mut stsd, &h);
    let e = crate::codec::parse_stsd(&stsd).unwrap();
    assert_eq!(&e.kind, b"vp09");
    assert_eq!((e.family, e.width, e.height), ("vp9", 640, 360));
    assert_eq!(e.par, (0, 0), "square states no pasp");
    let c = e.colour;
    assert_eq!(c.range, Some(profluens_video::color::Range::from_flag(true)));
    assert_eq!(c.matrix, profluens_video::color::Matrix::from_h273(1));
    assert_eq!(c.chroma, Some(profluens_video::color::ChromaSite::TopLeft));
    let at = stsd.windows(4).position(|w| w == b"vpcC").unwrap();
    assert_eq!(&stsd[at + 4..at + 8], &[1, 0, 0, 0], "a version 1 FullBox");
    assert_eq!(&stsd[at + 8..], &vpcc);
}

#[test]
fn an_anamorphic_entry_states_its_pasp_and_shows_stretched() {
    let h = TrackHead { par: (32, 27), ..video_head(1, Entry::Avc(AVCC.to_vec()), 720, 480) };
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    sample_entry(&mut stsd, &h);
    let e = crate::codec::parse_stsd(&stsd).unwrap();
    assert_eq!(((e.width, e.height), e.par), ((720, 480), (32, 27)));
    // the presentation size widens, 720 * 32 / 27 is 853.3
    assert_eq!(shown(&h), (853, 480));
    let narrow = TrackHead { par: (8, 9), ..h.clone() };
    assert_eq!(shown(&narrow), (720, 540));
    let mut init = Vec::new();
    crate::cmaf::init(&mut init, &h);
    let at = init.windows(4).position(|w| w == b"tkhd").unwrap();
    // version 0, the sizes the last 8 bytes of the box, 16.16 fixed point
    let size = u32::from_be_bytes(init[at - 4..at].try_into().unwrap()) as usize;
    let end = at - 4 + size;
    let w = u32::from_be_bytes(init[end - 8..end - 4].try_into().unwrap());
    let hh = u32::from_be_bytes(init[end - 4..end].try_into().unwrap());
    assert_eq!((w >> 16, hh >> 16), (853, 480));
}
