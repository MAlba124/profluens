use crate::dovi::{DoviError, RpuForm};
use crate::h26x::{H265_DOVI_RPU, H265_PREFIX_SEI, SEI_T35};
use crate::hdr10plus::{Hdr10Plus, Hdr10PlusError};
use crate::rpu::{Fit, single_layer};
use crate::side::{Kind, SIDE_CAP, records};
use crate::t35::{T35Kind, classify};

const START: [u8; 4] = [0, 0, 0, 1];

/// Where a NAL unit goes in its access unit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    BeforeSlices,
    AfterSlices,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Body<'a> {
    /// One SEI message, its type and size coded in front, trailing bits behind.
    Sei { payload_type: u32, payload: &'a [u8] },
    /// A Dolby Vision RPU from its `rpu_type` on, the 0x19 goes in front.
    Rpu(&'a [u8]),
}

/// The NAL unit one side record becomes in an H.265 access unit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unit<'a> {
    pub place: Place,
    pub nal_type: u8,
    pub body: Body<'a>,
}

/// What a stream takes of its pictures' side records.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Keep {
    pub hdr10plus: bool,
    /// Dolby Vision RPUs as a single layer's, their active area fitted.
    pub dovi: Option<Fit>,
}

impl Keep {
    pub fn any(&self) -> bool {
        self.hdr10plus || self.dovi.is_some()
    }
}

/// Why a record is not written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Skip {
    /// A kind the stream does not keep or this codec does not carry, or a T.35 message of no
    /// known kind.
    Unwritten,
    /// HDR10+ with processing windows past the whole picture, whose pixel coordinates are the
    /// source's.
    Windows(u8),
    Malformed(Hdr10PlusError),
    Rpu(DoviError),
}

/// Records written nowhere, counted per reason.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Dropped {
    pub windows: u64,
    pub malformed: u64,
    /// Past the room the caller gave.
    pub room: u64,
    /// Dolby Vision RPUs that did not read.
    pub rpu: u64,
}

/// Bytes written in front of the slices and behind them.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Written {
    pub before: usize,
    pub after: usize,
}

/// The NAL unit record `kind` becomes in a stream that keeps `keep`. `scratch` holds what a
/// record is rewritten to.
pub fn h265_unit<'a>(
    kind: Kind,
    bytes: &'a [u8],
    keep: &Keep,
    scratch: &'a mut [u8],
) -> Result<Unit<'a>, Skip> {
    let (form, rpu) = match kind {
        Kind::T35 => match classify(bytes) {
            T35Kind::Hdr10Plus { payload } => {
                if !keep.hdr10plus {
                    return Err(Skip::Unwritten);
                }
                let h = Hdr10Plus::parse(payload).map_err(Skip::Malformed)?;
                if h.num_windows != 1 {
                    return Err(Skip::Windows(h.num_windows));
                }
                let body = Body::Sei { payload_type: SEI_T35, payload: bytes };
                return Ok(Unit { place: Place::BeforeSlices, nal_type: H265_PREFIX_SEI, body });
            }
            T35Kind::DoviRpu { payload } => (RpuForm::Av1Emdf, payload),
            T35Kind::Other => return Err(Skip::Unwritten),
        },
        Kind::DoviRpu => (RpuForm::Nal, bytes),
        // the enhancement layer's pictures are not decoded, so not coded again
        Kind::DoviEl => return Err(Skip::Unwritten),
    };
    let fit = keep.dovi.as_ref().ok_or(Skip::Unwritten)?;
    let n = single_layer(form, rpu, fit, scratch).map_err(Skip::Rpu)?;
    let written: &'a [u8] = scratch;
    let body = Body::Rpu(&written[..n]);
    Ok(Unit { place: Place::AfterSlices, nal_type: H265_DOVI_RPU, body })
}

/// The NAL units of those of `side`'s records `keep` takes, in Annex B, those in front of the
/// slices onto `before` and those behind them onto `after`. A unit that does not fit is left
/// out whole.
pub fn h265_side(
    side: &[u8],
    keep: &Keep,
    before: &mut [u8],
    after: &mut [u8],
    dropped: &mut Dropped,
) -> Written {
    let mut w = Written::default();
    let mut scratch = [0u8; SIDE_CAP];
    for (kind, bytes) in records(side) {
        let Some(kind) = Kind::from_u8(kind) else {
            continue;
        };
        let unit = match h265_unit(kind, bytes, keep, &mut scratch) {
            Ok(u) => u,
            Err(Skip::Unwritten) => continue,
            Err(Skip::Windows(_)) => {
                dropped.windows += 1;
                continue;
            }
            Err(Skip::Malformed(_)) => {
                dropped.malformed += 1;
                continue;
            }
            Err(Skip::Rpu(_)) => {
                dropped.rpu += 1;
                continue;
            }
        };
        let (out, at) = match unit.place {
            Place::BeforeSlices => (&mut *before, &mut w.before),
            Place::AfterSlices => (&mut *after, &mut w.after),
        };
        match write_h265(&unit, &mut out[*at..]) {
            Some(n) => *at += n,
            None => dropped.room += 1,
        }
    }
    w
}

/// `u` as an Annex B NAL unit at the front of `out`, its length, `None` when it does not fit.
pub fn write_h265(u: &Unit<'_>, out: &mut [u8]) -> Option<usize> {
    let mut e = Ebsp { out, at: 0, zeros: 0 };
    e.raw(&START)?;
    e.raw(&[u.nal_type << 1, 1])?;
    match u.body {
        Body::Sei { payload_type, payload } => {
            e.sei_number(payload_type as usize)?;
            e.sei_number(payload.len())?;
            for &b in payload {
                e.byte(b)?;
            }
            e.byte(0x80)?;
        }
        Body::Rpu(rpu) => {
            e.byte(0x19)?;
            for &b in rpu {
                e.byte(b)?;
            }
        }
    }
    Some(e.at)
}

/// RBSP bytes out with emulation prevention (H.265 7.4.2).
struct Ebsp<'a> {
    out: &'a mut [u8],
    at: usize,
    zeros: u8,
}

impl Ebsp<'_> {
    fn put(&mut self, b: u8) -> Option<()> {
        *self.out.get_mut(self.at)? = b;
        self.at += 1;
        Some(())
    }

    fn raw(&mut self, bytes: &[u8]) -> Option<()> {
        for &b in bytes {
            self.put(b)?;
        }
        self.zeros = 0;
        Some(())
    }

    fn byte(&mut self, b: u8) -> Option<()> {
        if self.zeros >= 2 && b <= 3 {
            self.put(3)?;
            self.zeros = 0;
        }
        self.put(b)?;
        self.zeros = if b == 0 { self.zeros + 1 } else { 0 };
        Some(())
    }

    fn sei_number(&mut self, mut v: usize) -> Option<()> {
        while v >= 255 {
            self.byte(0xFF)?;
            v -= 255;
        }
        self.byte(v as u8)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use crate::bits::{BitWriter, unescape_into};
    use crate::h26x::{Codec, Scan, annexb_nals, scan_annexb};
    use crate::hdr10plus::sample_payload;
    use crate::rpu::{Sample, nal_form, sample};
    use crate::side::{FrameMeta, Writer};

    fn hdr10plus_t35(payload: Vec<u8>) -> Vec<u8> {
        let mut m = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
        m.extend(payload);
        m
    }

    fn side_of(records: &[(Kind, &[u8])]) -> Vec<u8> {
        let mut buf = [0u8; 4096];
        let mut w = Writer::new(&mut buf);
        for (k, b) in records {
            assert!(w.push(*k, b));
        }
        let n = w.len();
        buf[..n].to_vec()
    }

    fn two_windows() -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(1, 8).put(2, 2);
        for v in [0u32, 0, 959, 539, 480, 270] {
            w.put(v, 16);
        }
        w.put(0, 8).put(100, 16).put(200, 16).put(100, 16).put(0, 1);
        w.put(1000, 27).put(0, 1);
        for _ in 0..2 {
            for v in [100u32, 200, 300, 150] {
                w.put(v, 17);
            }
            w.put(0, 4).put(1, 10);
        }
        w.put(0, 1);
        for _ in 0..2 {
            w.put(0, 1).put(0, 1);
        }
        w.into_bytes()
    }

    const PLUS: Keep = Keep { hdr10plus: true, dovi: None };
    const HALF: Fit = Fit { from: (3840, 2160), turns: 0, to: (1920, 1080) };
    const BOTH: Keep = Keep { hdr10plus: true, dovi: Some(HALF) };

    fn kept(side: &[u8], keep: &Keep) -> (Vec<u8>, Vec<u8>, Dropped) {
        let (mut before, mut after, mut dropped) = ([0u8; 8192], [0u8; 8192], Dropped::default());
        let w = h265_side(side, keep, &mut before, &mut after, &mut dropped);
        (before[..w.before].to_vec(), after[..w.after].to_vec(), dropped)
    }

    fn written(side: &[u8]) -> (Vec<u8>, Vec<u8>, Dropped) {
        kept(side, &PLUS)
    }

    fn av1_rpu(rpu: &[u8]) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(0x01be_6841, 27).put(rpu.len() as u32, 8).put(0, 1);
        for &b in rpu {
            w.put(u32::from(b), 8);
        }
        w.put(0x400, 17).put(0x1F, 5);
        let mut m = vec![0xB5, 0x00, 0x3B, 0x00, 0x00, 0x08, 0x00];
        m.extend(w.into_bytes());
        m
    }

    fn scanned(au: &[u8]) -> Vec<u8> {
        let (mut side, mut scratch) = ([0u8; 4096], [0u8; 4096]);
        let mut scan = Scan::new(Writer::new(&mut side));
        scan_annexb(Codec::H265, au, &mut scratch, &mut scan);
        scan.side.bytes().to_vec()
    }

    #[test]
    fn an_hdr10plus_record_reads_back_through_the_sei_scan_as_it_was() {
        let t35 = hdr10plus_t35(sample_payload([60_000, 50_000, 40_000], 1000));
        let side = side_of(&[(Kind::T35, &t35)]);
        let (before, after, dropped) = written(&side);
        assert!(after.is_empty());
        assert_eq!(dropped, Dropped::default());
        assert_eq!(&before[..6], &[0, 0, 0, 1, H265_PREFIX_SEI << 1, 1]);
        assert_eq!(annexb_nals(&before).count(), 1);
        let back = scanned(&before);
        assert_eq!(back, side, "the same record bytes");
        let m = FrameMeta::read(&back).unwrap();
        assert_eq!(m.hdr10plus().unwrap().unwrap().windows[0].maxscl, [60_000, 50_000, 40_000]);
    }

    #[test]
    fn zero_runs_in_the_payload_are_escaped_and_unescape_to_the_rbsp() {
        let t35 = hdr10plus_t35(sample_payload([0, 0, 0], 0));
        assert!(t35.windows(3).any(|w| w == [0, 0, 0]), "the case under test");
        let side = side_of(&[(Kind::T35, &t35)]);
        let (before, ..) = written(&side);
        let body = &before[4..];
        for (i, w) in body.windows(3).enumerate() {
            assert!(!(w[..2] == [0, 0] && w[2] < 3), "a start code emulated at {i}");
        }
        assert!(body.windows(4).any(|w| w == [0, 0, 3, 0]), "escaped at least once");
        let mut rbsp = [0u8; 2048];
        let n = unescape_into(&body[2..], &mut rbsp);
        let size = t35.len();
        assert_eq!(&rbsp[..2], &[SEI_T35 as u8, size as u8]);
        assert_eq!(&rbsp[2..2 + size], &t35[..]);
        assert_eq!(&rbsp[2 + size..n], &[0x80]);
        assert_eq!(scanned(&before), side);
    }

    #[test]
    fn each_byte_up_to_three_after_two_zeros_gets_a_three_in_front() {
        let payload = [0, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0];
        let u = Unit {
            place: Place::BeforeSlices,
            nal_type: H265_PREFIX_SEI,
            body: Body::Sei { payload_type: 5, payload: &payload },
        };
        let mut out = [0u8; 64];
        let n = write_h265(&u, &mut out).unwrap();
        let want = [
            0, 0, 0, 1, 0x4E, 1, 5, 17, 0, 0, 3, 0, 0, 3, 0, 1, 0, 0, 3, 2, 0, 0, 3, 3, 0, 0, 4, 0,
            0, 0x80,
        ];
        assert_eq!(&out[..n], &want);
    }

    #[test]
    fn a_type_and_size_past_254_take_ff_bytes() {
        let payload = [0xAA; 300];
        let u = Unit {
            place: Place::BeforeSlices,
            nal_type: H265_PREFIX_SEI,
            body: Body::Sei { payload_type: 256, payload: &payload },
        };
        let mut out = [0u8; 400];
        let n = write_h265(&u, &mut out).unwrap();
        assert_eq!(&out[6..10], &[0xFF, 1, 0xFF, 45]);
        assert_eq!((n, out[n - 1]), (6 + 4 + 300 + 1, 0x80));
        assert_eq!(write_h265(&u, &mut out[..n - 1]), None, "one byte short");
    }

    #[test]
    fn hdr10plus_with_two_windows_is_dropped_and_counted() {
        let payload = two_windows();
        assert_eq!(Hdr10Plus::parse(&payload).unwrap().num_windows, 2);
        let t35 = hdr10plus_t35(payload);
        let side = side_of(&[(Kind::T35, &t35)]);
        let (before, after, dropped) = written(&side);
        assert!(before.is_empty() && after.is_empty());
        assert_eq!(dropped, Dropped { windows: 1, ..Dropped::default() });
        let mut scratch = [0u8; 16];
        assert_eq!(h265_unit(Kind::T35, &t35, &PLUS, &mut scratch), Err(Skip::Windows(2)));
    }

    #[test]
    fn a_cut_hdr10plus_is_dropped_as_malformed() {
        let mut payload = sample_payload([1000, 1000, 1000], 400);
        payload.truncate(8);
        let t35 = hdr10plus_t35(payload);
        let (before, _, dropped) = written(&side_of(&[(Kind::T35, &t35)]));
        assert!(before.is_empty());
        assert_eq!(dropped.malformed, 1);
    }

    #[test]
    fn what_the_stream_does_not_keep_is_not_written() {
        let rpu = nal_form(&sample(Sample::P8));
        let t35 = hdr10plus_t35(sample_payload([60_000, 50_000, 40_000], 1000));
        let a53 = [0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03];
        let side = side_of(&[
            (Kind::DoviRpu, &rpu),
            (Kind::DoviEl, &[]),
            (Kind::T35, &t35),
            (Kind::T35, &a53),
        ]);
        let (before, after, dropped) = kept(&side, &Keep::default());
        assert!(before.is_empty() && after.is_empty());
        assert_eq!(dropped, Dropped::default());
        let (before, after, _) = kept(&side, &PLUS);
        assert!(!before.is_empty() && after.is_empty());
        let (before, after, _) = kept(&side, &Keep { hdr10plus: false, dovi: Some(HALF) });
        assert!(before.is_empty() && !after.is_empty());
        assert!(!Keep::default().any() && PLUS.any() && BOTH.any());
    }

    #[test]
    fn an_rpu_goes_behind_the_slices_as_a_single_layers_with_its_area_fitted() {
        let fel = Sample { nlq: Some(true), ..Sample::P8 };
        let t35 = hdr10plus_t35(sample_payload([60_000, 50_000, 40_000], 1000));
        let side = side_of(&[
            (Kind::DoviRpu, &nal_form(&sample(fel))),
            (Kind::DoviEl, &[]),
            (Kind::T35, &t35),
        ]);
        let (before, after, dropped) = kept(&side, &BOTH);
        assert_eq!(dropped, Dropped::default());
        assert_eq!(annexb_nals(&before).count(), 1, "the HDR10+ SEI alone");
        assert_eq!(&after[..7], &[0, 0, 0, 1, H265_DOVI_RPU << 1, 1, 0x19]);
        assert_eq!(annexb_nals(&after).count(), 1);
        for w in after[4..].windows(3) {
            assert!(!(w[..2] == [0, 0] && w[2] < 3), "a start code emulated");
        }
        let want = sample(Sample { flat: true, area: Some([0, 0, 138, 138]), ..Sample::P8 });
        let back = scanned(&after);
        let m = FrameMeta::read(&back).unwrap();
        assert_eq!(m.dovi_rpu(), Some((RpuForm::Nal, &nal_form(&want)[..])));
        assert!(!m.dovi_el());
        assert_eq!(m.dovi_rpu_header().unwrap().unwrap().guess_profile(), Some(8));
    }

    #[test]
    fn an_av1_rpu_writes_the_nal_an_hevc_source_does() {
        let rpu = sample(Sample { area: None, second_list: false, ..Sample::P8 });
        assert!(rpu.len() < 256, "one size byte in the wrapping");
        let av1 = side_of(&[(Kind::T35, &av1_rpu(&rpu))]);
        let hevc = side_of(&[(Kind::DoviRpu, &nal_form(&rpu))]);
        let (_, after, dropped) = kept(&av1, &BOTH);
        assert_eq!(dropped, Dropped::default());
        assert!(!after.is_empty());
        assert_eq!(after, kept(&hevc, &BOTH).1);
        assert_eq!(scanned(&after), hevc);
    }

    #[test]
    fn an_rpu_that_does_not_read_is_dropped_and_counted() {
        let mut rpu = sample(Sample::P8);
        let at = rpu.len() - 7;
        rpu[at] ^= 1;
        let side = side_of(&[(Kind::DoviRpu, &nal_form(&rpu)), (Kind::DoviRpu, &[0x19, 0x08])]);
        let (before, after, dropped) = kept(&side, &BOTH);
        assert!(before.is_empty() && after.is_empty());
        assert_eq!(dropped, Dropped { rpu: 2, ..Dropped::default() });
        let mut scratch = [0u8; SIDE_CAP];
        assert_eq!(
            h265_unit(Kind::DoviRpu, &nal_form(&rpu), &BOTH, &mut scratch),
            Err(Skip::Rpu(DoviError::Crc))
        );
    }

    #[test]
    fn an_av1_hdr10plus_writes_the_sei_an_hevc_source_does() {
        let t35 = hdr10plus_t35(sample_payload([70_000, 1, 1], 1000));
        let mut itut = vec![4];
        itut.extend_from_slice(&t35);
        itut.extend_from_slice(&[0x80, 0x00]);
        let mut av1_side = [0u8; 1024];
        let mut scan = Scan::new(Writer::new(&mut av1_side));
        crate::av1::metadata(&itut, &mut scan);
        let av1 = scan.side.bytes().to_vec();
        let hevc = side_of(&[(Kind::T35, &t35)]);
        assert_eq!(av1, hevc, "the record forms agree");
        assert_eq!(written(&av1).0, written(&hevc).0);
    }

    #[test]
    fn what_does_not_fit_is_counted_and_left_out_whole() {
        let t35 = hdr10plus_t35(sample_payload([60_000, 50_000, 40_000], 1000));
        let side = side_of(&[(Kind::T35, &t35), (Kind::T35, &t35)]);
        let one = written(&side_of(&[(Kind::T35, &t35)])).0.len();
        let (mut before, mut after, mut dropped) = ([0u8; 4096], [0u8; 16], Dropped::default());
        let w = h265_side(&side, &PLUS, &mut before[..one + one / 2], &mut after, &mut dropped);
        assert_eq!((w.before, w.after, dropped.room), (one, 0, 1));
    }
}
