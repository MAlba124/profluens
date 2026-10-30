//! Audio format vocabulary and the `audio/raw` negotiation offer.

use profluens_core::ctx::Ctx;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};

/// The negotiation family for uncompressed interleaved PCM.
pub const FAMILY: &str = "audio/raw";
/// Sample rate in Hz (`Value::Int`).
pub const FIELD_RATE: &str = "rate";
/// Channel count (`Value::Int`).
pub const FIELD_CHANNELS: &str = "channels";
/// Sample format, a categorical id, see [`SampleFormat::caps_name`].
pub const FIELD_SAMPLE: &str = "sample";
/// How far a compressed stream's stamps run ahead of presentation, in ns (`Value::Int`).
pub const FIELD_DELAY: &str = "delay";

/// What the sink offer of a decoder names so that a declared delay reaches it.
pub static DELAY_FIELDS: [FieldDesc; 1] =
    [FieldDesc { field: FIELD_DELAY, allowed: ConstraintDesc::Any, preferred: None }];

/// Most channels the per-channel DSP elements accept.
pub const MAX_CHANNELS: u16 = 64;

/// Highest sample rate those elements accept.
pub const MAX_SAMPLE_RATE: u32 = 1_048_575;

/// Interleaved PCM sample format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleFormat {
    U8,
    S16,
    S24,
    S32,
    F32,
}

impl SampleFormat {
    pub const fn bytes(self) -> usize {
        match self {
            SampleFormat::U8 => 1,
            SampleFormat::S16 => 2,
            SampleFormat::S24 => 3,
            SampleFormat::S32 => 4,
            SampleFormat::F32 => 4,
        }
    }

    /// Bit depth, 24 for `S24` though it occupies 3 bytes.
    pub const fn bits(self) -> u32 {
        match self {
            SampleFormat::U8 => 8,
            SampleFormat::S16 => 16,
            SampleFormat::S24 => 24,
            SampleFormat::S32 => 32,
            SampleFormat::F32 => 32,
        }
    }

    pub const fn is_float(self) -> bool {
        matches!(self, SampleFormat::F32)
    }

    /// The categorical name used in `audio/raw` offers.
    pub const fn caps_name(self) -> &'static str {
        match self {
            SampleFormat::U8 => "u8",
            SampleFormat::S16 => "s16",
            SampleFormat::S24 => "s24",
            SampleFormat::S32 => "s32",
            SampleFormat::F32 => "f32",
        }
    }

    pub fn from_caps_name(s: &str) -> Option<SampleFormat> {
        Some(match s {
            "u8" => SampleFormat::U8,
            "s16" => SampleFormat::S16,
            "s24" => SampleFormat::S24,
            "s32" => SampleFormat::S32,
            "f32" => SampleFormat::F32,
            _ => return None,
        })
    }

    /// Resolve a WAV `(format tag, bits-per-sample)` pair.
    pub fn from_wav(tag: u16, bits: u16) -> Option<SampleFormat> {
        Some(match (tag, bits) {
            (1, 8) => SampleFormat::U8,
            (1, 16) => SampleFormat::S16,
            (1, 24) => SampleFormat::S24,
            (1, 32) => SampleFormat::S32,
            (3, 32) => SampleFormat::F32,
            _ => return None,
        })
    }
}

impl SampleFormat {
    pub const ALL: [SampleFormat; 5] = [
        SampleFormat::U8,
        SampleFormat::S16,
        SampleFormat::S24,
        SampleFormat::S32,
        SampleFormat::F32,
    ];

    /// The format of `takes` that holds this one best, itself when taken.
    pub fn nearest_in(self, takes: SampleFormats) -> Option<SampleFormat> {
        use SampleFormat::*;
        let rank: [SampleFormat; 5] = match self {
            U8 => [U8, S16, S24, S32, F32],
            S16 => [S16, S24, S32, F32, U8],
            S24 => [S24, S32, F32, S16, U8],
            S32 => [S32, F32, S24, S16, U8],
            F32 => [F32, S32, S24, S16, U8],
        };
        rank.into_iter().find(|f| takes.has(*f))
    }
}

/// A set of [`SampleFormat`]s, one bit each.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SampleFormats(u8);

impl SampleFormats {
    pub const NONE: SampleFormats = SampleFormats(0);

    pub const fn with(self, f: SampleFormat) -> SampleFormats {
        SampleFormats(self.0 | 1 << f as u8)
    }

    pub const fn has(self, f: SampleFormat) -> bool {
        self.0 & 1 << f as u8 != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn of_names<'a>(names: impl IntoIterator<Item = &'a str>) -> SampleFormats {
        names
            .into_iter()
            .filter_map(SampleFormat::from_caps_name)
            .fold(SampleFormats::NONE, SampleFormats::with)
    }
}

/// The sample formats everything downstream of src pad `pad` takes, by the solve.
pub fn link_samples(ctx: &Ctx, pad: SrcPad) -> SampleFormats {
    let Some(field) = ctx.field_id(FIELD_SAMPLE) else {
        return SampleFormats::NONE;
    };
    let Some(vals) = ctx.src_takes(pad, field) else {
        return SampleFormats::NONE;
    };
    let name = |v: &Value| match v {
        Value::Id(id) => ctx.value_name(*id),
        _ => None,
    };
    SampleFormats::of_names(vals.iter().filter_map(name))
}

/// A concrete interleaved-PCM format, the runtime form of a fixated `audio/raw`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
}

impl AudioFormat {
    pub const fn new(sample_rate: u32, channels: u16, format: SampleFormat) -> Self {
        Self { sample_rate, channels, format }
    }

    /// Bytes in one interchannel frame, the stride a parser must never split.
    pub const fn frame_stride(self) -> usize {
        self.channels as usize * self.format.bytes()
    }

    pub const fn bits_per_sample(self) -> u32 {
        self.format.bits()
    }

    pub const fn within_dsp_limits(self) -> bool {
        self.channels >= 1
            && self.channels <= MAX_CHANNELS
            && self.sample_rate >= 1
            && self.sample_rate <= MAX_SAMPLE_RATE
    }
}

impl std::fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} Hz {} ch {}", self.sample_rate, self.channels, self.format.caps_name())
    }
}

/// The `audio/raw` format fixed on `pad`, `None` until rate, channels and sample all resolve.
pub fn negotiated_audio_format(ctx: &Ctx, pad: SinkPad) -> Option<AudioFormat> {
    let fixed = ctx.negotiated_sink(pad)?;
    let int_field = |name: &str| -> Option<i64> {
        ctx.field_id(name).and_then(|id| fixed.get(id)).and_then(|v| match v {
            Value::Int(n) => Some(n),
            _ => None,
        })
    };
    let (rate, channels) = (int_field(FIELD_RATE)?, int_field(FIELD_CHANNELS)?);
    // Bounds too, an `i64` past `u16` or `u32` must not wrap into a plausible format.
    if rate <= 0 || channels <= 0 || rate > u32::MAX as i64 || channels > u16::MAX as i64 {
        return None;
    }
    let sample = ctx
        .field_id(FIELD_SAMPLE)
        .and_then(|id| fixed.get(id))
        .and_then(|v| match v {
            Value::Id(vid) => ctx.value_name(vid),
            _ => None,
        })
        .and_then(SampleFormat::from_caps_name)?;
    Some(AudioFormat::new(rate as u32, channels as u16, sample))
}

/// Announce `fmt` on src pad `pad` as `audio/raw`, with the channel `layout` where said.
pub fn announce_raw(ctx: &mut Ctx, pad: SrcPad, fmt: AudioFormat, layout: Option<u64>) {
    let fields = [
        (FIELD_RATE, ValueDesc::Int(i64::from(fmt.sample_rate))),
        (FIELD_CHANNELS, ValueDesc::Int(i64::from(fmt.channels))),
        (FIELD_SAMPLE, ValueDesc::Id(fmt.format.caps_name())),
        (crate::layout::FIELD_LAYOUT, ValueDesc::Int(layout.unwrap_or(0) as i64)),
    ];
    ctx.announce_format(pad, FAMILY, &fields[..3 + usize::from(layout.is_some())]);
}

static SAMPLE_VALUES: [ValueDesc; 5] = [
    ValueDesc::Id("u8"),
    ValueDesc::Id("s16"),
    ValueDesc::Id("s24"),
    ValueDesc::Id("s32"),
    ValueDesc::Id("f32"),
];

static RAW_ANY_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: FIELD_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: FIELD_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: FIELD_SAMPLE,
        allowed: ConstraintDesc::Set(&SAMPLE_VALUES),
        preferred: None,
    },
];

/// Any interleaved PCM, any rate, channels and known [`SampleFormat`].
pub static RAW_ANY_OFFER: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &RAW_ANY_FIELDS }];

/// A borrowed, validated view over interleaved PCM bytes.
pub struct AudioFrameRef<'a> {
    bytes: &'a [u8],
    format: AudioFormat,
}

impl<'a> AudioFrameRef<'a> {
    /// Validate once that `bytes` is a whole number of interchannel frames, then view it.
    pub fn new(bytes: &'a [u8], format: AudioFormat) -> Option<Self> {
        let stride = format.frame_stride();
        if stride == 0 || !bytes.len().is_multiple_of(stride) {
            return None;
        }
        Some(Self { bytes, format })
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    pub fn frames(&self) -> usize {
        self.bytes.len() / self.format.frame_stride()
    }

    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Sample `ch` of frame `frame` as a sign-normalised `i32`.
    pub fn sample_i32(&self, frame: usize, ch: usize) -> i32 {
        debug_assert!(!self.format.format.is_float(), "use sample_f32 for float PCM");
        let bps = self.format.format.bytes();
        let off = frame * self.format.frame_stride() + ch * bps;
        let b = &self.bytes[off..off + bps];
        match self.format.format {
            SampleFormat::U8 => b[0] as i32 - 128,
            SampleFormat::S16 => i16::from_le_bytes([b[0], b[1]]) as i32,
            SampleFormat::S24 => {
                let v = b[0] as i32 | (b[1] as i32) << 8 | (b[2] as i32) << 16;
                (v << 8) >> 8
            }
            SampleFormat::S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            SampleFormat::F32 => 0, // unreachable in practice (debug_assert above)
        }
    }

    pub fn sample_f32(&self, frame: usize, ch: usize) -> f32 {
        debug_assert!(self.format.format.is_float(), "sample_f32 is for F32 PCM only");
        let bps = self.format.format.bytes();
        let off = frame * self.format.frame_stride() + ch * bps;
        let b = &self.bytes[off..off + bps];
        f32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn a_format_goes_to_the_taken_one_that_holds_it_best() {
        use SampleFormat::*;
        let set = |fs: &[SampleFormat]| fs.iter().fold(SampleFormats::NONE, |s, f| s.with(*f));
        assert_eq!(F32.nearest_in(SampleFormats::NONE), None);
        for f in SampleFormat::ALL {
            assert_eq!(f.nearest_in(set(&SampleFormat::ALL)), Some(f), "taken as is");
            assert_eq!(f.nearest_in(set(&[f])), Some(f));
        }
        assert_eq!(S16.nearest_in(set(&[F32, S32])), Some(S32), "the narrowest exact one");
        assert_eq!(S32.nearest_in(set(&[S16, S24, F32])), Some(F32));
        assert_eq!(F32.nearest_in(set(&[U8, S16])), Some(S16), "narrowed as little as taken");
        assert_eq!(U8.nearest_in(set(&[F32])), Some(F32));
        assert_eq!(SampleFormats::of_names(["f32", "nope", "s16"]), set(&[S16, F32]));
    }

    #[test]
    fn sample_format_widths_and_names() {
        assert_eq!(SampleFormat::S16.bytes(), 2);
        assert_eq!(SampleFormat::S24.bytes(), 3);
        assert_eq!(SampleFormat::S24.bits(), 24);
        assert_eq!(SampleFormat::S16.caps_name(), "s16");
        assert_eq!(SampleFormat::from_caps_name("f32"), Some(SampleFormat::F32));
        assert_eq!(SampleFormat::from_caps_name("nope"), None);
        for f in [
            SampleFormat::U8,
            SampleFormat::S16,
            SampleFormat::S24,
            SampleFormat::S32,
            SampleFormat::F32,
        ] {
            assert_eq!(SampleFormat::from_caps_name(f.caps_name()), Some(f));
        }
    }

    #[test]
    fn from_wav_maps_common_pcm() {
        assert_eq!(SampleFormat::from_wav(1, 16), Some(SampleFormat::S16));
        assert_eq!(SampleFormat::from_wav(1, 24), Some(SampleFormat::S24));
        assert_eq!(SampleFormat::from_wav(3, 32), Some(SampleFormat::F32));
        assert_eq!(SampleFormat::from_wav(1, 12), None, "odd bit depth unsupported");
        assert_eq!(SampleFormat::from_wav(7, 16), None, "a-law unsupported");
    }

    #[test]
    fn frame_stride() {
        let f = AudioFormat::new(48_000, 2, SampleFormat::S16);
        assert_eq!(f.frame_stride(), 4);
        assert_eq!(AudioFormat::new(44_100, 1, SampleFormat::S24).frame_stride(), 3);
    }

    #[test]
    fn frame_view_reads_interleaved_samples() {
        let mut bytes = Vec::new();
        for s in [1i16, -1, 32767, -32768] {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let fmt = AudioFormat::new(48_000, 2, SampleFormat::S16);
        let view = AudioFrameRef::new(&bytes, fmt).expect("whole frames");
        assert_eq!(view.frames(), 2);
        assert_eq!(view.sample_i32(0, 0), 1);
        assert_eq!(view.sample_i32(0, 1), -1);
        assert_eq!(view.sample_i32(1, 0), 32767);
        assert_eq!(view.sample_i32(1, 1), -32768);
        assert!(AudioFrameRef::new(&bytes[..3], fmt).is_none());
    }

    #[test]
    fn s24_sign_extends() {
        let bytes = [0xFF, 0xFF, 0xFF];
        let fmt = AudioFormat::new(48_000, 1, SampleFormat::S24);
        let view = AudioFrameRef::new(&bytes, fmt).unwrap();
        assert_eq!(view.sample_i32(0, 0), -1);
    }
}
