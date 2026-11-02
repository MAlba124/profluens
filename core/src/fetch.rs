use std::fmt;
use std::io;

/// `len` bytes of a file from byte `at`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Need {
    pub at: u64,
    pub len: usize,
}

/// What a machine wants next, or its result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step<T> {
    Need(Need),
    Done(T),
}

/// One read machine, polled until [`Step::Done`]. Each [`Step::Need`] is answered with one
/// [`Fetch::filled`] before the next poll. A machine left with a Need unanswered, after a read
/// error, is dropped and not polled again.
pub trait Fetch {
    type Out;
    type Err;
    /// What the machine wants next, or its result.
    fn poll(&mut self) -> Result<Step<Self::Out>, Self::Err>;
    /// The buffer the caller reads the last Need into, `len` long, owned and reused by the
    /// machine.
    fn buf(&mut self) -> &mut [u8];
    /// `n` bytes of the buffer were read, fewer than asked only at the end of the file.
    fn filled(&mut self, n: usize);
}

/// Copies the bytes of `file` from `at` into `buf`, fewer past its end.
fn copy_at(file: &[u8], at: u64, buf: &mut [u8]) -> usize {
    let from = usize::try_from(at).map_or(file.len(), |a| a.min(file.len()));
    let n = buf.len().min(file.len() - from);
    buf[..n].copy_from_slice(&file[from..from + n]);
    n
}

/// Runs `m` over a file held in memory, a Need past its end reads short.
pub fn drive_slice<F: Fetch>(m: &mut F, file: &[u8]) -> Result<F::Out, F::Err> {
    loop {
        match m.poll()? {
            Step::Done(out) => return Ok(out),
            Step::Need(need) => {
                let buf = m.buf();
                debug_assert_eq!(buf.len(), need.len, "a buffer other than the Need");
                let n = copy_at(file, need.at, buf);
                m.filled(n);
            }
        }
    }
}

/// [`drive_slice`] with every Need appended to `log`, for tests that check what is read.
pub fn drive_slice_log<F: Fetch>(
    m: &mut F,
    file: &[u8],
    log: &mut Vec<Need>,
) -> Result<F::Out, F::Err> {
    loop {
        match m.poll()? {
            Step::Done(out) => return Ok(out),
            Step::Need(need) => {
                log.push(need);
                let buf = m.buf();
                debug_assert_eq!(buf.len(), need.len, "a buffer other than the Need");
                let n = copy_at(file, need.at, buf);
                m.filled(n);
            }
        }
    }
}

/// What [`drive_file`] refuses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileError<E> {
    /// The machine refused the bytes it read.
    Fetch(E),
    /// Reading `need` failed.
    Io { need: Need, kind: io::ErrorKind },
}

impl<E: fmt::Display> fmt::Display for FileError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileError::Fetch(e) => e.fmt(f),
            FileError::Io { need, kind } => {
                write!(f, "{} bytes at byte {} cannot be read, {kind}", need.len, need.at)
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for FileError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FileError::Fetch(e) => Some(e),
            FileError::Io { .. } => None,
        }
    }
}

/// Runs `m` over `file` with blocking positioned reads, app-side setup and sources that own
/// their file.
#[cfg(unix)]
// The caller's open file, read in place.
#[allow(clippy::disallowed_types)]
pub fn drive_file<F: Fetch>(m: &mut F, file: &std::fs::File) -> Result<F::Out, FileError<F::Err>> {
    loop {
        match m.poll().map_err(FileError::Fetch)? {
            Step::Done(out) => return Ok(out),
            Step::Need(need) => {
                let buf = m.buf();
                debug_assert_eq!(buf.len(), need.len, "a buffer other than the Need");
                let n =
                    read_full(file, need.at, buf).map_err(|kind| FileError::Io { need, kind })?;
                m.filled(n);
            }
        }
    }
}

/// Fills `buf` from byte `at` of `file`, fewer bytes only at its end.
#[cfg(unix)]
// Blocking preads by design, see `drive_file`.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
fn read_full(file: &std::fs::File, at: u64, buf: &mut [u8]) -> Result<usize, io::ErrorKind> {
    use std::os::unix::fs::FileExt as _;
    let mut got = 0;
    while got < buf.len() {
        // pread refuses an offset past i64::MAX, no file reaches it
        let Some(off) = at.checked_add(got as u64).filter(|&o| o <= i64::MAX as u64) else {
            break;
        };
        match file.read_at(&mut buf[got..], off) {
            Ok(0) => break,
            Ok(n) => got += n.min(buf.len() - got),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.kind()),
        }
    }
    Ok(got)
}

#[cfg(test)]
// Test fixtures allocate and touch the disk freely.
#[allow(clippy::disallowed_methods, clippy::disallowed_types, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Reads `chunk` bytes at a time from `at` until a short fill, the bytes summed. A 0xFF
    /// byte is refused.
    struct Sum {
        at: u64,
        chunk: usize,
        buf: [u8; 4],
        got: usize,
        sum: u64,
        asked: bool,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Refused(u64);

    impl fmt::Display for Refused {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "0xFF at byte {}", self.0)
        }
    }

    impl std::error::Error for Refused {}

    fn sum(at: u64, chunk: usize) -> Sum {
        Sum { at, chunk, buf: [0; 4], got: 0, sum: 0, asked: false }
    }

    impl Fetch for Sum {
        type Out = u64;
        type Err = Refused;

        fn poll(&mut self) -> Result<Step<u64>, Refused> {
            if self.asked {
                let read = &self.buf[..self.got];
                if let Some(i) = read.iter().position(|&b| b == 0xFF) {
                    return Err(Refused(self.at + i as u64));
                }
                self.sum += read.iter().map(|&b| u64::from(b)).sum::<u64>();
                self.at += self.got as u64;
                if self.got < self.chunk {
                    self.asked = false;
                    return Ok(Step::Done(self.sum));
                }
            }
            self.asked = true;
            Ok(Step::Need(Need { at: self.at, len: self.chunk }))
        }

        fn buf(&mut self) -> &mut [u8] {
            &mut self.buf[..self.chunk]
        }

        fn filled(&mut self, n: usize) {
            self.got = n.min(self.chunk);
        }
    }

    #[test]
    fn a_short_fill_is_the_end_of_the_file() {
        let file = [1u8, 2, 3, 4, 5, 6, 7];
        assert_eq!(drive_slice(&mut sum(0, 3), &file), Ok(28));
        let mut log = Vec::new();
        assert_eq!(drive_slice_log(&mut sum(2, 3), &file, &mut log), Ok(25));
        let at = |at, len| Need { at, len };
        assert_eq!(log, [at(2, 3), at(5, 3)], "the second read is short and ends it");
        log.clear();
        assert_eq!(drive_slice_log(&mut sum(3, 4), &file, &mut log), Ok(22));
        assert_eq!(log, [at(3, 4), at(7, 4)], "a fill of exactly the end asks once more");
    }

    #[test]
    fn a_need_past_the_end_reads_nothing() {
        let file = [1u8, 2];
        let mut log = Vec::new();
        assert_eq!(drive_slice_log(&mut sum(9, 2), &file, &mut log), Ok(0));
        assert_eq!(log, [Need { at: 9, len: 2 }]);
        assert_eq!(drive_slice(&mut sum(u64::MAX, 2), &file), Ok(0));
        assert_eq!(copy_at(&file, u64::MAX, &mut [0; 2]), 0);
    }

    #[test]
    fn a_machine_is_polled_by_hand_as_the_drivers_do() {
        let mut m = sum(1, 2);
        let Ok(Step::Need(need)) = m.poll() else { panic!("a read first") };
        assert_eq!(need, Need { at: 1, len: 2 });
        assert_eq!(m.buf().len(), need.len);
        m.buf().copy_from_slice(&[5, 6]);
        m.filled(2);
        assert_eq!(m.poll(), Ok(Step::Need(Need { at: 3, len: 2 })));
        m.filled(0);
        assert_eq!(m.poll(), Ok(Step::Done(11)));
    }

    #[test]
    fn the_machines_error_comes_through_either_driver() {
        let file = [1u8, 2, 3, 0xFF, 5];
        assert_eq!(drive_slice(&mut sum(0, 2), &file), Err(Refused(3)));
        let path = std::env::temp_dir().join(format!("pf-core-fetch-{}", std::process::id()));
        std::fs::write(&path, file).unwrap();
        let opened = std::fs::File::open(&path).unwrap();
        assert_eq!(drive_file(&mut sum(0, 2), &opened), Err(FileError::Fetch(Refused(3))));
        assert_eq!(drive_file(&mut sum(4, 2), &opened), Ok(5), "a short read at the end");
        assert_eq!(drive_file(&mut sum(u64::MAX - 1, 2), &opened), Ok(0), "past any file");
        let e = drive_file(&mut sum(0, 2), &opened).unwrap_err();
        assert_eq!(e.to_string(), "0xFF at byte 3");
        assert!(std::error::Error::source(&e).is_some());
        // a file open for writing alone refuses reads
        let write_only = std::fs::File::create(&path).unwrap();
        let e = drive_file(&mut sum(1, 2), &write_only).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(e, FileError::Io { need: Need { at: 1, len: 2 }, .. }), "{e:?}");
        assert!(e.to_string().starts_with("2 bytes at byte 1 cannot be read"), "{e}");
    }

    #[test]
    fn the_file_driver_reads_what_the_slice_driver_does() {
        let file: Vec<u8> = (0..1000u32).map(|i| (i % 200) as u8).collect();
        let path = std::env::temp_dir().join(format!("pf-core-fetch-same-{}", std::process::id()));
        std::fs::write(&path, &file).unwrap();
        let opened = std::fs::File::open(&path).unwrap();
        for (at, chunk) in [(0, 4), (3, 3), (997, 4), (1000, 1), (5000, 2)] {
            let want = drive_slice(&mut sum(at, chunk), &file);
            assert_eq!(drive_file(&mut sum(at, chunk), &opened), want.map_err(FileError::Fetch));
        }
        let _ = std::fs::remove_file(&path);
    }
}
