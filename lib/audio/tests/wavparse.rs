// Tests own their fixtures and staging `Vec`s.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_audio::{AudioFormat, SampleFormat, WavParse, parse_wav_header, write_pcm_wav};
use profluens_core::time::Timestamp;
use profluens_elements::io::{FileSink, FileSrc};

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("pf_wav_{}_{}.bin", tag, std::process::id()));
    p
}

#[test]
fn wavparse_strips_header_and_streams_pcm() {
    let fmt = AudioFormat::new(44_100, 2, SampleFormat::S16);
    let frames = 90_000usize; // stereo s16, 360000 payload bytes
    let mut pcm = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let l = (i as i16).wrapping_mul(31);
        let r = (i as i16).wrapping_mul(-17).wrapping_add(5);
        pcm.extend_from_slice(&l.to_le_bytes());
        pcm.extend_from_slice(&r.to_le_bytes());
    }
    let wav = write_pcm_wav(&fmt, &pcm);
    assert_eq!(parse_wav_header(&wav).unwrap().format, fmt);

    let inp = temp_path("in");
    let outp = temp_path("out");
    std::fs::write(&inp, &wav).unwrap();

    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&inp));
    let wavp = p.add(WavParse::new());
    let sink = p.add(FileSink::new(&outp));
    p.link(src.src(FileSrc::OUT), wavp.sink(WavParse::IN)).expect("link src->wavparse");
    p.link(wavp.src(WavParse::OUT), sink.sink(FileSink::IN)).expect("link wavparse->sink");
    p.run().expect("run");

    let got = std::fs::read(&outp).unwrap();
    assert_eq!(got.len(), pcm.len(), "payload length preserved (header stripped)");
    assert_eq!(got, pcm, "PCM streamed byte-identically");

    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
}

#[test]
fn flush_rewinds_the_payload_cursor_so_repeated_seeks_do_not_truncate() {
    let fmt = AudioFormat::new(44_100, 2, SampleFormat::S16);
    let pcm: Vec<u8> = (0..4_000u16).flat_map(|i| i.to_le_bytes()).collect();
    let wav = write_pcm_wav(&fmt, &pcm);

    let mut h = Harness::with_slot_size(WavParse::new(), 1 << 16);
    h.start().expect("start");

    let drain = |h: &mut Harness| {
        let mut got = Vec::new();
        while let Some(b) = h.pull(WavParse::OUT) {
            got.extend_from_slice(b.memory.data());
        }
        got
    };

    let buf = h.alloc(&wav);
    h.push(buf).expect("push");
    assert_eq!(drain(&mut h), pcm, "first pass: header stripped, payload streamed");

    for pass in 0..10 {
        h.seek(44, Timestamp::ZERO).expect("seek");
        let buf = h.alloc(&pcm);
        h.push(buf).expect("push");
        let got = drain(&mut h);
        assert_eq!(
            got.len(),
            pcm.len(),
            "pass {pass}: the data_len clamp truncated the payload ({} of {} bytes)",
            got.len(),
            pcm.len()
        );
        assert_eq!(got, pcm, "pass {pass}: payload changed");
    }
    h.seek_to(Timestamp::ZERO, None).expect("seek");
    let buf = h.alloc(&wav);
    h.push(buf).expect("push");
    assert_eq!(drain(&mut h), pcm, "the whole file again, header stripped");
}

#[test]
fn a_pool_of_a_few_slots_holds_the_parser_back_and_loses_nothing() {
    let (wav, pcm) = counted_wav(90_000);
    for slots in [1u32, 2, 4, 8, 12, 16, 24, 64] {
        let inp = temp_path(&format!("small_in_{slots}"));
        let outp = temp_path(&format!("small_out_{slots}"));
        std::fs::write(&inp, &wav).unwrap();
        let mut p = pf_reactor::pipeline();
        p.set_pool(4096, slots);
        let src = p.add(FileSrc::new(&inp));
        let wavp = p.add(WavParse::new());
        let sink = p.add(FileSink::new(&outp));
        p.link(src.src(FileSrc::OUT), wavp.sink(WavParse::IN)).expect("link src->wavparse");
        p.link(wavp.src(WavParse::OUT), sink.sink(FileSink::IN)).expect("link wavparse->sink");
        let (done, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || done.send(p.run().map(|_| ()).map_err(|e| format!("{e:?}"))));
        let ran = ended.recv_timeout(std::time::Duration::from_secs(20));
        assert_eq!(ran, Ok(Ok(())), "a pool of {slots} slots");
        assert!(std::fs::read(&outp).unwrap() == pcm, "a pool of {slots} slots");
        let _ = std::fs::remove_file(&inp);
        let _ = std::fs::remove_file(&outp);
    }
}

#[test]
fn three_elements_that_wait_for_a_slot_run_through_pools_of_one() {
    use profluens_audio::{AudioConvert, AudioGain};
    let (wav, pcm) = counted_wav(90_000);
    for slots in [1u32, 2, 3, 16] {
        let inp = temp_path(&format!("waiters_in_{slots}"));
        let outp = temp_path(&format!("waiters_out_{slots}"));
        std::fs::write(&inp, &wav).unwrap();
        let mut p = pf_reactor::pipeline();
        p.set_pool(4096, slots);
        let src = p.add(FileSrc::new(&inp));
        let wavp = p.add(WavParse::new());
        let conv = p.add(AudioConvert::with_input(
            AudioFormat::new(48_000, 2, SampleFormat::S16),
            SampleFormat::F32,
        ));
        let gain = p.add(AudioGain::new(0.5));
        let sink = p.add(FileSink::new(&outp));
        p.link(src.src(FileSrc::OUT), wavp.sink(WavParse::IN)).expect("link src->wavparse");
        p.link(wavp.src(WavParse::OUT), conv.sink(AudioConvert::IN))
            .expect("link wavparse->convert");
        p.link(conv.src(AudioConvert::OUT), gain.sink(AudioGain::IN)).expect("link convert->gain");
        p.link(gain.src(AudioGain::OUT), sink.sink(FileSink::IN)).expect("link gain->sink");
        let (done, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || done.send(p.run().map(|_| ()).map_err(|e| format!("{e:?}"))));
        let ran = ended.recv_timeout(std::time::Duration::from_secs(30));
        assert_eq!(ran, Ok(Ok(())), "pools of {slots} slots");
        let out = std::fs::read(&outp).unwrap();
        assert_eq!(out.len(), pcm.len() * 2, "every frame, as f32, with pools of {slots} slots");
        for (k, (got, was)) in
            out.as_chunks::<4>().0.iter().zip(pcm.as_chunks::<2>().0).enumerate().step_by(997)
        {
            let (got, was) =
                (f32::from_le_bytes(*got), f32::from(i16::from_le_bytes(*was)) / 32_768.0);
            assert!((got - was * 0.5).abs() < 1e-4, "sample {k} is {got}, half of {was}");
        }
        let _ = std::fs::remove_file(&inp);
        let _ = std::fs::remove_file(&outp);
    }
}

fn counted_wav(frames: usize) -> (Vec<u8>, Vec<u8>) {
    let fmt = AudioFormat::new(48_000, 2, SampleFormat::S16);
    let pcm: Vec<u8> = (0..frames)
        .flat_map(|i| [(i as u16).to_le_bytes(), ((i >> 16) as u16).to_le_bytes()].concat())
        .collect();
    (write_pcm_wav(&fmt, &pcm), pcm)
}

fn at_frame(n: u64) -> Timestamp {
    profluens_audio::time_at(n, 48_000)
}

#[test]
fn a_read_goes_out_a_few_buffers_a_pass_each_stamped_at_its_first_frame() {
    const CUT: usize = 4096;
    let frames = 10 * CUT + 100;
    let (wav, pcm) = counted_wav(frames);
    let mut h = Harness::with_slot_size(WavParse::new(), 1 << 20);
    h.start().expect("start");
    let buf = h.alloc(&wav);
    h.push(buf).expect("push");

    let (mut got, mut passes, mut next) = (Vec::new(), Vec::new(), 0u64);
    loop {
        let out = h.drain_outputs();
        if out.is_empty() {
            break;
        }
        passes.push(out.len());
        for b in out {
            let frames = b.memory.len() as u64 / 4;
            assert_eq!(b.pts, at_frame(next), "the buffer at frame {next}");
            assert_eq!(b.memory.len() % 4, 0, "whole frames");
            assert!(b.memory.len() <= CUT * 4, "{} bytes in one buffer", b.memory.len());
            assert_eq!(b.duration, at_frame(next + frames).saturating_sub(b.pts));
            next += frames;
            got.extend_from_slice(b.memory.data());
        }
        h.crank().expect("crank");
    }
    assert_eq!(passes, [4, 4, 3], "eleven buffers, four a pass");
    assert_eq!(got, pcm, "every frame, in order");
}

#[test]
fn with_no_slot_it_keeps_its_input_and_goes_on_when_one_comes_back() {
    const SLOT: usize = 1024;
    let (wav, pcm) = counted_wav(8 * SLOT / 4);
    let mut h = Harness::with_pool(WavParse::new(), SLOT, 2);
    h.start().expect("start");
    let buf = h.alloc(&wav);
    h.push(buf).expect("push");

    let held = h.drain_outputs();
    assert_eq!(held.len(), 2, "the two slots the pool has");
    h.crank().expect("crank");
    assert!(h.drain_outputs().is_empty(), "nothing more while both are out");
    let mut got: Vec<u8> = held.iter().flat_map(|b| b.memory.data().to_vec()).collect();
    assert_eq!(held[1].pts, at_frame(SLOT as u64 / 4));
    drop(held);

    let tail = h.eos().expect("eos");
    assert_eq!(tail[0].pts, at_frame(2 * SLOT as u64 / 4), "on from the frame it stopped at");
    got.extend(tail.iter().flat_map(|b| b.memory.data().to_vec()));
    assert_eq!(got, pcm, "every frame, in order");
}

#[test]
fn frames_go_out_whole_across_reads_and_after_a_seek_into_one() {
    let (wav, pcm) = counted_wav(50);
    let mut h = Harness::with_slot_size(WavParse::new(), 1 << 16);
    h.start().expect("start");
    let drain = |h: &mut Harness| -> Vec<(Timestamp, Vec<u8>)> {
        let mut out = Vec::new();
        loop {
            let got = h.drain_outputs();
            if got.is_empty() {
                return out;
            }
            out.extend(got.iter().map(|b| (b.pts, b.memory.data().to_vec())));
            h.crank().expect("crank");
        }
    };
    let mut got = Vec::new();
    for piece in wav.chunks(7) {
        let buf = h.alloc(piece);
        h.push(buf).expect("push");
        for (pts, bytes) in drain(&mut h) {
            assert_eq!(pts, at_frame(got.len() as u64 / 4), "stamped at its first frame");
            assert_eq!(bytes.len() % 4, 0, "whole frames");
            got.extend(bytes);
        }
    }
    assert_eq!(got, pcm);

    h.seek(44 + 42, at_frame(10)).expect("seek");
    let buf = h.alloc(&wav[44 + 42..wav.len() - 1]);
    h.push(buf).expect("push");
    let out = drain(&mut h);
    assert_eq!(out[0].0, at_frame(11), "the next whole frame");
    let bytes: Vec<u8> = out.into_iter().flat_map(|(_, b)| b).collect();
    assert_eq!(bytes, pcm[44..pcm.len() - 4], "frames 11 to 48");
}

#[test]
fn wavparse_publishes_the_line_of_its_header() {
    use profluens_audio::wav_index;
    let fmt = AudioFormat::new(48_000, 2, SampleFormat::S16);
    let wav = write_pcm_wav(&fmt, &vec![0u8; 48_000 * 4]);
    let inp = temp_path("index");
    std::fs::write(&inp, &wav).unwrap();
    let len = wav.len() as u64;

    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&inp));
    let wavp = p.add(WavParse::with_len(Some(len)));
    let sink = p.add(FileSink::new(temp_path("index_out")));
    p.link(src.src(FileSrc::OUT), wavp.sink(WavParse::IN)).expect("link src->wavparse");
    p.link(wavp.src(WavParse::OUT), sink.sink(FileSink::IN)).expect("link wavparse->sink");
    p.run().expect("run");

    let index = p.index(wavp).expect("published");
    assert_eq!(*index, wav_index(&parse_wav_header(&wav).unwrap(), Some(len)));
    assert_eq!(index.duration, Timestamp::from_secs(1));
    assert_eq!(index.entries.len(), 10);
    assert_eq!((index.entries[9].time_ns, index.entries[9].byte), (900_000_000, 44 + 43_200 * 4));
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(temp_path("index_out"));
}
