/// The sampling frequencies an index names, ISO/IEC 14496-3 Table 1.18.
pub const AAC_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// What an AudioSpecificConfig states (ISO/IEC 14496-3 1.6.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asc {
    /// The core's audio object type, behind an explicit SBR signal.
    pub object: u8,
    /// The core's sampling frequency.
    pub rate: u32,
    /// channelConfiguration, 0 when a program config element states the channels.
    pub config: u8,
    /// The decoder's output frequency, an explicitly signalled SBR extension's, else `rate`.
    pub out_rate: u32,
    pub sbr: bool,
    /// Explicitly signalled parametric stereo.
    pub ps: bool,
    /// frameLengthFlag, 960 samples a frame instead of 1024.
    pub short_frame: bool,
}

impl Asc {
    /// The channels the decoder puts out, None where a program config element states them.
    pub fn channels(&self) -> Option<u32> {
        let n = match self.config {
            1 if self.ps => 2,
            1..=6 => u32::from(self.config),
            7 | 12 | 14 => 8,
            11 => 7,
            13 => 24,
            _ => return None,
        };
        Some(n)
    }
}

struct Bits<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn take(&mut self, n: usize) -> Option<u32> {
        if self.left() < n {
            return None;
        }
        let mut v = 0u32;
        for i in self.at..self.at + n {
            v = v << 1 | u32::from(self.b[i / 8] >> (7 - i % 8) & 1);
        }
        self.at += n;
        Some(v)
    }

    fn left(&self) -> usize {
        self.b.len() * 8 - self.at
    }

    fn object(&mut self) -> Option<u8> {
        match self.take(5)? {
            31 => Some(32 + self.take(6)? as u8),
            n => Some(n as u8),
        }
    }

    fn rate(&mut self) -> Option<u32> {
        match self.take(4)? {
            15 => self.take(24),
            i => AAC_RATES.get(i as usize).copied(),
        }
    }
}

/// The fields of `asc`, None when it ends before its channelConfiguration or names no rate. An
/// extension that does not parse is left out, implicit SBR is not seen.
pub fn parse(asc: &[u8]) -> Option<Asc> {
    let mut r = Bits { b: asc, at: 0 };
    let mut object = r.object()?;
    let rate = r.rate()?;
    let config = r.take(4)? as u8;
    let mut a =
        Asc { object, rate, config, out_rate: rate, sbr: false, ps: false, short_frame: false };
    let hierarchical = object == 5 || object == 29;
    if hierarchical {
        (a.sbr, a.ps) = (true, object == 29);
        a.out_rate = r.rate().unwrap_or(rate);
        object = r.object().unwrap_or(0);
        a.object = object;
        if object == 22 {
            r.take(4);
        }
    }
    let general = matches!(object, 1..=4 | 6 | 7 | 17 | 19..=23);
    if !general || ga_config(&mut r, object, &mut a).is_none() || hierarchical {
        return Some(a);
    }
    if r.left() >= 16 && r.take(11) == Some(0x2b7) && r.object() == Some(5) && r.take(1) == Some(1)
    {
        if let Some(out) = r.rate() {
            (a.sbr, a.out_rate) = (true, out);
        }
        if r.left() >= 12 && r.take(11) == Some(0x548) {
            a.ps = r.take(1) == Some(1);
        }
    }
    Some(a)
}

/// GASpecificConfig and the epConfig behind it, None where an error protection config follows.
fn ga_config(r: &mut Bits<'_>, object: u8, a: &mut Asc) -> Option<()> {
    a.short_frame = r.take(1)? == 1;
    if r.take(1)? == 1 {
        r.take(14)?;
    }
    let extension = r.take(1)? == 1;
    // a program config element runs to its own length, nothing behind it is read
    if a.config == 0 {
        return None;
    }
    if object == 6 || object == 20 {
        r.take(3)?;
    }
    if extension {
        if object == 22 {
            r.take(16)?;
        }
        if matches!(object, 17 | 19 | 20 | 23) {
            r.take(3)?;
        }
        r.take(1)?;
    }
    if matches!(object, 17 | 19..=23) && r.take(2)? >= 2 {
        return None;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_aac_states_its_rate_and_channels() {
        let a = parse(&[0x11, 0x90]).expect("LC 48 kHz stereo");
        assert_eq!((a.object, a.rate, a.config, a.out_rate), (2, 48_000, 2, 48_000));
        assert_eq!((a.sbr, a.ps, a.channels()), (false, false, Some(2)));
        assert_eq!(parse(&[0x11, 0xB0]).and_then(|a| a.channels()), Some(6));
        assert_eq!(parse(&[0x11, 0xB8]).and_then(|a| a.channels()), Some(8), "config 7");
        assert_eq!(parse(&[0x11, 0x80]).and_then(|a| a.channels()), None, "a PCE");
        assert!(parse(&[0x11, 0x84]).expect("a PCE behind").short_frame);
        assert!(parse(&[0x11, 0x94]).expect("960").short_frame);
        assert_eq!(parse(&[0x11]), None, "cut");
        assert_eq!(parse(&[0x17, 0x10]), None, "a reserved frequency index");
    }

    #[test]
    fn escaped_object_types_and_frequencies_are_read() {
        // object 31 escapes to 32 + 6 bits, frequency index 15 to 24 bits, config 1
        let a = parse(&[0xF8, 0x1F, 0xFF, 0xFF, 0xFE, 0x20]).expect("escaped");
        assert_eq!((a.object, a.rate, a.config), (32, 0xFFFFFF, 1));
    }

    #[test]
    fn hierarchical_sbr_states_the_output_rate() {
        // object 5, core 24 kHz, stereo, extension 48 kHz, then object 2
        let a = parse(&[0x2B, 0x11, 0x88, 0x00]).expect("HE-AAC");
        assert_eq!((a.object, a.rate, a.out_rate, a.sbr, a.ps), (2, 24_000, 48_000, true, false));
        assert_eq!(a.channels(), Some(2));
        // object 29, mono core, parametric stereo out
        let v2 = parse(&[0xEB, 0x09, 0x88, 0x00]).expect("HE-AAC v2");
        assert_eq!((v2.config, v2.out_rate, v2.ps, v2.channels()), (1, 48_000, true, Some(2)));
    }

    #[test]
    fn the_sync_extension_states_sbr_and_ps_behind_a_plain_core() {
        // LC 24 kHz mono, GASpecificConfig, 0x2b7, object 5, sbr, 48 kHz, 0x548, ps
        let asc = [0x13, 0x08, 0x56, 0xE5, 0x9D, 0x48, 0x80];
        let a = parse(&asc).expect("sync extension");
        assert_eq!((a.object, a.rate, a.out_rate, a.sbr, a.ps), (2, 24_000, 48_000, true, true));
        assert_eq!(a.channels(), Some(2));
        let no_ps = parse(&asc[..5]).expect("no ps");
        assert_eq!(
            (no_ps.sbr, no_ps.out_rate, no_ps.ps, no_ps.channels()),
            (true, 48_000, false, Some(1))
        );
        // a sync word that is not 0x2b7 states nothing
        let other = parse(&[0x13, 0x08, 0x00, 0x00, 0x00]).expect("plain");
        assert_eq!((other.sbr, other.out_rate), (false, 24_000));
    }
}
