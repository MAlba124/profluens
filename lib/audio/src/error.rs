use std::fmt;

use profluens_core::ctx::Ctx;
use profluens_core::error::Error;

use crate::format::{AudioFormat, MAX_CHANNELS, MAX_SAMPLE_RATE, SampleFormat};
use crate::wav::WavError;

/// A failure of one audio element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioError {
    pub element: &'static str,
    pub kind: AudioErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioErrorKind {
    PcmBeforeFormat,
    NoInputFormat,
    RaggedFrame { len: usize },
    Convert { from: SampleFormat, to: SampleFormat },
    InputStrideUnknown,
    UnsupportedFormat { format: AudioFormat },
    OutputStrideUnknown,
    SlotTooSmall { capacity: usize, stride: usize },
    NoEngine,
    NoRoom,
    UnsupportedRate { num: u16, den: u16 },
    FormatChangeIgnored { latched: AudioFormat, offered: AudioFormat },
    WavHeaderBudget { buffered: usize, budget: usize },
    Wav(WavError),
}

pub(crate) fn failed(ctx: &Ctx, element: &'static str, kind: AudioErrorKind) -> Error {
    Error::element_failed(ctx.element(), AudioError { element, kind })
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.element, self.kind)?;
        if matches!(self.kind, AudioErrorKind::FormatChangeIgnored { .. }) {
            let element = self.element;
            write!(f, " Rebuild the branch (or insert a fresh {element}) to follow the change.")?;
        }
        Ok(())
    }
}

impl fmt::Display for AudioErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use AudioErrorKind as K;
        match self {
            K::PcmBeforeFormat => f.write_str(
                "PCM arrived before an audio/raw input format was known (construct the \
                 element with_input, or feed an announcing upstream)",
            ),
            K::NoInputFormat => f.write_str("input format unknown"),
            K::RaggedFrame { len } => {
                write!(f, "ragged frame, {len} bytes are not whole interchannel frames")
            }
            K::Convert { from, to } => {
                write!(f, "conversion from {} to {} failed", from.caps_name(), to.caps_name())
            }
            K::InputStrideUnknown => f.write_str("input frame stride unknown"),
            K::UnsupportedFormat { format } => write!(
                f,
                "input format {format} is outside the supported range (1 to {MAX_CHANNELS} \
                 channels, 1 to {MAX_SAMPLE_RATE} Hz)"
            ),
            K::OutputStrideUnknown => f.write_str("output frame stride unknown"),
            K::SlotTooSmall { capacity, stride } => write!(
                f,
                "pool slot of {capacity} bytes is smaller than one interchannel frame of \
                 {stride} bytes"
            ),
            K::NoEngine => f.write_str("no engine"),
            K::NoRoom => f.write_str("input buffer made no room"),
            K::UnsupportedRate { num, den } => {
                write!(f, "a rate of {num}/{den} is outside 1/4 to 4, the rate stays as it is")
            }
            K::FormatChangeIgnored { latched, offered } => write!(
                f,
                "ignoring a mid-stream input format change ({latched} -> {offered}), the \
                 input format is fixed at the first negotiation and every later buffer is \
                 still processed as the latched one."
            ),
            K::WavHeaderBudget { buffered, budget } => write!(
                f,
                "no WAV data chunk within header budget ({buffered} bytes buffered, budget \
                 {budget})"
            ),
            K::Wav(e) => write!(f, "bad WAV, {e}"),
        }
    }
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavError::NeedMore => f.write_str("header incomplete, more bytes needed"),
            WavError::NotRiff => f.write_str("not a RIFF file"),
            WavError::NotWave => f.write_str("RIFF form is not WAVE"),
            WavError::BadFmt => f.write_str("fmt chunk too short, zero channels or zero rate"),
            WavError::NoFmt => f.write_str("data chunk before any fmt chunk"),
            WavError::Unsupported { tag, bits } => {
                write!(f, "unsupported PCM encoding (format tag {tag}, {bits} bits)")
            }
            WavError::Malformed => f.write_str("chunk sizes overflow"),
        }
    }
}

impl std::error::Error for AudioError {}
impl std::error::Error for AudioErrorKind {}
impl std::error::Error for WavError {}

#[cfg(test)]
mod tests {
    use super::*;
    use AudioErrorKind as K;

    #[test]
    fn the_element_names_its_own_failures() {
        let kind = K::SlotTooSmall { capacity: 3, stride: 8 };
        let e = AudioError { element: "audiogain", kind };
        assert_eq!(
            format!("{e}"),
            "audiogain: pool slot of 3 bytes is smaller than one interchannel frame of 8 bytes"
        );
    }

    #[test]
    fn a_relatch_warning_reads_as_caps_and_names_the_fix() {
        let latched = AudioFormat::new(48_000, 2, SampleFormat::S16);
        let offered = AudioFormat::new(44_100, 2, SampleFormat::S16);
        let e = AudioError {
            element: "audioresample",
            kind: K::FormatChangeIgnored { latched, offered },
        };
        let s = format!("{e}");
        let needles = [
            "audioresample: ignoring",
            "48000 Hz 2 ch s16 -> 44100 Hz 2 ch s16",
            "fresh audioresample",
        ];
        for n in needles {
            assert!(s.contains(n), "{s:?} should mention {n:?}");
        }
    }

    #[test]
    fn every_message_is_one_plain_line() {
        let f = AudioFormat::new(48_000, 2, SampleFormat::S16);
        let all = [
            K::PcmBeforeFormat,
            K::NoInputFormat,
            K::RaggedFrame { len: 7 },
            K::Convert { from: SampleFormat::F32, to: SampleFormat::S24 },
            K::InputStrideUnknown,
            K::OutputStrideUnknown,
            K::SlotTooSmall { capacity: 3, stride: 8 },
            K::NoEngine,
            K::NoRoom,
            K::UnsupportedRate { num: 9, den: 2 },
            K::FormatChangeIgnored { latched: f, offered: f },
            K::WavHeaderBudget { buffered: 2 << 20, budget: 1 << 20 },
            K::Wav(WavError::Unsupported { tag: 85, bits: 16 }),
        ];
        for kind in all {
            let s = format!("{}", AudioError { element: "audiotest", kind });
            assert!(s.starts_with("audiotest: "), "{s}");
            assert!(!s.contains('\n') && !s.contains('\u{2014}') && !s.ends_with(' '), "{s}");
        }
        let s = format!("{}", K::Convert { from: SampleFormat::F32, to: SampleFormat::S24 });
        assert_eq!(s, "conversion from f32 to s24 failed");
        let s = format!("{}", K::Wav(WavError::Unsupported { tag: 85, bits: 16 }));
        assert!(s.contains("tag 85") && s.contains("16 bits"), "{s}");
    }
}
