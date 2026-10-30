use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::Error;

pub fn root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest).to_path_buf()
}

/// Removed with its contents on drop.
pub struct TempDir {
    pub path: PathBuf,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn temp_dir(tag: &str) -> Result<TempDir, Error> {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let base = std::env::temp_dir();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    loop {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("xtask-{tag}-{}-{nanos:x}-{seq}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(TempDir { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(Error::Io { path, source }),
        }
    }
}

const TAIL_LINES: usize = 20;

pub fn tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim_end();
    let start = text.rmatch_indices('\n').nth(TAIL_LINES - 1).map_or(0, |(i, _)| i + 1);
    text[start..].to_string()
}

/// Stdout of `cmd`, the stderr tail in the error. `replay` passes stderr on.
pub fn exec(tool: &str, cmd: &mut Command, replay: bool) -> Result<Vec<u8>, Error> {
    let out = cmd.output().map_err(|source| Error::Spawn { tool: tool.to_string(), source })?;
    if !out.status.success() {
        return Err(Error::Tool {
            tool: tool.to_string(),
            status: out.status,
            stderr: tail(&out.stderr),
        });
    }
    if replay && !out.stderr.is_empty() {
        let _ = std::io::stderr().write_all(&out.stderr);
    }
    Ok(out.stdout)
}

pub fn run(tool: &str, cmd: &mut Command) -> Result<(), Error> {
    exec(tool, cmd.stdout(Stdio::inherit()), true).map(drop)
}

pub fn run_silent(tool: &str, cmd: &mut Command) -> Result<(), Error> {
    exec(tool, cmd.stdout(Stdio::null()), false).map(drop)
}

pub fn capture(tool: &str, cmd: &mut Command) -> Result<Vec<u8>, Error> {
    exec(tool, cmd, true)
}

pub fn run_live(tool: &str, cmd: &mut Command) -> Result<(), Error> {
    let status = cmd.status().map_err(|source| Error::Spawn { tool: tool.to_string(), source })?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Tool { tool: tool.to_string(), status, stderr: String::new() })
    }
}

pub fn which(name: &OsStr) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if name.as_bytes().contains(&b'/') {
        let path = PathBuf::from(name);
        return is_executable(&path).then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir })
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn env_nonempty(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|v| !v.is_empty())
}

/// Sorted regular files ending in `suffix`, dot files left out, as the glob `dir/*suffix`.
pub fn glob(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.as_bytes();
            name.first() != Some(&b'.') && name.ends_with(suffix.as_bytes())
        })
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files
}

pub fn list(dir: &Path) -> Result<Vec<OsString>, Error> {
    let entries = fs::read_dir(dir).map_err(|source| io_err(dir, source))?;
    let mut names: Vec<OsString> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .filter(|n| n.as_bytes().first() != Some(&b'.'))
        .collect();
    names.sort();
    Ok(names)
}

pub fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io { path: path.to_path_buf(), source }
}

pub fn mkdir_p(path: &Path) -> Result<(), Error> {
    fs::create_dir_all(path).map_err(|e| io_err(path, e))
}

pub fn read(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(|e| io_err(path, e))
}

pub fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    fs::write(path, bytes).map_err(|e| io_err(path, e))
}

pub fn copy(from: &Path, to: &Path) -> Result<(), Error> {
    fs::copy(from, to).map(drop).map_err(|e| io_err(from, e))
}

pub fn file_name(path: &Path) -> &OsStr {
    path.file_name().unwrap_or(path.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines() {
        let many: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let t = tail(many.as_bytes());
        assert!(t.starts_with("line 10\n"));
        assert!(t.ends_with("line 29"));
        assert_eq!(tail(b"one\ntwo\n"), "one\ntwo");
        assert_eq!(tail(b""), "");
    }

    #[test]
    fn which_checks_paths_with_a_slash() {
        assert_eq!(which(OsStr::new("/nonexistent/ffmpeg")), None);
        assert_eq!(which(OsStr::new("")), None);
    }
}
