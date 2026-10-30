use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryError {
    DeviceOpen { path: &'static str, errno: i32 },
    Memfd { errno: i32 },
    Truncate { len: usize, errno: i32 },
    Seal { errno: i32 },
    Map { len: usize, errno: i32 },
    Create { slot: u32, errno: i32 },
    Unaligned { stride: usize, page: usize },
    Sync { errno: i32 },
    Dup { errno: i32 },
    NotDmabuf,
    Name { errno: i32 },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let os = |errno: i32| std::io::Error::from_raw_os_error(errno);
        match self {
            MemoryError::DeviceOpen { path, errno } => write!(
                f,
                "{path} cannot be opened ({}), a udev uaccess rule for it or membership of its group (kvm) lets a user export dmabufs",
                os(*errno)
            ),
            MemoryError::Memfd { errno } => write!(f, "memfd_create failed ({})", os(*errno)),
            MemoryError::Truncate { len, errno } => {
                write!(f, "ftruncate of the memfd to {len} bytes failed ({})", os(*errno))
            }
            MemoryError::Seal { errno } => {
                write!(f, "F_SEAL_SHRINK on the memfd failed ({})", os(*errno))
            }
            MemoryError::Map { len, errno } => {
                write!(f, "mmap of {len} bytes failed ({})", os(*errno))
            }
            MemoryError::Create { slot, errno } => {
                write!(f, "UDMABUF_CREATE of slot {slot} failed ({})", os(*errno))
            }
            MemoryError::Unaligned { stride, page } => {
                write!(f, "a slot stride of {stride} bytes is not whole pages of {page}")
            }
            MemoryError::Sync { errno } => write!(f, "DMA_BUF_IOCTL_SYNC failed ({})", os(*errno)),
            MemoryError::Dup { errno } => write!(f, "dup of the dmabuf fd failed ({})", os(*errno)),
            MemoryError::NotDmabuf => write!(f, "the memory is not a udmabuf slot"),
            MemoryError::Name { errno } => write!(f, "naming the mapping failed ({})", os(*errno)),
        }
    }
}

impl std::error::Error for MemoryError {}

pub(crate) fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
