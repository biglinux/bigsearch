//! Which mount holds a path, read from `/proc/self/mountinfo`.
//!
//! The kernel's own table instead of a `findmnt` child per question: no
//! process to spawn, nothing to hang when a network mount is gone, and the
//! answer is the mount namespace this service actually runs in.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// The mount a path lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub fstype: String,
    /// The mount source: a device node for a disk, `server:/share` for NFS.
    pub source: String,
}

/// The mount holding `path`, or `None` when the table cannot be read.
pub fn mount_for(path: &Path) -> Option<Mount> {
    let path = std::fs::canonicalize(path).ok()?;
    let table = std::fs::read("/proc/self/mountinfo").ok()?;
    innermost(&table, &path)
}

/// The deepest mount point above `path`. Among equal points the last wins:
/// that is the mount stacked on top, the one a lookup actually reaches.
fn innermost(table: &[u8], path: &Path) -> Option<Mount> {
    innermost_entry(table, path).map(|(_, mount)| mount)
}

fn innermost_entry(table: &[u8], path: &Path) -> Option<(PathBuf, Mount)> {
    // The later of two mounts at one depth is the one on top.
    let mut best: Option<(usize, PathBuf, Mount)> = None;
    for line in table.split(|&byte| byte == b'\n') {
        let Some((point, mount)) = parse_line(line) else {
            continue;
        };
        if !path.starts_with(&point) {
            continue;
        }
        let depth = point.components().count();
        if best
            .as_ref()
            .is_none_or(|(deepest, _, _)| depth >= *deepest)
        {
            best = Some((depth, point, mount));
        }
    }
    best.map(|(_, point, mount)| (point, mount))
}

/// Whether the tree at `root` may change where this machine does not see it,
/// so its folder times prove nothing: it lives on, or holds, a disk that comes
/// and goes or another machine's folder. `true` when the mount table cannot be
/// read.
pub fn detachable(root: &Path) -> bool {
    let (Ok(root), Ok(table)) = (
        std::fs::canonicalize(root),
        std::fs::read("/proc/self/mountinfo"),
    ) else {
        return true;
    };
    detachable_in(&table, &root)
}

fn detachable_in(table: &[u8], root: &Path) -> bool {
    innermost_entry(table, root).is_some_and(|(point, mount)| detachable_mount(&point, &mount))
        || table
            .split(|&byte| byte == b'\n')
            .filter_map(parse_line)
            .any(|(point, mount)| point.starts_with(root) && detachable_mount(&point, &mount))
}

/// FAT and exFAT, written by other systems, keep no folder times worth
/// trusting nor stable inode numbers; the other paths are where the system
/// mounts disks that come and go.
fn detachable_mount(point: &Path, mount: &Mount) -> bool {
    matches!(mount.fstype.as_str(), "vfat" | "exfat" | "msdos")
        || crate::scan_roots::classify_fstype(&mount.fstype)
            == crate::scan_roots::MountKind::Network
        || ["/run/media", "/media", "/mnt/usb"]
            .iter()
            .any(|removable| point.starts_with(removable))
}

/// This mount namespace's table, held open to learn when it changes.
pub struct MountTable {
    file: std::fs::File,
}

impl MountTable {
    /// # Errors
    ///
    /// `/proc/self/mountinfo` cannot be opened.
    pub fn open() -> std::io::Result<Self> {
        Ok(Self {
            file: std::fs::File::open("/proc/self/mountinfo")?,
        })
    }

    /// The mounts now, as mount point and mount id, sorted.
    ///
    /// # Errors
    ///
    /// The table cannot be read.
    pub fn points(&mut self) -> std::io::Result<Vec<(PathBuf, u64)>> {
        use std::io::{Read, Seek};
        let mut table = Vec::new();
        self.file.rewind()?;
        self.file.read_to_end(&mut table)?;
        let mut mounts: Vec<(PathBuf, u64)> = table
            .split(|&byte| byte == b'\n')
            .filter_map(|line| {
                let id = std::str::from_utf8(line.split(|&byte| byte == b' ').next()?)
                    .ok()?
                    .parse()
                    .ok()?;
                Some((parse_line(line)?.0, id))
            })
            .collect();
        mounts.sort_unstable();
        Ok(mounts)
    }

    /// Block until something is mounted or unmounted: the kernel raises
    /// `POLLPRI` on the table, so the wait costs nothing.
    ///
    /// # Errors
    ///
    /// `poll` fails for a reason other than a signal.
    pub fn wait(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let mut table = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        loop {
            // SAFETY: one pollfd, valid for the call; `self.file` keeps its fd open.
            if unsafe { libc::poll(&mut table, 1, -1) } > 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

/// `id parent major:minor root mount-point options [optional…] - fstype source super-options`,
/// in bytes: a mount point need not be UTF-8.
fn parse_line(line: &[u8]) -> Option<(PathBuf, Mount)> {
    let separator = line.windows(3).position(|window| window == b" - ")?;
    let (left, right) = line.split_at(separator);
    let point = left.split(|&byte| byte == b' ').nth(4)?;
    let mut right = right.get(3..)?.split(|&byte| byte == b' ');
    let fstype = right.next()?;
    let source = right.next()?;
    Some((
        PathBuf::from(OsString::from_vec(unescape_octal(point))),
        Mount {
            fstype: String::from_utf8_lossy(fstype).into_owned(),
            source: String::from_utf8_lossy(&unescape_octal(source)).into_owned(),
        },
    ))
}

/// The kernel writes space, tab, newline and backslash in mount fields as
/// `\ooo` octal escapes.
fn unescape_octal(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut rest = field;
    while let Some((&first, tail)) = rest.split_first() {
        let decoded = (first == b'\\')
            .then(|| tail.get(..3))
            .flatten()
            .and_then(|digits| u8::from_str_radix(std::str::from_utf8(digits).ok()?, 8).ok());
        match decoded {
            Some(byte) => {
                out.push(byte);
                rest = &tail[3..];
            }
            None => {
                out.push(first);
                rest = tail;
            }
        }
    }
    out
}

/// Filesystem UUID and label of a block device, from the names udev gives it
/// under `/dev/disk/by-uuid` and `/dev/disk/by-label`. `None` for either when
/// the source is not a device node or udev has no such name for it.
pub fn uuid_label(source: &str) -> (Option<String>, Option<String>) {
    let Ok(device) = std::fs::canonicalize(source) else {
        return (None, None);
    };
    let name_in = |dir: &str| {
        std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
            (std::fs::canonicalize(entry.path()).ok()? == device)
                .then(|| unescape_udev(&entry.file_name().to_string_lossy()))
        })
    };
    (name_in("/dev/disk/by-uuid"), name_in("/dev/disk/by-label"))
}

/// udev writes unsafe bytes in link names as `\xNN` (a space is `\x20`).
fn unescape_udev(name: &str) -> String {
    let mut bytes = Vec::with_capacity(name.len());
    let mut rest = name.as_bytes();
    while let Some((&first, tail)) = rest.split_first() {
        let decoded = (first == b'\\' && tail.first() == Some(&b'x'))
            .then(|| tail.get(1..3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match decoded {
            Some(byte) => {
                bytes.push(byte);
                rest = &tail[3..];
            }
            None => {
                bytes.push(first);
                rest = tail;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    const TABLE: &str = "\
22 1 0:21 / / rw,noatime shared:1 - btrfs /dev/nvme1n1p2 rw,compress-force=zstd:5,subvol=/@
30 22 0:21 /@home /home rw,noatime shared:2 - btrfs /dev/nvme1n1p2 rw,subvol=/@home
31 22 0:5 / /proc rw,nosuid shared:3 - proc proc rw
40 30 0:40 / /home/p/My\\040Share rw - cifs //nas/share rw
41 30 0:41 / /home/p/stacked rw - ext4 /dev/sdb1 rw
42 41 0:42 / /home/p/stacked rw - tmpfs tmpfs rw
";

    #[test]
    fn the_deepest_and_topmost_mount_answers() {
        let at =
            |path: &str| innermost(TABLE.as_bytes(), Path::new(path)).map(|mount| mount.fstype);
        assert_eq!(at("/etc/fstab").as_deref(), Some("btrfs"));
        assert_eq!(at("/proc/self").as_deref(), Some("proc"));
        assert_eq!(at("/home/p/My Share/doc.odt").as_deref(), Some("cifs"));
        assert_eq!(
            innermost(TABLE.as_bytes(), Path::new("/home/p/notes.md")).map(|mount| mount.source),
            Some("/dev/nvme1n1p2".to_owned())
        );
        // Component-wise: `/proclaim` is not under `/proc`.
        assert_eq!(at("/proclaim").as_deref(), Some("btrfs"));
        assert_eq!(at("/home/p/stacked/x").as_deref(), Some("tmpfs"));
    }

    #[test]
    fn detachable_mounts_are_found() {
        let table = format!(
            "{TABLE}50 30 0:50 / /home/p/stick rw - vfat /dev/sdc1 rw\n\
             51 22 0:51 / /run/media/p/BACKUP rw - ext4 /dev/sdd1 rw\n"
        );
        let at = |root: &str| detachable_in(table.as_bytes(), Path::new(root));
        assert!(!at("/home/p/Documents"));
        assert!(at("/home/p"), "holds a network share and a FAT disk");
        assert!(at("/home/p/My Share"));
        assert!(at("/home/p/stick/photos"));
        assert!(at("/run/media/p/BACKUP"));
        assert!(!at("/etc"));
    }

    /// Guards: regression: a mount point that is not UTF-8 (a FAT label in
    /// Latin-1) is read, and every other line of the table with it.
    #[test]
    fn non_utf8_mount_point_reads() {
        let mut table = TABLE.as_bytes().to_vec();
        table.extend_from_slice(b"60 30 0:60 / /home/p/caf\xe9 rw - vfat /dev/sde1 rw\n");
        let point = Path::new(std::ffi::OsStr::from_bytes(b"/home/p/caf\xe9"));
        assert!(detachable_in(&table, point));
        assert!(!detachable_in(&table, Path::new("/home/p/Documents")));
        assert_eq!(
            innermost(&table, &point.join("photo.jpg")).map(|mount| mount.fstype),
            Some("vfat".to_owned())
        );
    }

    #[test]
    fn escapes_decode_and_malformed_stay() {
        assert_eq!(unescape_octal(b"a\\040b\\011c"), b"a b\tc");
        assert_eq!(unescape_octal(b"trailing\\"), b"trailing\\");
        assert_eq!(unescape_octal(b"bad\\9zz"), b"bad\\9zz");
        assert_eq!(unescape_udev("My\\x20Disk"), "My Disk");
        assert_eq!(unescape_udev("x\\xZZ"), "x\\xZZ");
        assert_eq!(unescape_udev("end\\x2"), "end\\x2");
    }
}
