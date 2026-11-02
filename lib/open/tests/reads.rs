#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

mod common;

use common::*;
use pf_bare::BareKind;
use pf_mkv::CuesAt;
use pf_mkv::walk::MetaAt;
use pf_open::{Caps, Want};
use pf_probe::head::{AviHeadAt, MkvHeadAt, Mp4HeadAt};
use profluens_core::fetch::{Need, Window};

/// The reads fhs makes today to open `b` for the tunnel, by the machines its openers run.
fn today(b: &[u8], kind: &str) -> Vec<Need> {
    let mut log = Vec::new();
    let len = b.len() as u64;
    match kind {
        "mkv" => {
            let h = reads(&mut MkvHeadAt::new(64 << 20), b, &mut log).unwrap();
            reads(&mut CuesAt::new(&h.head, 32 << 20), b, &mut log);
            reads(&mut MetaAt::new(&h.head, 4 << 20, 16 << 20), b, &mut log);
        }
        "mp4" => {
            let w = reads(&mut Window::new(0, (256 << 10).min(b.len())), b, &mut log).unwrap();
            reads(&mut Mp4HeadAt::new(&w, len), b, &mut log).unwrap();
        }
        "avi" => {
            reads(&mut AviHeadAt::new(16 << 20), b, &mut log).unwrap();
        }
        "ogg" => {
            reads(&mut Window::new(0, (64 << 10).min(b.len())), b, &mut log);
            let at = len.saturating_sub(pf_ogg::OGG_TAIL_LEN as u64);
            reads(&mut Window::new(at, (len - at) as usize), b, &mut log);
        }
        "flac" | "mp3" | "aac" => {
            let k = match kind {
                "flac" => BareKind::Flac,
                "mp3" => BareKind::Mp3,
                _ => BareKind::Adts,
            };
            reads(&mut pf_bare::OpenAt::new(len, k), b, &mut log).unwrap();
        }
        _ => {
            // the HLS source, the sniff prefix then the demuxer's head
            log.push(Need { at: 0, len: pf_probe::PREFIX_LEN });
            log.push(Need { at: 0, len: b.len().min(pf_flv::HEAD) });
        }
    }
    log
}

/// Every container opened by `OpenAt` in no more reads, nor bytes past a window, than today.
#[test]
fn no_open_reads_more_than_today() {
    let big = mkv_with(MkvParts { secs: 40, tags: true, pad: 2000, front: false }).bytes;
    let mut moov_last = fixture("plugins/formats/mp4/tests/fixtures/moov_last.mp4")[..40].to_vec();
    let small = fixture("plugins/formats/mp4/tests/fixtures/moov_last.mp4");
    let mdat = 82510 + (1 << 20);
    moov_last.extend_from_slice(&(mdat as u32).to_be_bytes());
    moov_last.extend_from_slice(b"mdat");
    moov_last.resize(40 + mdat, 0);
    moov_last.extend_from_slice(&small[82550..]);
    let cases: Vec<(&str, &str, Vec<u8>, Want)> = vec![
        ("mkv 2 MB, Cues and Tags at the end", "mkv", big, Want::ALL),
        ("mkv tagged release in the window", "mkv", own("tagged.mkv"), Want::ALL),
        ("webm", "mkv", own("tone.webm"), Want::ALL),
        (
            "mp4 faststart",
            "mp4",
            fixture("plugins/formats/mp4/tests/fixtures/tiny_av.mp4"),
            Want::ALL,
        ),
        ("mp4 moov behind 1 MB", "mp4", moov_last, Want::ALL),
        ("m4a", "mp4", own("tone.m4a"), Want::ALL),
        (
            "avi, idx1 left to the demuxer",
            "avi",
            fixture("plugins/formats/avi/tests/fixtures/tiny.avi"),
            Want { index: false, meta: true },
        ),
        ("ogg 700 KB", "ogg", opus_file(7, 6_000, 0, 0), Want::ALL),
        ("flac", "flac", fixture("plugins/formats/bare/tests/fixtures/tone.flac"), Want::ALL),
        ("mp3", "mp3", fixture("plugins/formats/bare/tests/fixtures/tone.mp3"), Want::ALL),
        ("adts", "aac", fixture("plugins/formats/bare/tests/fixtures/tone.aac"), Want::ALL),
        ("flv", "flv", fixture("plugins/formats/flv/tests/fixtures/h264_aac.flv"), Want::ALL),
    ];
    for (name, kind, b, want) in cases {
        let (o, log) = open_with(&b, want, Caps::default());
        o.unwrap();
        let was = today(&b, kind);
        eprintln!("{name}: {} reads, today {}", log.len(), was.len());
        assert!(log.len() <= was.len(), "{name} {log:?} against {was:?}");
    }
}
