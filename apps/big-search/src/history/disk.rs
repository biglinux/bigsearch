//! What the filesystem allows: sharing a document's blocks with `FICLONE`, and
//! how much free space must stay for that to remain harmless.
//!
//! [`clone_file`] is also how `content_dedupe` merges identical files.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use super::Unavailable;

/// Free disk below which versions stop and space is given back, whatever the
/// store costs: the machine must stay usable.
const LOW_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Free disk must also be this many times what the versions cost. A share of
/// the disk would stop a 2 TB disk with 200 GB free for a store of a few
/// megabytes; what matters is whether the versions are a real part of what is
/// left.
const FREE_TO_STORE_RATIO: u64 = 10;

/// Free space under which versions stop, for a store whose superseded
/// versions cost `store_bytes`.
pub(super) fn low_disk_threshold(store_bytes: u64) -> u64 {
    LOW_DISK_BYTES.max(store_bytes.saturating_mul(FREE_TO_STORE_RATIO))
}

/// Free space the eviction climbs back to, a quarter above the threshold, so
/// the thermostat does not sit on its own threshold switching on and off.
pub(super) fn recovered_disk_target(store_bytes: u64) -> u64 {
    let threshold = low_disk_threshold(store_bytes);
    threshold.saturating_add(threshold / 4)
}

/// Why a clone was refused, and what the refusal is about.
pub(crate) enum Refusal {
    /// Nothing on this device can be cloned.
    Device(Unavailable),
    /// This one file cannot.
    File,
    /// No space, or over quota.
    OutOfSpace,
}

/// Space on the filesystem holding `path`, from `statvfs(2)`.
pub(super) struct DiskCapacity {
    /// What an unprivileged process may still write (`f_bavail`).
    pub(super) available_bytes: u64,
    pub(super) total_bytes: u64,
}

pub(super) fn disk_capacity(path: &Path) -> io::Result<DiskCapacity> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `stats` is a correctly sized output buffer and `c_path` a valid
    // NUL-terminated string for the duration of the call.
    let stats = unsafe {
        let mut stats = std::mem::zeroed::<libc::statvfs>();
        if libc::statvfs(c_path.as_ptr(), &raw mut stats) != 0 {
            return Err(io::Error::last_os_error());
        }
        stats
    };
    // POSIX counts blocks in fragments; a filesystem reporting none uses f_bsize.
    let fragment = if stats.f_frsize > 0 {
        stats.f_frsize
    } else {
        stats.f_bsize
    };
    Ok(DiskCapacity {
        available_bytes: stats.f_bavail.saturating_mul(fragment),
        total_bytes: stats.f_blocks.saturating_mul(fragment),
    })
}

/// Clone `source` into `destination`, sharing its blocks.
///
/// The destination is created exclusively and read-only: a version somebody can
/// save over is not a version. Nothing here ever copies bytes: a refusal is a
/// refusal.
pub(crate) fn clone_file(source: &Path, destination: &Path) -> std::result::Result<(), Refusal> {
    let before = std::fs::symlink_metadata(source).map_err(|_| Refusal::File)?;
    // `O_NOFOLLOW`: a link is not a document, and following one would keep a
    // version of whatever it points at, filters and all.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)
        .map_err(|_| Refusal::File)?;
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| classify(&error))?;
    }
    let clone = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(destination)
        .map_err(|error| classify(&error))?;

    // SAFETY: FICLONE takes the source descriptor as its argument and writes
    // nothing into user memory. Both descriptors are open and owned here.
    let outcome = unsafe { libc::ioctl(clone.as_raw_fd(), libc::FICLONE, file.as_raw_fd()) };
    if outcome != 0 {
        let error = io::Error::last_os_error();
        drop(clone);
        let _ = std::fs::remove_file(destination);
        return Err(classify(&error));
    }
    drop(clone);

    // The file may have been saved again while this ran. `FICLONE` itself is
    // atomic against concurrent writes, but a version captured in the middle of
    // a sequence of saves is a version of a moment nobody meant to keep.
    let after = std::fs::symlink_metadata(source).map_err(|_| Refusal::File)?;
    if after.dev() != before.dev()
        || after.ino() != before.ino()
        || after.len() != before.len()
        || after.mtime_nsec() != before.mtime_nsec()
        || after.mtime() != before.mtime()
    {
        let _ = std::fs::remove_file(destination);
        return Err(Refusal::File);
    }
    Ok(())
}

/// What an error from the kernel is really saying.
fn classify(error: &io::Error) -> Refusal {
    match error.raw_os_error() {
        // The filesystem cannot share blocks at all.
        Some(libc::EOPNOTSUPP) | Some(libc::ENOTTY) => Refusal::Device(Unavailable::NoReflink),
        // Sharing across mount points, which is what this service always does,
        // is only allowed from Linux 5.18.
        Some(libc::EXDEV) => Refusal::Device(Unavailable::OldKernel),
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => Refusal::OutOfSpace,
        // `EINVAL` is what a file with copy-on-write turned off answers. The
        // disk is fine; this file is not.
        _ => Refusal::File,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refusal is filed against the right thing: the disk, the file, or the
    /// space left.
    ///
    /// Getting this wrong in either direction is expensive: blaming the disk for
    /// one file switches the feature off for everything, and blaming the file
    /// for the disk means retrying on every document forever.
    #[test]
    fn errno_maps_to_its_refusal() {
        let of = |code| classify(&io::Error::from_raw_os_error(code));
        assert!(matches!(
            of(libc::EOPNOTSUPP),
            Refusal::Device(Unavailable::NoReflink)
        ));
        assert!(matches!(
            of(libc::EXDEV),
            Refusal::Device(Unavailable::OldKernel)
        ));
        assert!(matches!(of(libc::ENOSPC), Refusal::OutOfSpace));
        assert!(matches!(of(libc::EDQUOT), Refusal::OutOfSpace));
        assert!(matches!(of(libc::EINVAL), Refusal::File));
        assert!(matches!(of(libc::EPERM), Refusal::File));
    }

    #[test]
    fn free_space_follows_the_store() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // A small store only needs the floor, however big the disk.
        assert_eq!(low_disk_threshold(800 * 1024 * 1024), 8000 * 1024 * 1024);
        assert_eq!(low_disk_threshold(0), 2 * GIB);
        assert_eq!(low_disk_threshold(10 * GIB), 100 * GIB);
        assert_eq!(recovered_disk_target(0), 2 * GIB + GIB / 2);
    }
}
