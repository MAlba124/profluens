//! Remux an MP4 into Matroska with every passthrough-supported track, sample bytes untouched.
//! cargo run -p pf-mp4 --example remux_to_mkv -- [--no-verify] input.mp4 [output.mkv]

// App main, its own files.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

use pf_mkv::MatroskaReader;
use pf_mp4::{Mp4Demux, Mp4Reader, Tables, Track};
use profluens_core::id::ElementId;
use profluens_core::memory::SIZED_TOP;
use profluens_core::pipeline::Pipeline;
use profluens_elements::io::{FileSink, FileSrc};

/// Bytes before `mdat`, plus `moov` when it lies behind `mdat` (ISO/IEC 14496-12 8.2.1).
fn read_head(path: &str) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let (mut at, mut mdat, mut moov) = (0u64, None, None);
    while at + 8 <= len && (mdat.is_none() || moov.is_none()) {
        let mut hdr = [0u8; 16];
        let n = (len - at).min(16) as usize;
        f.seek(SeekFrom::Start(at))?;
        f.read_exact(&mut hdr[..n])?;
        let size = match u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) {
            0 => len - at,
            1 if n == 16 => u64::from_be_bytes(hdr[8..16].try_into().expect("8 bytes")),
            1 => return Err(std::io::Error::other("truncated largesize box header")),
            n => u64::from(n),
        };
        if size < 8 || at.saturating_add(size) > len {
            return Err(std::io::Error::other(format!(
                "box at {at} of {size} bytes overruns the file"
            )));
        }
        match &hdr[4..8] {
            b"mdat" if mdat.is_none() => mdat = Some(at),
            b"moov" => moov = Some((at, size)),
            _ => {}
        }
        at += size;
    }
    let (moov_at, moov_len) = moov.ok_or_else(|| std::io::Error::other("no moov box"))?;
    let prefix = mdat.unwrap_or(len);
    let tail = if moov_at >= prefix { moov_len } else { 0 };
    let mut head = vec![0u8; (prefix + tail) as usize];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut head[..prefix as usize])?;
    if tail > 0 {
        f.seek(SeekFrom::Start(moov_at))?;
        f.read_exact(&mut head[prefix as usize..])?;
    }
    Ok(head)
}

fn remuxable(t: &Track) -> Option<&'static str> {
    match &t.entry.kind {
        b"avc1" | b"avc3" if !t.entry.config_record.is_empty() => Some("h264/avcc"),
        b"hvc1" | b"hev1" if !t.entry.config_record.is_empty() => Some("h265/hvcc"),
        b"vp09" => Some("vp9"),
        _ if t.family() == "aac" => Some("aac"),
        _ => None,
    }
}

fn verify_prefix(path: &str, budget: usize) -> std::io::Result<()> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; budget];
    let n = f.read(&mut buf)?;
    buf.truncate(n);

    let mut r = MatroskaReader::new();
    r.push(&buf).map_err(|e| std::io::Error::other(format!("output does not parse: {e:?}")))?;
    let (video_no, codec, private_len, length_size) = {
        let t = r
            .tracks()
            .iter()
            .find(|t| t.codec_id.starts_with("V_"))
            .ok_or_else(|| std::io::Error::other("no video track in output"))?;
        (
            t.track_number,
            t.codec_id.clone(),
            t.codec_private.len(),
            (t.codec_private.get(4).copied().unwrap_or(3) & 0x3) as usize + 1,
        )
    };
    let ntracks = r.tracks().len();

    let (mut frames, mut keyframes, mut audio_blocks) = (0u64, 0u64, 0u64);
    while let Some(fr) = r.next_frame() {
        if fr.track_number != video_no {
            audio_blocks += 1;
            continue;
        }
        let d = &fr.data;
        let mut off = 0usize;
        while off < d.len() {
            if off + length_size > d.len() {
                return Err(std::io::Error::other(format!(
                    "frame {frames}: truncated NAL length at {off} (split sample? \
                     raise the demuxer pool slot)"
                )));
            }
            let mut l = 0usize;
            for b in &d[off..off + length_size] {
                l = (l << 8) | *b as usize;
            }
            off += length_size + l;
        }
        if off != d.len() {
            return Err(std::io::Error::other(format!(
                "frame {frames}: NAL walk overran the block"
            )));
        }
        frames += 1;
        keyframes += fr.keyframe as u64;
    }
    println!(
        "verify: {ntracks} tracks, {codec} CodecPrivate {private_len}B, first {frames} \
         video blocks NAL-walk clean ({keyframes} keyframes), {audio_blocks} other-track \
         blocks interleaved"
    );
    Ok(())
}

fn size_demux_pool(p: &mut Pipeline, demux: ElementId, tables: &Tables, tracks: &[usize]) {
    let pool = tables.remux_pool(tracks);
    let largest = pool.largest;
    assert!(
        largest <= SIZED_TOP as u64,
        "a sample of {largest} bytes is past what a demuxer buffer holds"
    );
    let mib = pool.bytes as f64 / (1 << 20) as f64;
    println!("demux pool: {mib:.1} MiB, {} of the {largest} B sample reserved", pool.held);
    p.set_element_pool_bytes(demux, 64 << 10, 512, pool.bytes);
    p.set_element_reserve(demux, largest as usize, pool.held);
}

fn main() {
    let verify = !std::env::args().any(|a| a == "--no-verify");
    let mut args = std::env::args().skip(1).filter(|a| a != "--no-verify");
    let Some(input) = args.next() else {
        eprintln!("usage: remux_to_mkv [--no-verify] <input.mp4> [output.mkv]");
        std::process::exit(2);
    };
    let output = args.next().unwrap_or_else(|| format!("{}.mkv", input.trim_end_matches(".mp4")));

    let head = read_head(&input).expect("read MP4 head (ftyp+moov)");
    println!("head: {} bytes (ftyp through moov)", head.len());

    let probe = Mp4Reader::new(&head).expect("resolve MP4 tables");
    let picked: Vec<(String, &'static str)> = probe
        .tracks()
        .iter()
        .filter_map(|t| {
            remuxable(t).map(|fam| {
                println!(
                    "track {}: {} → {fam}{}",
                    t.track_id,
                    String::from_utf8_lossy(&t.entry.kind),
                    if t.width != 0 {
                        format!(" ({}x{})", t.width, t.height)
                    } else {
                        String::new()
                    },
                );
                (format!("src_track{}", t.track_id), fam)
            })
        })
        .collect();
    assert!(!picked.is_empty(), "no remuxable track");
    println!("{} of {} tracks remuxed", picked.len(), probe.tracks().len());

    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&input));
    let demux = p.add(Mp4Demux::passthrough(head));
    p.link(src.src(FileSrc::OUT), demux.sink(Mp4Demux::IN)).expect("src -> demux");
    let added = p.preroll().expect("preroll").srcs;
    let mux = p.add(pf_mkv::MkvMux::multi(picked.len()));
    let sink = p.add(FileSink::new(&output));
    for (i, (pad_name, _)) in picked.iter().enumerate() {
        let ap =
            added.iter().find(|ap| &ap.name == pad_name).expect("picked pad discovered at preroll");
        p.link(ap.element.src(ap.pad), mux.sink(pf_mkv::MkvMuxN::INS[i])).expect("demux -> mux");
    }
    p.link(mux.src(pf_mkv::MkvMuxN::OUT), sink.sink(FileSink::IN)).expect("mux -> sink");
    let tracks: Vec<usize> =
        (0..probe.tracks().len()).filter(|&i| remuxable(&probe.tracks()[i]).is_some()).collect();
    size_demux_pool(&mut p, demux, &probe.tables(), &tracks);

    let tap = p.tap_handle();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let progress = {
        let done = std::sync::Arc::clone(&done);
        std::thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                // `park_timeout`, not `sleep`, so a finished remux is not held open for a whole
                // slice.
                std::thread::park_timeout(std::time::Duration::from_millis(500));
                if let Some(s) = tap.snapshot(mux) {
                    eprint!("\rmuxed {:>8.1} MiB…", s.bytes_out as f64 / (1024.0 * 1024.0));
                }
            }
            eprintln!();
        })
    };

    let started = Instant::now();
    p.run().expect("remux run");
    // Sampled before joining the observer so the rate excludes shutdown.
    let secs = started.elapsed().as_secs_f64();
    done.store(true, std::sync::atomic::Ordering::Release);
    progress.thread().unpark(); // `done` is published before this, so the wake cannot be lost
    let _ = progress.join();

    let out_len = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    println!(
        "wrote {output}: {:.1} MiB in {secs:.1}s ({:.0} MiB/s)",
        out_len as f64 / (1024.0 * 1024.0),
        out_len as f64 / (1024.0 * 1024.0) / secs.max(0.001),
    );
    if verify {
        verify_prefix(&output, 16 << 20).expect("verify remuxed output");
    }
}
