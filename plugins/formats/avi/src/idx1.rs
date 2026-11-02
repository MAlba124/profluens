use std::convert::Infallible;

use profluens_core::fetch::{Fetch, Need, Step};

use crate::element::{IDX1_PIECE, MAX_HOPS, MAX_IDX1};

/// The entries of an `idx1` chunk as [`Idx1At`] reads them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Idx1Body {
    pub body: Vec<u8>,
    /// No entry the chunk declares is cut by the file's end or [`MAX_IDX1`].
    pub whole: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Idx1State {
    Start,
    /// A read from `base` of chunk heads, the one at `off` in it next.
    Heads {
        base: u64,
        off: usize,
        hops: u8,
    },
    /// The body past the `held` bytes the heads read brought, `want` long.
    Body {
        held: usize,
        want: usize,
        whole: bool,
    },
}

/// The `idx1` chunk behind the `movi` list ending at `movi_end` in a file of `len` bytes, the
/// demuxer's walk as one machine. A first read of [`IDX1_PIECE`] takes the heads and a short
/// index whole, one more the rest. Up to [`MAX_HOPS`] chunks before it are hopped over. None when
/// no `idx1` follows.
#[derive(Debug)]
pub struct Idx1At {
    movi_end: u64,
    len: u64,
    state: Idx1State,
    buf: Vec<u8>,
    got: usize,
}

impl Idx1At {
    // Empty until the first read, cold, once per open.
    #[allow(clippy::disallowed_methods)]
    pub fn new(movi_end: u64, len: u64) -> Self {
        Idx1At { movi_end, len, state: Idx1State::Start, buf: Vec::new(), got: 0 }
    }

    fn heads_at(&mut self, at: u64, hops: u8) -> Step<Option<Idx1Body>> {
        if at.saturating_add(8) > self.len {
            return self.done(None);
        }
        let len = (self.len - at).min(u64::from(IDX1_PIECE)) as usize;
        self.buf.clear();
        self.buf.resize(len, 0);
        self.state = Idx1State::Heads { base: at, off: 0, hops };
        Step::Need(Need { at, len })
    }

    fn done(&mut self, out: Option<Idx1Body>) -> Step<Option<Idx1Body>> {
        self.state = Idx1State::Start;
        Step::Done(out)
    }

    fn heads(&mut self, base: u64, mut off: usize, mut hops: u8) -> Step<Option<Idx1Body>> {
        loop {
            let Some(head) = self.buf[..self.got].get(off..off + 8) else {
                // a short read is the end of the file, a head cut by the read's end is read again
                return match self.got < self.buf.len() {
                    true => self.done(None),
                    false => self.heads_at(base + off as u64, hops),
                };
            };
            let size = u64::from(u32::from_le_bytes([head[4], head[5], head[6], head[7]]));
            let at = base + off as u64;
            if &head[..4] != b"idx1" {
                let next = at.saturating_add(8).saturating_add(size + (size & 1));
                let plausible = head[..4].iter().all(|b| b.is_ascii_alphanumeric() || *b == b' ');
                if !plausible || next.saturating_add(8) > self.len || hops == MAX_HOPS {
                    return self.done(None);
                }
                hops += 1;
                match usize::try_from(next - base).ok().filter(|&o| o < self.got) {
                    Some(o) => off = o,
                    None => return self.heads_at(next, hops),
                }
                continue;
            }
            let start = at + 8;
            let declared = start.saturating_add(size);
            let end = declared.min(start.saturating_add(MAX_IDX1)).min(self.len);
            let want = (end - start) as usize;
            let from = off + 8;
            let held = (self.got - from).min(want);
            self.buf.truncate(from + held);
            self.buf.drain(..from);
            if held == want {
                let body = std::mem::take(&mut self.buf);
                return self.done(Some(Idx1Body { body, whole: end == declared }));
            }
            self.buf.resize(want, 0);
            self.state = Idx1State::Body { held, want, whole: end == declared };
            return Step::Need(Need { at: start + held as u64, len: want - held });
        }
    }
}

impl Fetch for Idx1At {
    type Out = Option<Idx1Body>;
    type Err = Infallible;

    fn poll(&mut self) -> Result<Step<Option<Idx1Body>>, Infallible> {
        Ok(match self.state {
            Idx1State::Start => self.heads_at(self.movi_end, 0),
            Idx1State::Heads { base, off, hops } => self.heads(base, off, hops),
            Idx1State::Body { held, want, whole } => {
                self.buf.truncate(held + self.got);
                let body = std::mem::take(&mut self.buf);
                self.done(Some(Idx1Body { body, whole: whole && held + self.got == want }))
            }
        })
    }

    fn buf(&mut self) -> &mut [u8] {
        match self.state {
            Idx1State::Body { held, .. } => &mut self.buf[held..],
            _ => &mut self.buf,
        }
    }

    fn filled(&mut self, n: usize) {
        self.got = n.min(self.buf().len());
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods, clippy::unwrap_used)]
mod tests {
    use super::*;
    use profluens_core::fetch::drive_slice_log;

    const MOVI_END: usize = 100;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    /// `n` entries of 16 octets, each its own index in its first byte.
    fn entries(n: usize) -> Vec<u8> {
        (0..n).flat_map(|k| [[k as u8; 1].as_slice(), &[0; 15]].concat()).collect()
    }

    fn file(tail: &[Vec<u8>]) -> Vec<u8> {
        [vec![7u8; MOVI_END], tail.concat()].concat()
    }

    fn read(file: &[u8]) -> (Option<Idx1Body>, Vec<Need>) {
        let mut log = Vec::new();
        let mut m = Idx1At::new(MOVI_END as u64, file.len() as u64);
        let got = drive_slice_log(&mut m, file, &mut log).unwrap();
        (got, log)
    }

    fn need(at: usize, len: usize) -> Need {
        Need { at: at as u64, len }
    }

    #[test]
    fn a_short_idx1_is_one_read() {
        let body = entries(10);
        let f = file(&[chunk(b"idx1", &body)]);
        let (got, log) = read(&f);
        assert_eq!(got, Some(Idx1Body { body, whole: true }));
        assert_eq!(log, [need(MOVI_END, f.len() - MOVI_END)]);
    }

    #[test]
    fn a_long_idx1_is_two_reads_where_the_demuxer_takes_a_piece_each() {
        let body = entries(10_000);
        let f = file(&[chunk(b"idx1", &body)]);
        let (got, log) = read(&f);
        assert_eq!(got, Some(Idx1Body { body: body.clone(), whole: true }));
        let piece = IDX1_PIECE as usize;
        assert_eq!(log, [need(MOVI_END, piece), need(MOVI_END + piece, body.len() + 8 - piece)]);
        assert_eq!((body.len() + 8).div_ceil(piece), 3, "the demuxer reads it in 3");
    }

    #[test]
    fn chunks_before_it_are_hopped_in_the_read_or_past_it() {
        let body = entries(4);
        let f = file(&[chunk(b"JUNK", &[0; 25]), chunk(b"idx1", &body)]);
        let (got, log) = read(&f);
        assert_eq!((got.unwrap().body, log.len()), (body.clone(), 1));
        let big = vec![0u8; IDX1_PIECE as usize + 100];
        let f = file(&[chunk(b"JUNK", &big), chunk(b"idx1", &body)]);
        let (got, log) = read(&f);
        assert_eq!(got.unwrap().body, body);
        let behind = MOVI_END + 8 + big.len();
        assert_eq!(log, [need(MOVI_END, IDX1_PIECE as usize), need(behind, f.len() - behind)]);
        let five: Vec<Vec<u8>> = (0..5).map(|_| chunk(b"JUNK", &[0; 4])).collect();
        let f = file(&[five.concat(), chunk(b"idx1", &body)]);
        assert_eq!(read(&f).0, None, "more chunks than the demuxer hops");
        let f = file(&[chunk(b"\x00\x01ab", &[0; 4]), chunk(b"idx1", &body)]);
        assert_eq!(read(&f).0, None, "no chunk id");
    }

    #[test]
    fn a_head_cut_by_the_first_read_is_read_again() {
        let body = entries(4);
        let pad = IDX1_PIECE as usize - 8 - 4;
        let f = file(&[chunk(b"JUNK", &vec![0; pad]), chunk(b"idx1", &body)]);
        let (got, log) = read(&f);
        assert_eq!(got.unwrap().body, body);
        let at = MOVI_END + 8 + pad;
        assert_eq!(log, [need(MOVI_END, IDX1_PIECE as usize), need(at, f.len() - at)]);
    }

    #[test]
    fn a_cut_or_absent_idx1_is_told() {
        let body = entries(10);
        let mut f = file(&[chunk(b"idx1", &body)]);
        f.truncate(f.len() - 20);
        let (got, _) = read(&f);
        assert_eq!(got, Some(Idx1Body { body: body[..body.len() - 20].to_vec(), whole: false }));
        let mut long = file(&[chunk(b"idx1", &entries(10_000))]);
        long.truncate(long.len() - 5);
        let mut m = Idx1At::new(MOVI_END as u64, long.len() as u64 + 5);
        let got = drive_slice_log(&mut m, &long, &mut Vec::new()).unwrap().unwrap();
        assert!(!got.whole && got.body.len() == 160_000 - 5, "a read short of the length");
        assert_eq!(read(&file(&[])).0, None, "nothing behind the movi");
        assert_eq!(read(&file(&[vec![1, 2, 3]])).0, None, "a cut head");
        assert_eq!(read(&file(&[chunk(b"JUNK", &[0; 4])])).0, None);
        let mut m = Idx1At::new(MOVI_END as u64, f.len() as u64);
        let first = drive_slice_log(&mut m, &f, &mut Vec::new());
        assert_eq!(drive_slice_log(&mut m, &f, &mut Vec::new()), first, "done starts over");
    }
}
