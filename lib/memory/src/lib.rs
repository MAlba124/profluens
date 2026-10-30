//! Region allocators for a core [`Pool`](profluens_core::memory::Pool), outside core since they are
//! syscalls.

pub mod error;
pub mod mmap;
pub mod name;
pub mod smaps;
pub mod udmabuf;

pub use error::MemoryError;
