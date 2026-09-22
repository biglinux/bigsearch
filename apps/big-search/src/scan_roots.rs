//! Resolve the configured scan roots: expand top-level symlinks under a root
//! into real, disjoint local directories according to the symlink policy,
//! rejecting network/virtual mounts and operating-system trees.
use big_os_kit::subprocess::BigSubprocessSpec;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkPolicy {
    /// Follow only local symlink targets that look like user-owned data.
    PersonalLocal,
    /// Follow the exact local target. Network and virtual filesystems are still skipped.
    LocalExact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountKind {
    Local,
    Network,
    Virtual,
    Unknown,
}

/// Expand a base root set for symlink-aware scanning using the safe default
/// policy. Prefer [`resolve_roots_with_policy`] when a config value is available.
pub fn resolve_roots(base: &[PathBuf], follow: bool) -> Vec<PathBuf> {
    resolve_roots_with_policy(base, follow, SymlinkPolicy::PersonalLocal)
}

/// Expand a base root set for symlink-aware scanning. The walk itself never
/// follows symlinks, so deep links and loops are left alone. Only top-level links
/// under a configured root are evaluated, and the target policy decides which
/// local personal directories are added as real scan roots.
pub fn resolve_roots_with_policy(
    base: &[PathBuf],
    follow: bool,
    policy: SymlinkPolicy,
) -> Vec<PathBuf> {
    resolve_roots_with_mount_classifier(base, follow, policy, mount_kind_for_path)
}

fn resolve_roots_with_mount_classifier(
    base: &[PathBuf],
    follow: bool,
    policy: SymlinkPolicy,
    mount_kind_for: impl Fn(&Path) -> MountKind,
) -> Vec<PathBuf> {
    if !follow {
        return base.to_vec();
    }
    let mut targets: Vec<PathBuf> = Vec::new();
    for root in base {
        if let Ok(canon) = std::fs::canonicalize(root) {
            targets.push(canon);
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_symlink = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink());
            if !is_symlink {
                continue;
            }
            // canonicalize resolves the link (and any chain) to a real path. The
            // walk still uses follow_links(false), so only these top-level
            // user-visible links are evaluated.
            if let Ok(target) = std::fs::canonicalize(&path) {
                targets.extend(accepted_symlink_targets(&target, policy, &mount_kind_for));
            }
        }
    }
    prune_nested(targets)
}

fn accepted_symlink_targets(
    target: &Path,
    policy: SymlinkPolicy,
    mount_kind_for: &impl Fn(&Path) -> MountKind,
) -> Vec<PathBuf> {
    if !target.is_dir() || mount_kind_for(target) != MountKind::Local {
        return Vec::new();
    }
    match policy {
        SymlinkPolicy::LocalExact => vec![target.to_path_buf()],
        SymlinkPolicy::PersonalLocal => personal_local_targets(target),
    }
}

fn personal_local_targets(target: &Path) -> Vec<PathBuf> {
    if is_rejected_system_target(target) {
        return Vec::new();
    }
    let mut derived = windows_personal_targets(target);
    derived.extend(linux_home_targets(target));
    if !derived.is_empty() {
        return derived;
    }
    if looks_like_operating_system_root(target) {
        return Vec::new();
    }
    vec![target.to_path_buf()]
}

fn windows_personal_targets(target: &Path) -> Vec<PathBuf> {
    let users = target.join("Users");
    if !users.is_dir() {
        return Vec::new();
    }
    let mut roots = Vec::new();
    let Ok(entries) = std::fs::read_dir(users) else {
        return roots;
    };
    for entry in entries.flatten() {
        let user_dir = entry.path();
        if !user_dir.is_dir() || is_windows_system_user_dir(&user_dir) {
            continue;
        }
        for name in [
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Music",
            "Videos",
            "OneDrive",
        ] {
            let personal_dir = user_dir.join(name);
            if personal_dir.is_dir() {
                roots.push(personal_dir);
            }
        }
    }
    roots
}

fn linux_home_targets(target: &Path) -> Vec<PathBuf> {
    let home = target.join("home");
    if !home.is_dir() {
        return Vec::new();
    }
    let mut roots = Vec::new();
    let Ok(entries) = std::fs::read_dir(home) else {
        return roots;
    };
    for entry in entries.flatten() {
        let user_dir = entry.path();
        if user_dir.is_dir() && !is_hidden_path(&user_dir) {
            roots.push(user_dir);
        }
    }
    roots
}

fn is_windows_system_user_dir(path: &Path) -> bool {
    let Some(name) = lowercase_file_name(path) else {
        return true;
    };
    matches!(
        name.as_str(),
        "all users" | "default" | "default user" | "public"
    )
}

fn is_rejected_system_target(path: &Path) -> bool {
    let components = lowercase_components(path);
    if components
        .windows(2)
        .any(|pair| pair == ["users", "appdata"])
    {
        return true;
    }
    lowercase_file_name(path).is_some_and(|name| {
        matches!(
            name.as_str(),
            "windows"
                | "program files"
                | "program files (x86)"
                | "programdata"
                | "$recycle.bin"
                | "system volume information"
                | "recovery"
                | "efi"
                | "usr"
                | "etc"
                | "var"
                | "opt"
                | "boot"
                | "root"
                | "proc"
                | "sys"
                | "dev"
                | "run"
        )
    })
}

fn looks_like_operating_system_root(path: &Path) -> bool {
    (path.join("Windows").is_dir() && path.join("Program Files").is_dir())
        || (path.join("etc").is_dir() && path.join("usr").is_dir() && path.join("var").is_dir())
}

fn lowercase_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect()
}

fn lowercase_file_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
}

fn is_hidden_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn mount_kind_for_path(path: &Path) -> MountKind {
    let Ok(output) = BigSubprocessSpec::builder()
        .program("findmnt")
        .args(["-no", "FSTYPE", "--target"])
        .arg(path)
        .build()
        .run()
    else {
        return MountKind::Unknown;
    };
    if !output.status.success() {
        return MountKind::Unknown;
    }
    let fstype = String::from_utf8_lossy(&output.stdout);
    classify_fstype(fstype.trim())
}

fn classify_fstype(fstype: &str) -> MountKind {
    match fstype {
        "ext2" | "ext3" | "ext4" | "btrfs" | "xfs" | "f2fs" | "bcachefs" | "zfs" | "ntfs"
        | "ntfs3" | "exfat" | "vfat" | "fuseblk" | "hfsplus" | "apfs" => MountKind::Local,
        "nfs" | "nfs4" | "cifs" | "smb3" | "sshfs" | "fuse.sshfs" | "davfs" | "ceph" | "9p"
        | "fuse.rclone" | "fuse.gvfsd-fuse" => MountKind::Network,
        "proc" | "sysfs" | "devtmpfs" | "devpts" | "tmpfs" | "securityfs" | "cgroup"
        | "cgroup2" | "pstore" | "efivarfs" | "debugfs" | "tracefs" | "configfs" | "overlay"
        | "squashfs" | "autofs" | "mqueue" | "hugetlbfs" | "fuse.portal" => MountKind::Virtual,
        _ => MountKind::Unknown,
    }
}

/// Sort canonical paths and drop any that is equal to, or nested under, an
/// earlier (shorter) one — leaving a disjoint set so the walk scans each subtree
/// exactly once.
fn prune_nested(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort();
    roots.dedup();
    let mut kept: Vec<PathBuf> = Vec::new();
    for root in roots {
        // Sorting puts every ancestor before its descendants, so a root nested
        // under any already-kept root is dropped (component-wise `starts_with`).
        if kept.iter().any(|prev| root.starts_with(prev)) {
            continue;
        }
        kept.push(root);
    }
    kept
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn resolve_roots_expands_top_level_symlinks_and_prunes_nested() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("lsearch-roots-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let home = base.join("home");
        let ext = base.join("ext"); // a real external tree, linked from home
        std::fs::create_dir_all(home.join("realdir")).unwrap();
        std::fs::create_dir_all(ext.join("sub")).unwrap();
        symlink(&ext, home.join("extlink")).unwrap(); // top-level link -> ext
        symlink(ext.join("sub"), home.join("sublink")).unwrap(); // -> ext/sub (nested)

        let canon_home = std::fs::canonicalize(&home).unwrap();
        let canon_ext = std::fs::canonicalize(&ext).unwrap();

        // follow OFF: base returned unchanged.
        assert_eq!(
            resolve_roots(std::slice::from_ref(&home), false),
            vec![home.clone()]
        );

        // local-exact: home + ext; ext/sub dropped (nested under ext).
        let on = resolve_roots_with_mount_classifier(
            std::slice::from_ref(&home),
            true,
            SymlinkPolicy::LocalExact,
            |_| MountKind::Local,
        );
        assert!(on.contains(&canon_home), "home kept: {on:?}");
        assert!(on.contains(&canon_ext), "ext target added: {on:?}");
        assert!(
            !on.contains(&canon_ext.join("sub")),
            "nested pruned: {on:?}"
        );
        assert_eq!(on.len(), 2, "exactly home + ext: {on:?}");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn personal_symlink_policy_derives_windows_user_dirs_only() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("lsearch-winlink-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let home = base.join("home");
        let windows = base.join("WindowsDisk");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(windows.join("Windows")).unwrap();
        std::fs::create_dir_all(windows.join("Program Files")).unwrap();
        std::fs::create_dir_all(windows.join("Users/Bruno/Documents")).unwrap();
        std::fs::create_dir_all(windows.join("Users/Bruno/Downloads")).unwrap();
        std::fs::create_dir_all(windows.join("Users/Bruno/AppData")).unwrap();
        std::fs::create_dir_all(windows.join("Users/Public/Documents")).unwrap();
        symlink(&windows, home.join("windows")).unwrap();

        let roots = resolve_roots_with_mount_classifier(
            std::slice::from_ref(&home),
            true,
            SymlinkPolicy::PersonalLocal,
            |_| MountKind::Local,
        );
        let docs = std::fs::canonicalize(windows.join("Users/Bruno/Documents")).unwrap();
        let downloads = std::fs::canonicalize(windows.join("Users/Bruno/Downloads")).unwrap();
        let appdata = std::fs::canonicalize(windows.join("Users/Bruno/AppData")).unwrap();
        let public_docs = std::fs::canonicalize(windows.join("Users/Public/Documents")).unwrap();
        let win_root = std::fs::canonicalize(&windows).unwrap();

        assert!(roots.contains(&docs), "documents accepted: {roots:?}");
        assert!(roots.contains(&downloads), "downloads accepted: {roots:?}");
        assert!(!roots.contains(&appdata), "appdata rejected: {roots:?}");
        assert!(
            !roots.contains(&public_docs),
            "public user rejected: {roots:?}"
        );
        assert!(
            !roots.contains(&win_root),
            "windows root rejected: {roots:?}"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn personal_symlink_policy_derives_linux_home_dirs_only() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("lsearch-linuxlink-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let home = base.join("home");
        let old_root = base.join("OldLinux");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(old_root.join("etc")).unwrap();
        std::fs::create_dir_all(old_root.join("usr")).unwrap();
        std::fs::create_dir_all(old_root.join("var")).unwrap();
        std::fs::create_dir_all(old_root.join("home/bruno/Documents")).unwrap();
        symlink(&old_root, home.join("old-linux")).unwrap();

        let roots = resolve_roots_with_mount_classifier(
            std::slice::from_ref(&home),
            true,
            SymlinkPolicy::PersonalLocal,
            |_| MountKind::Local,
        );
        let user_home = std::fs::canonicalize(old_root.join("home/bruno")).unwrap();
        let old_root = std::fs::canonicalize(&old_root).unwrap();

        assert!(
            roots.contains(&user_home),
            "linux user home accepted: {roots:?}"
        );
        assert!(!roots.contains(&old_root), "linux root rejected: {roots:?}");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn personal_symlink_policy_rejects_network_targets() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("lsearch-netlink-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let home = base.join("home");
        let network = base.join("nas");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(network.join("Documents")).unwrap();
        symlink(&network, home.join("nas")).unwrap();

        let roots = resolve_roots_with_mount_classifier(
            std::slice::from_ref(&home),
            true,
            SymlinkPolicy::PersonalLocal,
            |path| {
                if path.starts_with(&network) {
                    MountKind::Network
                } else {
                    MountKind::Local
                }
            },
        );
        let network = std::fs::canonicalize(&network).unwrap();

        assert!(
            !roots.contains(&network),
            "network link rejected: {roots:?}"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn personal_symlink_policy_accepts_plain_local_data_dir() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("lsearch-datalink-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let home = base.join("home");
        let local_photos_target = base.join("DataDisk/Photos");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&local_photos_target).unwrap();
        symlink(&local_photos_target, home.join("photos")).unwrap();

        let roots = resolve_roots_with_mount_classifier(
            std::slice::from_ref(&home),
            true,
            SymlinkPolicy::PersonalLocal,
            |_| MountKind::Local,
        );
        let canonical_local_photos_target = std::fs::canonicalize(&local_photos_target).unwrap();

        assert!(
            roots.contains(&canonical_local_photos_target),
            "plain local data accepted: {roots:?}"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn filesystem_classification_rejects_network_and_virtual_mounts() {
        assert_eq!(classify_fstype("ext4"), MountKind::Local);
        assert_eq!(classify_fstype("ntfs3"), MountKind::Local);
        assert_eq!(classify_fstype("cifs"), MountKind::Network);
        assert_eq!(classify_fstype("nfs4"), MountKind::Network);
        assert_eq!(classify_fstype("proc"), MountKind::Virtual);
        assert_eq!(classify_fstype("madeupfs"), MountKind::Unknown);
    }
}
