//! Opens, stats, renames and unlinks as queue entries, so a cold cache stalls a kernel worker
//! and not the group. Names are copied into reused blocks held until the op lands.

use std::ffi::OsStr;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use profluens_core::io::{Handle, IoError, IoResult, NO_LEN, Op, OpenMode, Submission};
use rustix::io::Errno;
use stein_io::queue::{self, Cqe, Mem, STATX_LEN};
use stein_io::{CWD, Mode, OFlags};

use crate::hygiene::Hint;
use crate::ops::Started;
use crate::{Kind, NONE, OsReactor, StreamHygiene, errno, sys};

const SMALL: usize = 1024;
pub(crate) const SMALL_NAME: usize = (SMALL - STATX_LEN) / 2 - 1;
/// The longest name the kernel takes (`PATH_MAX` less its terminator).
const MAX_NAME: usize = 4095;
const LARGE: usize = 2 * (MAX_NAME + 1) + STATX_LEN;
const MAX_BLOCKS: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Step {
    First,
    Unlink,
    Create,
    Again,
}

fn layout(size: usize) -> (usize, usize) {
    let half = (size - STATX_LEN) / 2;
    (half, size - STATX_LEN)
}

fn name(block: &mut [u8], at: usize) -> Mem {
    let len = block[at..].iter().position(|b| *b == 0).map_or(0, |n| n + 1);
    sys::block(block, at, len)
}

impl OsReactor {
    pub(crate) fn path_submit(&mut self, s: Submission, names: &[u8]) {
        let (a, b) = match s.kind {
            Op::Open { path, .. } | Op::Stat { path } | Op::Unlink { path } => {
                (path.of(names), &[][..])
            }
            Op::Rename { from, to } => (from.of(names), to.of(names)),
            _ => return self.complete(s, IoResult::Err(IoError::Unsupported)),
        };
        if a.contains(&0) || b.contains(&0) {
            return self.complete(s, IoResult::Err(IoError::Os(Errno::INVAL.raw_os_error())));
        }
        if a.len().max(b.len()) > MAX_NAME {
            return self.complete(s, IoResult::Err(IoError::Os(Errno::NAMETOOLONG.raw_os_error())));
        }
        let Some(block) = self.take_block(a.len().max(b.len()) > SMALL_NAME) else {
            return self.complete(s, IoResult::Err(IoError::Full));
        };
        let room = &mut self.blocks[block as usize];
        let (half, _) = layout(room.len());
        room[..a.len()].copy_from_slice(a);
        room[a.len()] = 0;
        room[half..half + b.len()].copy_from_slice(b);
        room[half + b.len()] = 0;
        let Some(idx) = self.pend(s, Handle::NONE) else {
            return self.free_block(block);
        };
        if let Some(p) = self.ops[idx as usize].as_mut() {
            p.block = block;
        }
        self.waiting.push_back(idx);
    }

    // Made once and kept.
    #[allow(clippy::disallowed_methods)]
    fn take_block(&mut self, large: bool) -> Option<u32> {
        let size = if large { LARGE } else { SMALL };
        if let Some(at) =
            self.free_blocks.iter().rposition(|b| self.blocks[*b as usize].len() == size)
        {
            return Some(self.free_blocks.swap_remove(at));
        }
        if self.blocks.len() >= MAX_BLOCKS {
            return None;
        }
        self.blocks.push(vec![0; size].into_boxed_slice());
        Some(self.blocks.len() as u32 - 1)
    }

    pub(crate) fn free_block(&mut self, block: u32) {
        self.free_blocks.push(block);
    }

    pub(crate) fn start_path(&mut self, idx: u32) -> Started {
        let Some(p) = self.ops[idx as usize].as_ref() else {
            return Started::Wait;
        };
        let (kind, step, block) = (p.kind, p.step, p.block);
        let room = &mut self.blocks[block as usize];
        let (half, at) = layout(room.len());
        let (first, second) = (name(room, 0), name(room, half));
        let out = sys::block(room, at, STATX_LEN);
        let read = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let create = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC;
        let op = match (kind, step) {
            (Op::Open { mode: OpenMode::Create, .. }, Step::First) => {
                queue::Op::Statx { dir: CWD, path: first, out, follow: false }
            }
            (Op::Open { mode: OpenMode::Create, .. }, Step::Unlink) => {
                queue::Op::Unlink { dir: CWD, path: first, rmdir: false }
            }
            (Op::Open { mode: OpenMode::Create, .. }, _) => queue::Op::Open {
                dir: CWD,
                path: first,
                flags: create,
                mode: Mode::from_raw_mode(0o666),
            },
            (Op::Open { .. }, _) => {
                queue::Op::Open { dir: CWD, path: first, flags: read, mode: Mode::empty() }
            }
            (Op::Stat { .. }, _) => queue::Op::Statx { dir: CWD, path: first, out, follow: true },
            (Op::Rename { .. }, _) => {
                queue::Op::Rename { from_dir: CWD, from: first, to_dir: CWD, to: second }
            }
            (Op::Unlink { .. }, _) => queue::Op::Unlink { dir: CWD, path: first, rmdir: false },
            _ => return Started::Done(IoResult::Err(IoError::Unsupported)),
        };
        self.push(idx, op)
    }

    pub(crate) fn landed_path(&mut self, idx: u32, c: &Cqe) {
        let Some(p) = self.ops[idx as usize].as_ref() else {
            return;
        };
        let (kind, step) = (p.kind, p.step);
        match (kind, step) {
            (Op::Open { mode: OpenMode::Create, .. }, _) => self.step_create(idx, c, step),
            (Op::Open { mode, .. }, _) => match c.result() {
                Ok(fd) => self.opened(idx, sys::adopt(fd as RawFd), mode),
                Err(e) => self.finish(idx, IoResult::Err(errno(e))),
            },
            (Op::Stat { .. }, _) => {
                let result = match (c.result(), self.statx(idx)) {
                    (Ok(_), Some((_, len))) => IoResult::File { handle: Handle::NONE, len },
                    (Ok(_), None) => IoResult::Err(IoError::Os(Errno::IO.raw_os_error())),
                    (Err(e), _) => IoResult::Err(errno(e)),
                };
                self.finish(idx, result);
            }
            _ => self.finish(idx, crate::ops::moved(c)),
        }
    }

    /// A file the element holds open, served on a duplicate so the reactor owns what it closes.
    pub(crate) fn adopt(&mut self, s: Submission, fd: RawFd, mode: OpenMode) {
        let result = match (mode, sys::dup(fd)) {
            (OpenMode::Create, _) => IoResult::Err(IoError::Unsupported),
            (_, Ok(fd)) => self.file_of(fd, mode),
            (_, Err(e)) => IoResult::Err(errno(e)),
        };
        self.complete(s, result);
    }

    fn opened(&mut self, idx: u32, fd: OwnedFd, mode: OpenMode) {
        let result = self.file_of(fd, mode);
        self.finish(idx, result);
    }

    fn file_of(&mut self, fd: OwnedFd, mode: OpenMode) -> IoResult {
        let st = match rustix::fs::fstat(&fd) {
            Ok(st) => st,
            Err(e) => return IoResult::Err(errno(e)),
        };
        let len = st.st_size as u64;
        let kind = match st.st_mode & libc::S_IFMT {
            libc::S_IFDIR => return IoResult::Err(IoError::NotAFile),
            libc::S_IFIFO => Kind::Pipe,
            _ => {
                let mut hygiene = StreamHygiene::with_window(self.hygiene_window);
                // Only a read-once file drops pages behind the read, a rerun would otherwise go to
                // disk.
                if mode != OpenMode::ReadOnce {
                    hygiene.disable_read();
                }
                Kind::File { hygiene }
            }
        };
        let (pipe, handle) = (matches!(kind, Kind::Pipe), self.insert(kind, fd));
        match handle {
            Ok(handle) if pipe => IoResult::File { handle, len: NO_LEN },
            Ok(handle) => {
                self.hint(handle, Hint::Sequential);
                IoResult::File { handle, len }
            }
            Err(e) => IoResult::Err(e),
        }
    }

    /// A create unlinks a regular file instead of truncating. Truncating a large file frees extents
    /// through the journal and stalls the first writes (ext4 over LUKS, 1.5 to 3 s). A symlink is
    /// kept.
    fn step_create(&mut self, idx: u32, c: &Cqe, step: Step) {
        match step {
            Step::First => {
                let regular = c.result().is_ok()
                    && self.statx(idx).is_some_and(|st| st.0 & libc::S_IFMT == libc::S_IFREG);
                self.next_step(idx, if regular { Step::Unlink } else { Step::Create });
            }
            // An unlink that failed leaves the open to truncate.
            Step::Unlink => self.next_step(idx, Step::Create),
            Step::Create if c.result() == Err(Errno::NOENT) && self.make_parent(idx) => {
                self.next_step(idx, Step::Again);
            }
            _ => match c.result() {
                Ok(fd) => {
                    let mut hygiene = StreamHygiene::with_window(self.hygiene_window);
                    hygiene.disable_read();
                    let result = match self.insert(Kind::File { hygiene }, sys::adopt(fd as RawFd))
                    {
                        Ok(handle) => {
                            self.hint(handle, Hint::Sequential);
                            IoResult::File { handle, len: 0 }
                        }
                        Err(e) => IoResult::Err(e),
                    };
                    self.finish(idx, result);
                }
                Err(e) => self.finish(idx, IoResult::Err(errno(e))),
            },
        }
    }

    // The reactor makes a create's parent directories.
    #[allow(clippy::disallowed_methods)]
    fn make_parent(&mut self, idx: u32) -> bool {
        let Some(block) = self.ops[idx as usize].as_ref().map(|p| p.block) else {
            return false;
        };
        let room = &self.blocks[block as usize];
        let len = room.iter().position(|b| *b == 0).unwrap_or(0);
        let path = Path::new(OsStr::from_bytes(&room[..len]));
        path.parent()
            .is_some_and(|dir| !dir.as_os_str().is_empty() && std::fs::create_dir_all(dir).is_ok())
    }

    fn next_step(&mut self, idx: u32, step: Step) {
        if let Some(p) = self.ops[idx as usize].as_mut() {
            p.step = step;
        }
        self.restart(idx);
    }

    fn statx(&self, idx: u32) -> Option<(u32, u64)> {
        let block = self.ops[idx as usize].as_ref()?.block;
        let room = self.blocks.get(block as usize)?;
        let st = queue::statx_of(&room[layout(room.len()).1..])?;
        Some((u32::from(st.stx_mode), st.stx_size))
    }

    pub(crate) fn path_cancelled(&mut self, idx: u32, c: &Cqe) {
        let Some(p) = self.ops[idx as usize].as_ref() else {
            return;
        };
        let opens = match (p.kind, p.step) {
            (Op::Open { mode: OpenMode::Create, .. }, step) => {
                matches!(step, Step::Create | Step::Again)
            }
            (Op::Open { .. }, step) => step == Step::First,
            _ => false,
        };
        if opens && c.res >= 0 {
            drop(sys::adopt(c.res as RawFd));
        }
    }

    pub(crate) fn path_done(&mut self, block: u32) {
        if block != NONE {
            self.free_block(block);
        }
    }
}
