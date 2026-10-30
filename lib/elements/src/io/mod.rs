//! IO elements. They submit through `ctx.io()` and drain completions in `process()`.

mod error;
mod filesink;
mod filesrc;
mod growingfilesrc;

pub use error::FileError;
pub use filesink::FileSink;
pub use filesrc::FileSrc;
pub use growingfilesrc::{FrontierHandle, GrowingFileSrc};
