use super::*;

fn scratch(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bs-ipc-{label}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Clients do not authenticate the daemon: the socket and its directory
/// are this user's alone.
#[test]
fn socket_and_directory_private() {
    let base = scratch("mode");
    let sock = base.join("biglinux/indexd.sock");
    let _listener = bind_private_socket(&sock).unwrap();
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&sock), 0o600);
    assert_eq!(mode(&base.join("biglinux")), 0o700);
    std::fs::remove_dir_all(&base).ok();
}

/// A symlink planted where the runtime directory goes is refused, and the
/// directory it points at keeps its mode.
#[test]
fn symlinked_runtime_dir_refused() {
    let base = scratch("link");
    let target = base.join("theirs");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&target, base.join("biglinux")).unwrap();
    assert!(bind_private_socket(&base.join("biglinux/indexd.sock")).is_err());
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
    std::fs::remove_dir_all(&base).ok();
}
