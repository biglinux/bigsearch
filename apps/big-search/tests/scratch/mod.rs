//! Scratch directories for the integration tests.

use std::path::{Path, PathBuf};

/// An empty directory in `/tmp` for the home the binary indexes. The walk
/// skips dot-directories and honours the `.gitignore` of a repository above
/// it, and `TMPDIR` can be either (`~/.cache/…`, a repository's `target/`).
pub fn scratch_dir(name: &str) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let dir = Path::new("/tmp").join(format!(
        "{name}-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // Created here and only here: a path someone else planted fails the test
    // instead of receiving its files.
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .expect("create a fresh scratch directory");
    dir
}
