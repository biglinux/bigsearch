//! Which mount holds a path, read from `/proc/self/mountinfo`.
//!
//! The kernel's own table instead of a `findmnt` child per question: no
//! process to spawn, nothing to hang when a network mount is gone, and the
//! answer is the mount namespace this service actually runs in.

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
    let table = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    innermost(&table, &path)
}

/// The deepest mount point above `path`. Among equal points the last wins:
/// that is the mount stacked on top, the one a lookup actually reaches.
fn innermost(table: &str, path: &Path) -> Option<Mount> {
    let mut best: Option<(usize, Mount)> = None;
    for line in table.lines() {
        let Some((point, mount)) = parse_line(line) else {
            continue;
        };
        if !path.starts_with(&point) {
            continue;
        }
        let depth = point.components().count();
        if best.as_ref().is_none_or(|(deepest, _)| depth >= *deepest) {
            best = Some((depth, mount));
        }
    }
    best.map(|(_, mount)| mount)
}

/// `id parent major:minor root mount-point options [optional…] - fstype source super-options`
fn parse_line(line: &str) -> Option<(PathBuf, Mount)> {
    let (left, right) = line.split_once(" - ")?;
    let point = left.split(' ').nth(4)?;
    let mut right = right.split(' ');
    let fstype = right.next()?;
    let source = right.next()?;
    Some((
        PathBuf::from(unescape_octal(point)),
        Mount {
            fstype: fstype.to_owned(),
            source: unescape_octal(source),
        },
    ))
}

/// The kernel writes space, tab, newline and backslash in mount fields as
/// `\ooo` octal escapes.
fn unescape_octal(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let code = rest.get(at + 1..at + 4);
        match code.and_then(|digits| u8::from_str_radix(digits, 8).ok()) {
            Some(byte) => {
                out.push(char::from(byte));
                rest = &rest[at + 4..];
            }
            None => {
                out.push('\\');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
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
        let at = |path: &str| innermost(TABLE, Path::new(path)).map(|mount| mount.fstype);
        assert_eq!(at("/etc/fstab").as_deref(), Some("btrfs"));
        assert_eq!(at("/proc/self").as_deref(), Some("proc"));
        assert_eq!(at("/home/p/My Share/doc.odt").as_deref(), Some("cifs"));
        assert_eq!(
            innermost(TABLE, Path::new("/home/p/notes.md")).map(|mount| mount.source),
            Some("/dev/nvme1n1p2".to_owned())
        );
        // Component-wise: `/proclaim` is not under `/proc`.
        assert_eq!(at("/proclaim").as_deref(), Some("btrfs"));
        assert_eq!(at("/home/p/stacked/x").as_deref(), Some("tmpfs"));
    }

    #[test]
    fn escapes_are_decoded_and_malformed_ones_kept() {
        assert_eq!(unescape_octal("a\\040b\\011c"), "a b\tc");
        assert_eq!(unescape_octal("trailing\\"), "trailing\\");
        assert_eq!(unescape_octal("bad\\9zz"), "bad\\9zz");
        assert_eq!(unescape_udev("My\\x20Disk"), "My Disk");
        assert_eq!(unescape_udev("x\\xZZ"), "x\\xZZ");
        assert_eq!(unescape_udev("end\\x2"), "end\\x2");
    }
}
