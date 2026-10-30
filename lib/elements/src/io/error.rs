//! Failures of the file elements (`filesrc`, `filesink`, `growingfilesrc`).

use std::fmt;
use std::path::Path;

use profluens_core::io::IoError;

#[derive(Debug)]
pub enum FileError {
    Open {
        path: Box<Path>,
        kind: IoError,
    },
    Create {
        path: Box<Path>,
        kind: IoError,
    },
    /// `process()` or a patch ran before `start()`. `element` is the element name.
    NotStarted {
        element: &'static str,
    },
    CompletionOutsideWindow {
        element: &'static str,
        seq: u64,
    },
    /// A positioned read failed. `offset` is the emit watermark, where one is tracked.
    Read {
        path: Box<Path>,
        offset: Option<u64>,
        kind: IoError,
    },
    Write {
        path: Box<Path>,
        kind: IoError,
    },
    /// The writer aborted the download. `emitted` bytes went downstream.
    WriterAborted {
        path: Box<Path>,
        emitted: u64,
        downloaded: u64,
    },
    WriterAbandoned {
        path: Box<Path>,
        emitted: u64,
    },
    Truncated {
        path: Box<Path>,
        declared: u64,
        readable: u64,
    },
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileError::Open { path, kind } => write!(f, "open {}: {kind}", path.display()),
            FileError::Create { path, kind } => write!(f, "create {}: {kind}", path.display()),
            FileError::NotStarted { element } => write!(f, "{element} not started"),
            FileError::CompletionOutsideWindow { element, seq } => {
                write!(f, "{element}: completion {seq} outside the credit window")
            }
            FileError::Read { path, offset: Some(offset), kind } => {
                write!(f, "read of {} failed at offset {offset}: {kind}", path.display())
            }
            FileError::Read { path, offset: None, kind } => {
                write!(f, "read of {} failed: {kind}", path.display())
            }
            FileError::Write { path, kind } => {
                write!(f, "write to {} failed: {kind}", path.display())
            }
            FileError::WriterAborted { path, emitted, downloaded } => write!(
                f,
                "growingfilesrc: the writer aborted the download of {} after {emitted} of \
                 {downloaded} byte(s), ending the stream",
                path.display()
            ),
            FileError::WriterAbandoned { path, emitted } => write!(
                f,
                "growingfilesrc: every FrontierHandle for {} was dropped without finish() \
                 or abort() after {emitted} byte(s), treating the download as abandoned \
                 and ending the stream",
                path.display()
            ),
            FileError::Truncated { path, declared, readable } => write!(
                f,
                "growingfilesrc: {} is shorter than the {declared} byte(s) the writer \
                 declared, only {readable} byte(s) were readable (the frontier contract \
                 is watermark <= bytes actually written)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for FileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FileError::Open { kind, .. }
            | FileError::Create { kind, .. }
            | FileError::Read { kind, .. }
            | FileError::Write { kind, .. } => Some(kind),
            _ => None,
        }
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_one_line_and_carry_the_fields() {
        let p: Box<Path> = Path::new("/tmp/x.bin").into();
        let cases: Vec<(FileError, &[&str])> = vec![
            (
                FileError::Open { path: p.clone(), kind: IoError::NotFound },
                &["open /tmp/x.bin", "not found"],
            ),
            (FileError::Create { path: p.clone(), kind: IoError::Denied }, &["create /tmp/x.bin"]),
            (FileError::NotStarted { element: "filesrc" }, &["filesrc not started"]),
            (
                FileError::CompletionOutsideWindow { element: "filesrc", seq: 9 },
                &["filesrc", "completion 9", "credit window"],
            ),
            (
                FileError::Read { path: p.clone(), offset: Some(42), kind: IoError::BadHandle },
                &["read of /tmp/x.bin", "offset 42", "stale handle"],
            ),
            (
                FileError::Read { path: p.clone(), offset: None, kind: IoError::Os(5) },
                &["read of /tmp/x.bin failed"],
            ),
            (FileError::Write { path: p.clone(), kind: IoError::Os(28) }, &["write to /tmp/x.bin"]),
            (
                FileError::WriterAborted { path: p.clone(), emitted: 3, downloaded: 8 },
                &["aborted the download", "3 of 8 byte(s)"],
            ),
            (
                FileError::WriterAbandoned { path: p.clone(), emitted: 5 },
                &["abandoned", "after 5 byte(s)"],
            ),
            (
                FileError::Truncated { path: p.clone(), declared: 10, readable: 4 },
                &["is shorter than the 10 byte(s)", "only 4 byte(s)"],
            ),
        ];
        for (e, needles) in cases {
            let s = e.to_string();
            assert!(!s.contains('\n') && !s.contains('\u{2014}'), "{s:?}");
            for n in needles {
                assert!(s.contains(n), "{s:?} should mention {n:?}");
            }
        }
    }
}
