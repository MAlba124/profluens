//! Access units out of the PES payloads of one stream. Video is cut at start codes, a DVD
//! subpicture unit is gathered to the length its first two bytes give.

/// A PES packet's timestamps, raw 33-bit 90 kHz.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub pts: u64,
    pub dts: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Video {
    Mpeg12,
    Mpeg4,
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Header,
    Picture,
    Other,
}

/// A start code's class and, for a picture, whether a decoder starts there. `None` wants more.
fn classify(kind: Video, b: &[u8]) -> Option<(Class, bool)> {
    let code = *b.get(3)?;
    Some(match kind {
        Video::Mpeg12 => match code {
            0xB3 | 0xB8 => (Class::Header, false),
            // picture_coding_type 1 is I (ISO/IEC 13818-2 Table 6-12).
            0x00 => (Class::Picture, (b.get(5)? >> 3) & 7 == 1),
            _ => (Class::Other, false),
        },
        Video::Mpeg4 => match code {
            0xB0 | 0xB3 | 0xB5 | 0x00..=0x2F => (Class::Header, false),
            // vop_coding_type 0 is I (ISO/IEC 14496-2 6.3.5).
            0xB6 => (Class::Picture, b.get(4)? >> 6 == 0),
            _ => (Class::Other, false),
        },
        // H.264 7.4.1.2.3, a slice with first_mb_in_slice 0 or a non-VCL unit ahead of one.
        Video::H264 => match code & 0x1F {
            1 | 5 => match b.get(4)? & 0x80 {
                0 => (Class::Other, false),
                _ => (Class::Picture, code & 0x1F == 5),
            },
            6..=9 | 14..=18 => (Class::Header, false),
            _ => (Class::Other, false),
        },
        // H.265 7.4.2.4.4, first_slice_segment_in_pic_flag, IRAP is 16 to 23.
        Video::Hevc => match (code >> 1) & 0x3F {
            t @ 0..=31 => match b.get(5)? & 0x80 {
                0 => (Class::Other, false),
                _ => (Class::Picture, (16..=23).contains(&t)),
            },
            32..=35 | 39 | 41..=44 | 48..=55 => (Class::Header, false),
            _ => (Class::Other, false),
        },
    })
}

pub struct Au<'a> {
    pub bytes: &'a [u8],
    pub stamp: Option<Stamp>,
    pub key: bool,
    /// File byte of the PES packet the unit starts in.
    pub offset: u64,
}

/// Cuts video access units at the start code that begins the next one.
pub struct Framer {
    kind: Video,
    buf: Vec<u8>,
    max: usize,
    scanned: usize,
    started: bool,
    pic: bool,
    key: bool,
    stamp: Option<Stamp>,
    /// A PES stamp, where its payload starts in `buf`, waiting for the first picture behind it.
    pending: Option<(Stamp, usize)>,
    pes: [(usize, u64); 2],
    offset: u64,
    pub overflows: u64,
}

impl Framer {
    #[allow(clippy::disallowed_methods)] // once per stream
    pub fn new(kind: Video, reserve: usize, max: usize) -> Framer {
        Framer {
            kind,
            buf: Vec::with_capacity(reserve.min(max)),
            max,
            scanned: 0,
            started: false,
            pic: false,
            key: false,
            stamp: None,
            pending: None,
            pes: [(0, 0); 2],
            offset: 0,
            overflows: 0,
        }
    }

    pub fn kind(&self) -> Video {
        self.kind
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        (self.scanned, self.started, self.pic, self.key) = (0, false, false, false);
        (self.stamp, self.pending) = (None, None);
    }

    fn offset_at(&self, at: usize) -> u64 {
        if at >= self.pes[1].0 { self.pes[1].1 } else { self.pes[0].1 }
    }

    fn cut(&mut self, at: usize) {
        self.buf.drain(..at);
        self.scanned = self.scanned.saturating_sub(at);
        if let Some((_, from)) = &mut self.pending {
            *from = from.saturating_sub(at);
        }
        for p in &mut self.pes {
            p.0 = p.0.saturating_sub(at);
        }
    }

    pub fn push(
        &mut self,
        payload: &[u8],
        stamp: Option<Stamp>,
        offset: u64,
        emit: &mut impl FnMut(Au<'_>),
    ) {
        if self.buf.len() + payload.len() > self.max {
            self.overflows += 1;
            self.reset();
        }
        let base = self.buf.len();
        if let Some(stamp) = stamp {
            self.pending = Some((stamp, base));
        }
        self.pes = [self.pes[1], (base, offset)];
        self.buf.extend_from_slice(payload);
        self.scan(emit);
    }

    fn scan(&mut self, emit: &mut impl FnMut(Au<'_>)) {
        let mut i = self.scanned;
        loop {
            let Some(j) = self.buf.get(i..).and_then(|b| b.windows(3).position(|w| w == [0, 0, 1]))
            else {
                i = self.buf.len().saturating_sub(2).max(i);
                break;
            };
            let j = i + j;
            let Some((class, key)) = classify(self.kind, &self.buf[j..]) else {
                i = j;
                break;
            };
            let mut j = j;
            // The leading zero of a four byte start code goes with the unit it starts.
            let at = match self.kind {
                Video::H264 | Video::Hevc if j > 0 && self.buf[j - 1] == 0 => j - 1,
                _ => j,
            };
            if class != Class::Other && self.pic {
                emit(Au {
                    bytes: &self.buf[..at],
                    stamp: self.stamp.take(),
                    key: self.key,
                    offset: self.offset,
                });
                self.offset = self.offset_at(at);
                self.cut(at);
                (self.pic, self.key, j) = (false, false, j - at);
            }
            if class != Class::Other && !self.started {
                self.offset = self.offset_at(at);
                self.cut(at);
                (self.started, j) = (true, j - at);
            }
            // H.222.0 2.4.3.7 stamps the picture start code, FFmpeg's muxers the unit's first
            // byte, so a stamp from at or before either is the unit's.
            let first = class != Class::Other && j <= 1 && !self.pic;
            let open = (first || class == Class::Picture) && self.started && self.stamp.is_none();
            if open && self.pending.is_some_and(|(_, from)| from <= j) {
                self.stamp = self.pending.take().map(|(s, _)| s);
            }
            if class == Class::Picture && self.started {
                (self.pic, self.key) = (true, key);
            }
            i = j + 3;
        }
        self.scanned = i;
        if !self.started && i > 0 {
            self.cut(i);
        }
    }

    pub fn flush(&mut self, emit: &mut impl FnMut(Au<'_>)) {
        if self.pic {
            emit(Au { bytes: &self.buf, stamp: self.stamp, key: self.key, offset: self.offset });
        }
        self.reset();
    }
}

/// Gathers a DVD subpicture unit, its first two bytes its size (DVD-Video SPU header).
pub struct Spu {
    buf: Vec<u8>,
    need: usize,
    stamp: Option<Stamp>,
    offset: u64,
    pub broken: u64,
    lost: bool,
}

impl Spu {
    #[allow(clippy::disallowed_methods)] // once per stream
    pub fn new() -> Spu {
        Spu { buf: Vec::with_capacity(0x1_0000), need: 0, stamp: None, offset: 0, broken: 0, lost: false }
    }

    pub fn reset(&mut self) {
        (self.need, self.stamp) = (0, None);
        self.buf.clear();
    }

    /// A stamped packet starts a unit, an unstamped one goes on with the unit in progress.
    pub fn push(
        &mut self,
        payload: &[u8],
        stamp: Option<Stamp>,
        offset: u64,
        emit: &mut impl FnMut(Au<'_>),
    ) {
        // after a rejected unit only a stamped packet starts the next one
        if stamp.is_none() && self.lost {
            return;
        }
        if stamp.is_some() || self.buf.is_empty() {
            if !self.buf.is_empty() {
                self.broken += 1;
            }
            self.reset();
            let Some(size) =
                payload.get(..2).map(|s| usize::from(u16::from_be_bytes([s[0], s[1]])))
            else {
                self.lost = true;
                return;
            };
            if size < 4 {
                self.broken += 1;
                self.lost = true;
                return;
            }
            self.lost = false;
            (self.need, self.stamp, self.offset) = (size, stamp, offset);
        }
        let take = (self.need - self.buf.len()).min(payload.len());
        self.buf.extend_from_slice(&payload[..take]);
        if self.buf.len() == self.need {
            emit(Au { bytes: &self.buf, stamp: self.stamp, key: true, offset: self.offset });
            self.reset();
        }
    }
}

impl Default for Spu {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test vectors
mod tests {
    use super::*;

    type Got = Vec<(Vec<u8>, Option<u64>, bool, u64)>;

    fn run(f: &mut Framer, pieces: &[(&[u8], Option<u64>)]) -> Got {
        let mut got = Vec::new();
        let mut emit =
            |au: Au<'_>| got.push((au.bytes.to_vec(), au.stamp.map(|s| s.pts), au.key, au.offset));
        for (n, (p, pts)) in pieces.iter().enumerate() {
            f.push(p, pts.map(|pts| Stamp { pts, dts: None }), n as u64 * 100, &mut emit);
        }
        f.flush(&mut emit);
        got
    }

    const SEQ: [u8; 6] = [0, 0, 1, 0xB3, 1, 2];
    const I: [u8; 6] = [0, 0, 1, 0x00, 0x00, 0x08];
    const P: [u8; 6] = [0, 0, 1, 0x00, 0x00, 0x10];
    const SLICE: [u8; 5] = [0, 0, 1, 0x01, 9];

    #[test]
    fn mpeg_video_is_cut_ahead_of_the_header_of_the_next_picture() {
        let a: Vec<u8> = [&[7, 7][..], &SEQ, &I, &SLICE, &P[..2]].concat();
        let b: Vec<u8> = [&P[2..], &SLICE, &SEQ, &I, &SLICE].concat();
        let mut f = Framer::new(Video::Mpeg12, 64, 1 << 20);
        let got = run(&mut f, &[(&a, Some(3000)), (&b, Some(6000))]);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].0, [&SEQ[..], &I, &SLICE].concat(), "junk ahead is dropped");
        assert_eq!((got[0].1, got[0].2, got[0].3), (Some(3000), true, 0));
        assert_eq!(got[1].0, [&P[..], &SLICE].concat());
        assert_eq!((got[1].1, got[1].2, got[1].3), (None, false, 0), "its start code began in a");
        assert_eq!((got[2].1, got[2].2, got[2].3), (Some(6000), true, 100));
    }

    #[test]
    fn any_cut_of_the_payloads_gives_the_same_units() {
        let es: Vec<u8> = [&SEQ[..], &I, &SLICE, &P, &SLICE, &P, &SLICE, &SEQ, &I, &SLICE].concat();
        let mut want = Framer::new(Video::Mpeg12, 64, 1 << 20);
        let want: Vec<Vec<u8>> = run(&mut want, &[(&es, None)]).into_iter().map(|u| u.0).collect();
        assert_eq!(want.len(), 4);
        for n in 1..es.len() {
            let pieces: Vec<(&[u8], Option<u64>)> = es.chunks(n).map(|c| (c, None)).collect();
            let mut f = Framer::new(Video::Mpeg12, 0, 1 << 20);
            let got: Vec<Vec<u8>> = run(&mut f, &pieces).into_iter().map(|u| u.0).collect();
            assert_eq!(got, want, "pieces of {n}");
        }
    }

    #[test]
    fn h264_units_start_at_their_access_unit_delimiter_with_its_zero() {
        let aud = [0, 0, 0, 1, 0x09, 0xF0];
        let idr = [0, 0, 1, 0x65, 0x88, 1];
        let non = [0, 0, 1, 0x41, 0x9A, 1];
        let second_slice = [0, 0, 1, 0x41, 0x40, 2];
        let es: Vec<u8> = [&aud[..], &idr, &aud, &non, &second_slice, &aud, &non].concat();
        let mut f = Framer::new(Video::H264, 64, 1 << 20);
        let got = run(&mut f, &[(&es, Some(0))]);
        let lens: Vec<usize> = got.iter().map(|u| u.0.len()).collect();
        assert_eq!(lens, [12, 18, 12]);
        assert!(got.iter().all(|u| u.0.starts_with(&[0, 0, 0, 1, 9])));
        assert_eq!(got.iter().map(|u| u.2).collect::<Vec<_>>(), [true, false, false]);
    }

    #[test]
    fn hevc_and_mpeg4_keys() {
        let es: Vec<u8> =
            [&[0, 0, 1, 0x40, 1, 0][..], &[0, 0, 1, 0x26, 1, 0xAF], &[0, 0, 1, 0x02, 1, 0x80]]
                .concat();
        let got = run(&mut Framer::new(Video::Hevc, 0, 1 << 20), &[(&es, None)]);
        assert_eq!(
            got.iter().map(|u| (u.0.len(), u.2)).collect::<Vec<_>>(),
            [(12, true), (6, false)]
        );
        let es: Vec<u8> =
            [&[0, 0, 1, 0xB0, 1][..], &[0, 0, 1, 0xB6, 0x10], &[0, 0, 1, 0xB6, 0x50]].concat();
        let got = run(&mut Framer::new(Video::Mpeg4, 0, 1 << 20), &[(&es, None)]);
        assert_eq!(
            got.iter().map(|u| (u.0.len(), u.2)).collect::<Vec<_>>(),
            [(10, true), (5, false)]
        );
    }

    #[test]
    fn a_unit_past_the_limit_is_dropped() {
        let big = vec![0x55u8; 100];
        let es: Vec<u8> = [&SEQ[..], &I, &big].concat();
        let mut f = Framer::new(Video::Mpeg12, 0, 64);
        let got =
            run(&mut f, &[(&es[..60], None), (&es[60..], None), (&[&SEQ[..], &I].concat(), None)]);
        assert_eq!((got.len(), f.overflows), (1, 1));
    }

    #[test]
    fn a_subpicture_unit_is_gathered_to_its_size() {
        let mut s = Spu::new();
        let mut got = Vec::new();
        let mut emit = |au: Au<'_>| got.push((au.bytes.to_vec(), au.stamp.map(|s| s.pts)));
        let st = Some(Stamp { pts: 90, dts: None });
        s.push(&[0, 6, 0, 4], st, 0, &mut emit);
        s.push(&[0xFF, 0xEE, 0xDD], None, 1, &mut emit);
        s.push(&[0, 5, 0, 4, 1], st, 2, &mut emit);
        s.push(&[0, 9, 0], st, 3, &mut emit);
        s.push(&[0, 4, 0, 2], st, 4, &mut emit);
        assert_eq!(
            got,
            [
                (vec![0, 6, 0, 4, 0xFF, 0xEE], Some(90)),
                (vec![0, 5, 0, 4, 1], Some(90)),
                (vec![0, 4, 0, 2], Some(90))
            ]
        );
        assert_eq!(s.broken, 1, "the unit cut short by a stamped packet");
    }

    #[test]
    fn an_unstamped_packet_after_a_rejected_unit_is_dropped() {
        let mut s = Spu::new();
        let mut got = Vec::new();
        let mut emit = |au: Au<'_>| got.push(au.bytes.to_vec());
        let st = Some(Stamp { pts: 90, dts: None });
        s.push(&[0, 2, 9], st, 0, &mut emit);
        s.push(&[0, 4, 7, 7], None, 1, &mut emit);
        s.push(&[0, 4, 0, 2], st, 2, &mut emit);
        assert_eq!(got, [vec![0, 4, 0, 2]]);
        assert_eq!(s.broken, 1);
    }
}
