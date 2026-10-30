//! Remux between Matroska and MP4 with no decode.
//! cargo run -p pf-mp4 --example remux -- [--fragmented] input.(mkv|webm|mp4) output.(mp4|mkv)

// App main, files read before any pipeline exists.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::Path;

use pf_mkv::{MatroskaReader, MkvDemux, MkvMux, MkvMuxN};
use pf_mp4::{Mode, Mp4Demux, Mp4Mux, Mp4Reader};
use profluens_core::id::SinkPad;
use profluens_elements::io::{FileSink, FileSrc};

fn mkv_head(file: &[u8]) -> Vec<u8> {
    let at = file.windows(4).position(|w| w == [0x1F, 0x43, 0xB6, 0x75]).unwrap_or(file.len());
    file[..at].to_vec()
}

fn mp4_head(file: &[u8]) -> Vec<u8> {
    let (mut at, mut mdat, mut moov) = (0usize, None, None);
    while at + 8 <= file.len() {
        let size = match u32::from_be_bytes(file[at..at + 4].try_into().expect("4 bytes")) {
            0 => file.len() - at,
            1 if at + 16 <= file.len() => {
                u64::from_be_bytes(file[at + 8..at + 16].try_into().expect("8 bytes")) as usize
            }
            n => n as usize,
        };
        match &file[at + 4..at + 8] {
            b"mdat" if mdat.is_none() => mdat = Some(at),
            b"moov" => moov = Some((at, size)),
            _ => {}
        }
        if size < 8 {
            break;
        }
        at = at.saturating_add(size);
    }
    let (m, len) = moov.expect("an MP4 with a moov");
    let prefix = mdat.unwrap_or(file.len()).min(m + len);
    let mut head = file[..prefix].to_vec();
    if m >= prefix {
        head.extend_from_slice(&file[m..(m + len).min(file.len())]);
    }
    head
}

fn main() {
    let fragmented = std::env::args().any(|a| a == "--fragmented");
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--fragmented").collect();
    let [input, output] = args.as_slice() else {
        eprintln!("usage: remux [--fragmented] <input.mkv|input.mp4> <output.mp4|output.mkv>");
        std::process::exit(2);
    };
    let file = std::fs::read(input).expect("read the input");
    let to_mkv = Path::new(output).extension().is_some_and(|e| e == "mkv" || e == "webm");
    let mode = if fragmented { Mode::Fragmented } else { Mode::Progressive };

    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(input));
    let (names, demux) = if file.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        let head = mkv_head(&file);
        let mut r = MatroskaReader::new();
        r.push(&head).expect("the Matroska header parses");
        let wanted = |id: &str| {
            id.starts_with("V_") || id.starts_with("A_") || (!to_mkv && id == "S_TEXT/UTF8")
        };
        let names: Vec<String> = r
            .tracks()
            .iter()
            .filter(|t| wanted(&t.codec_id))
            .map(|t| format!("src_track{}", t.track_number))
            .collect();
        let demux = p.add(MkvDemux::new(head));
        p.link(src.src(FileSrc::OUT), demux.sink(MkvDemux::IN)).expect("src -> demux");
        (names, demux)
    } else {
        let head = mp4_head(&file);
        let tables = Mp4Reader::new(&head).expect("the MP4 tables resolve").tables();
        let names: Vec<String> = tables
            .tracks()
            .iter()
            .filter(|t| t.family() != "bytes" && (!to_mkv || !t.is_text()))
            .map(|t| format!("src_track{}", t.track_id))
            .collect();
        let demux = p.add(Mp4Demux::new(head));
        p.link(src.src(FileSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src -> demux");
        let pool = tables.remux_pool(&(0..tables.tracks().len()).collect::<Vec<_>>());
        p.set_element_pool_bytes(demux, 64 << 10, 512, pool.bytes);
        p.set_element_reserve(demux, pool.largest as usize, pool.held);
        (names, demux)
    };
    let added = p.preroll().expect("preroll").srcs;
    let names: Vec<&String> = names
        .iter()
        .filter(|n| added.iter().any(|a| a.element == demux && &a.name == *n))
        .collect();
    let (mux, ins, out): (_, [SinkPad; 8], _) = match to_mkv {
        true => {
            let m = p.add(MkvMux::multi(names.len()));
            (m, MkvMuxN::INS, m.src(MkvMuxN::OUT))
        }
        false => {
            let m = p.add(Mp4Mux::tracks(mode, names.len()));
            (m, Mp4Mux::INS, m.src(Mp4Mux::OUT))
        }
    };
    for (i, name) in names.iter().enumerate() {
        let ap = added.iter().find(|a| &a.name == *name).expect("a pad per track");
        println!("{name} -> {}", if to_mkv { "mkvmuxn" } else { "mp4mux" });
        p.link(ap.element.src(ap.pad), mux.sink(ins[i])).expect("demux -> mux");
    }
    let sink = p.add(FileSink::new(output));
    p.link(out, sink.sink(FileSink::IN)).expect("mux -> sink");
    let started = std::time::Instant::now();
    p.run().expect("the remux runs");
    let len = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    println!("wrote {output}, {len} bytes in {:.2} s", started.elapsed().as_secs_f64());
}
