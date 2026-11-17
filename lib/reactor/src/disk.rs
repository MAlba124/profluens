//! Whether a file's storage pays for reads that race, asked once when the file opens.
//!
//! The ring runs each positioned read on a kernel worker of its own, so reads of adjacent ranges
//! reach the storage in any order and share one readahead state. FUSE then splits them into small
//! requests its threads serve in the order they take them, and a disk head seeks back for every
//! one that lost the race. Under a block file system the reads a seek left behind keep the head
//! from the seek's. A fast disk wants the depth.

use std::ffi::CStr;
use std::io::Write;
use std::os::fd::BorrowedFd;

use rustix::fs::{Mode, OFlags};

use crate::sys;

const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;
/// The device ids of a btrfs asked, ids are sparse after a device was removed.
const BTRFS_IDS: u64 = 64;
/// Room for `/sys/dev/block/<major>:<minor>/../queue/rotational`.
const PATH: usize = 64;

/// How reads of one file go out, see [`crate::OsReactor::set_reads`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reads {
    /// In order on FUSE and on rotational disks, free elsewhere.
    #[default]
    Auto,
    /// One at a time in submission order.
    InOrder,
    /// As many at once as the element keeps.
    Free,
}

impl Reads {
    pub(crate) fn in_order(self, fd: BorrowedFd<'_>, dev: u64) -> bool {
        match self {
            Reads::Auto => slow(fd, dev),
            Reads::InOrder => true,
            Reads::Free => false,
        }
    }
}

// `f_type` is narrower than i64 on some targets.
#[allow(clippy::unnecessary_cast)]
fn slow(fd: BorrowedFd<'_>, dev: u64) -> bool {
    match rustix::fs::fstatfs(fd).map(|st| st.f_type as i64) {
        Ok(FUSE_SUPER_MAGIC) => true,
        Ok(BTRFS_SUPER_MAGIC) => btrfs_rotational(fd),
        _ => rotational(dev),
    }
}

/// A btrfs file's device number names no block device, the file system tells its devices. In
/// order when any of them is rotational.
fn btrfs_rotational(fd: BorrowedFd<'_>) -> bool {
    let Ok(max_id) = sys::btrfs_max_id(fd) else {
        return false;
    };
    let mut room = sys::BtrfsDev::default();
    (1..=max_id.min(BTRFS_IDS)).any(|id| {
        sys::btrfs_dev_path(fd, id, &mut room)
            .and_then(|path| rustix::fs::stat(path).ok())
            .is_some_and(|st| rotational(st.st_rdev))
    })
}

/// The block device's `rotational` flag, a partition's from its disk. False for a device of no
/// block queue (tmpfs, network file systems), which keep the free reads.
pub(crate) fn rotational(dev: u64) -> bool {
    let (major, minor) = (rustix::fs::major(dev), rustix::fs::minor(dev));
    if major == 0 {
        return false;
    }
    let mut path = [0u8; PATH];
    for tail in ["queue/rotational", "../queue/rotational"] {
        let mut w = &mut path[..];
        if write!(w, "/sys/dev/block/{major}:{minor}/{tail}\0").is_err() {
            return false;
        }
        let len = PATH - w.len();
        let Ok(name) = CStr::from_bytes_with_nul(&path[..len]) else {
            return false;
        };
        if let Ok(fd) = rustix::fs::open(name, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()) {
            let mut flag = [0u8; 2];
            return rustix::io::read(&fd, &mut flag).is_ok_and(|n| n > 0 && flag[0] == b'1');
        }
    }
    false
}

#[cfg(test)]
// Test fixtures open files directly.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use std::os::fd::AsFd;

    use super::*;

    #[test]
    fn a_device_of_no_block_queue_is_not_rotational() {
        assert!(!rotational(0));
        assert!(!rotational(rustix::fs::makedev(0, 42)));
        assert!(!rotational(rustix::fs::makedev(4095, 1_048_575)), "no such device");
    }

    #[test]
    fn a_fixed_choice_holds_whatever_the_storage() {
        let f = std::fs::File::open("/proc/self/stat").expect("procfs");
        assert!(Reads::InOrder.in_order(f.as_fd(), 0));
        assert!(!Reads::Free.in_order(f.as_fd(), rustix::fs::makedev(8, 0)));
        assert!(!Reads::Auto.in_order(f.as_fd(), 0), "procfs is neither FUSE nor a disk");
    }

    #[test]
    fn a_file_not_on_btrfs_tells_no_btrfs_devices() {
        let f = std::fs::File::open("/proc/self/stat").expect("procfs");
        assert!(sys::btrfs_max_id(f.as_fd()).is_err());
        assert!(sys::btrfs_dev_path(f.as_fd(), 1, &mut sys::BtrfsDev::default()).is_none());
        assert!(!btrfs_rotational(f.as_fd()));
    }
}
