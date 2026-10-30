use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

#[derive(Debug)]
pub enum Error {
    Usage(String),
    Io { path: PathBuf, source: io::Error },
    Spawn { tool: String, source: io::Error },
    Tool { tool: String, status: ExitStatus, stderr: String },
    Missing(String),
    Parse(String),
    Failed(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(msg) => write!(f, "{msg}"),
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Spawn { tool, source } => write!(f, "cannot run {tool}: {source}"),
            Error::Tool { tool, status, stderr } => {
                write!(f, "{tool} failed, {status}")?;
                if !stderr.is_empty() {
                    write!(f, "\n{stderr}")?;
                }
                Ok(())
            }
            Error::Missing(msg) => write!(f, "{msg}"),
            Error::Parse(msg) => write!(f, "unexpected tool output, {msg}"),
            Error::Failed(n) => write!(f, "{n} fixtures failed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } | Error::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}
