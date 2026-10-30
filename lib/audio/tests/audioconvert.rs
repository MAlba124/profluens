// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use profluens_audio::{AudioConvert, AudioFormat, SampleFormat, convert_interleaved_vec};
use profluens_elements::io::{FileSink, FileSrc};

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("pf_audioconvert_{}_{}.bin", tag, std::process::id()));
    p
}

fn run_convert(input_pcm: &[u8], in_fmt: AudioFormat, target: SampleFormat, tag: &str) -> Vec<u8> {
    let inp = temp_path(&format!("{tag}_in"));
    let outp = temp_path(&format!("{tag}_out"));
    std::fs::write(&inp, input_pcm).unwrap();

    let mut p = pf_reactor::pipeline();
    let src = p.add(FileSrc::new(&inp));
    let conv = p.add(AudioConvert::with_input(in_fmt, target));
    let sink = p.add(FileSink::new(&outp));
    p.link(src.src(FileSrc::OUT), conv.sink(AudioConvert::IN)).expect("link src->audioconvert");
    p.link(conv.src(AudioConvert::OUT), sink.sink(FileSink::IN)).expect("link audioconvert->sink");
    p.run().expect("run");

    let got = std::fs::read(&outp).unwrap();
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
    got
}

#[test]
fn s16_stereo_to_s32_matches_library_over_a_large_stream() {
    let in_fmt = AudioFormat::new(48_000, 2, SampleFormat::S16);
    let frames = 80_000usize; // stereo s16 -> 320_000 payload bytes
    let mut pcm = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let l = (i as i16).wrapping_mul(37).wrapping_add(11);
        let r = (i as i16).wrapping_mul(-53).wrapping_sub(7);
        pcm.extend_from_slice(&l.to_le_bytes());
        pcm.extend_from_slice(&r.to_le_bytes());
    }

    let got = run_convert(&pcm, in_fmt, SampleFormat::S32, "s16_s32");

    let want = convert_interleaved_vec(SampleFormat::S16, SampleFormat::S32, 2, &pcm).unwrap();
    assert_eq!(got.len(), want.len(), "converted length (s16->s32, 4x wider payload)");
    assert_eq!(got, want, "element output matches the conversion library byte-for-byte");
}

#[test]
fn s16_to_s32_exact_known_bytes() {
    let in_fmt = AudioFormat::new(44_100, 1, SampleFormat::S16);
    let samples: [i16; 4] = [0, 1, -1, i16::MAX];
    let mut pcm = Vec::new();
    for s in samples {
        pcm.extend_from_slice(&s.to_le_bytes());
    }

    let got = run_convert(&pcm, in_fmt, SampleFormat::S32, "known");

    let mut want = Vec::new();
    for s in samples {
        want.extend_from_slice(&((s as i32) << 16).to_le_bytes());
    }
    assert_eq!(got, want, "s16->s32 produces exactly (s << 16) little-endian per sample");
}

#[test]
fn f32_to_s16_full_scale_and_clamp() {
    let in_fmt = AudioFormat::new(48_000, 1, SampleFormat::F32);
    let samples: [f32; 5] = [0.0, 1.0, -1.0, 2.0, -9.0];
    let mut pcm = Vec::new();
    for s in samples {
        pcm.extend_from_slice(&s.to_le_bytes());
    }

    let got = run_convert(&pcm, in_fmt, SampleFormat::S16, "f32_s16");

    let want: Vec<u8> =
        [0i16, 32767, -32767, 32767, -32767].iter().flat_map(|s| s.to_le_bytes()).collect();
    assert_eq!(got, want, "f32->s16 scales by (2^15 - 1) and clamps at full scale");
}

#[test]
fn u8_to_s16_carries_the_128_bias() {
    let in_fmt = AudioFormat::new(8_000, 2, SampleFormat::U8);
    let bytes: [u8; 8] = [128, 0, 255, 129, 127, 200, 56, 128];
    let got = run_convert(&bytes, in_fmt, SampleFormat::S16, "u8_s16");

    let want: Vec<u8> =
        bytes.iter().flat_map(|&u| ((((u as i32) - 128) << 8) as i16).to_le_bytes()).collect();
    assert_eq!(got, want, "u8->s16 applies the 128 bias then widens by << 8");
}

#[test]
fn passthrough_when_target_equals_input() {
    let in_fmt = AudioFormat::new(44_100, 2, SampleFormat::S24);
    let frames = 5_000usize; // stereo s24 -> 30_000 bytes
    let mut pcm = Vec::with_capacity(frames * 6);
    for i in 0..frames {
        let l = (i as i32 * 7) & 0x00FF_FFFF;
        let r = (i as i32 * -3) & 0x00FF_FFFF;
        pcm.extend_from_slice(&l.to_le_bytes()[..3]);
        pcm.extend_from_slice(&r.to_le_bytes()[..3]);
    }

    let got = run_convert(&pcm, in_fmt, SampleFormat::S24, "passthrough");
    assert_eq!(got, pcm, "identical sample format passes the PCM through unchanged");
}

#[test]
fn s32_to_s16_narrows_with_rounding_matches_library() {
    let in_fmt = AudioFormat::new(48_000, 2, SampleFormat::S32);
    let frames = 20_000usize;
    let mut pcm = Vec::with_capacity(frames * 8);
    for i in 0..frames {
        let l = (i as i32).wrapping_mul(0x0001_3579).wrapping_add(0x0000_8000);
        let r = i32::MAX.wrapping_sub(i as i32);
        pcm.extend_from_slice(&l.to_le_bytes());
        pcm.extend_from_slice(&r.to_le_bytes());
    }

    let got = run_convert(&pcm, in_fmt, SampleFormat::S16, "s32_s16");
    let want = convert_interleaved_vec(SampleFormat::S32, SampleFormat::S16, 2, &pcm).unwrap();
    assert_eq!(got.len(), want.len(), "narrowed payload is half the width");
    assert_eq!(got, want, "s32->s16 narrowing (round+clamp) matches the library");
}

mod raw_src {
    use profluens_audio::{FAMILY, FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE};
    use profluens_core::batch::Inputs;
    use profluens_core::ctx::Ctx;
    use profluens_core::element::{
        Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
    };
    use profluens_core::error::Error;
    use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
    use profluens_core::id::SrcPad;

    pub const RATE: i64 = 44_100;
    pub const CHANNELS: i64 = 2;
    pub const SAMPLE: &str = "s16"; // == SampleFormat::S16.caps_name()

    static FIELDS: [FieldDesc; 3] = [
        FieldDesc {
            field: FIELD_RATE,
            allowed: ConstraintDesc::Eq(ValueDesc::Int(RATE)),
            preferred: None,
        },
        FieldDesc {
            field: FIELD_CHANNELS,
            allowed: ConstraintDesc::Eq(ValueDesc::Int(CHANNELS)),
            preferred: None,
        },
        FieldDesc {
            field: FIELD_SAMPLE,
            allowed: ConstraintDesc::Eq(ValueDesc::Id(SAMPLE)),
            preferred: None,
        },
    ];
    static OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &FIELDS }];
    static PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
    static DESC: ElementDesc = ElementDesc {
        name: "rawaudiosrc",
        sinks: &[],
        srcs: &PADS,
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::None,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    };

    pub struct RawAudioSrc {
        pcm: Vec<u8>,
        off: usize,
    }

    impl RawAudioSrc {
        pub const OUT: SrcPad = SrcPad(0);

        pub fn new(pcm: Vec<u8>) -> Self {
            Self { pcm, off: 0 }
        }
    }

    impl Element for RawAudioSrc {
        fn desc(&self) -> &'static ElementDesc {
            &DESC
        }
        fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
            self.off = 0;
            Ok(())
        }
        fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
            if self.off >= self.pcm.len() {
                return Ok(Flow::Eos);
            }
            let mut buf = match ctx.try_alloc() {
                Some(b) => b,
                None => return Ok(Flow::Ok),
            };
            let cap = buf.memory.capacity();
            let n = cap.min(self.pcm.len() - self.off);
            buf.memory.as_mut_full()[..n].copy_from_slice(&self.pcm[self.off..self.off + n]);
            buf.memory.set_len(n);
            self.off += n;
            ctx.out(Self::OUT).push(buf);
            Ok(Flow::Ok)
        }
    }
}

#[test]
fn new_infers_input_format_from_negotiated_caps() {
    let in_fmt =
        AudioFormat::new(raw_src::RATE as u32, raw_src::CHANNELS as u16, SampleFormat::S16);
    let frames = 40_000usize;
    let mut pcm = Vec::with_capacity(frames * in_fmt.frame_stride());
    for i in 0..frames {
        let l = (i as i16).wrapping_mul(41).wrapping_add(3);
        let r = (i as i16).wrapping_mul(-29).wrapping_sub(9);
        pcm.extend_from_slice(&l.to_le_bytes());
        pcm.extend_from_slice(&r.to_le_bytes());
    }

    let outp = temp_path("infer_out");

    let mut p = pf_reactor::pipeline();
    let src = p.add(raw_src::RawAudioSrc::new(pcm.clone()));
    let conv = p.add(AudioConvert::new(SampleFormat::S32));
    let sink = p.add(FileSink::new(&outp));
    p.link(src.src(raw_src::RawAudioSrc::OUT), conv.sink(AudioConvert::IN))
        .expect("link rawaudiosrc->audioconvert");
    p.link(conv.src(AudioConvert::OUT), sink.sink(FileSink::IN)).expect("link audioconvert->sink");
    p.run().expect("run");

    let got = std::fs::read(&outp).unwrap();
    let _ = std::fs::remove_file(&outp);

    let want = convert_interleaved_vec(SampleFormat::S16, SampleFormat::S32, 2, &pcm).unwrap();
    assert_eq!(got.len(), want.len(), "inferred-input conversion length (S16->S32, 2x wider)");
    assert_eq!(
        got, want,
        "AudioConvert::new inferred S16/2ch/44100 from negotiated caps and converted correctly"
    );
}
