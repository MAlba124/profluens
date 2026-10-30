pub const OPUS_RATE: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Application {
    Voip,
    Audio,
    /// CELT only.
    LowDelay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Auto,
    Voice,
    Music,
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    /// Must be 48000, resample upstream.
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_bps: u32,
    pub application: Application,
    /// 25, 50, 100, 200, 400 or 600 (2.5 to 60 ms).
    pub frame_ms_tenths: u16,
    pub vbr: bool,
    pub vbr_constrained: bool,
    /// 0 to 10.
    pub complexity: u8,
    pub signal: Signal,
}

impl EncoderConfig {
    /// 20 ms VBR audio at complexity 10, 64 kbps mono or 96 kbps stereo.
    #[must_use]
    pub fn new(channels: u8) -> Self {
        Self {
            sample_rate: OPUS_RATE,
            channels,
            bitrate_bps: if channels >= 2 { 96_000 } else { 64_000 },
            application: Application::Audio,
            frame_ms_tenths: 200,
            vbr: true,
            vbr_constrained: false,
            complexity: 10,
            signal: Signal::Auto,
        }
    }
}
