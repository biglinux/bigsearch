//! Scratch directories for the tests.

use std::path::{Path, PathBuf};

/// An empty directory in `/tmp`, private to the test and removed with what it
/// holds when dropped. The scan refuses a path under a dot-directory, a git
/// work tree's ignored files or a build directory, and `TMPDIR` can be any of
/// those (`~/.cache/…`, a repository's `target/`).
pub struct ScratchDir(PathBuf);

impl std::ops::Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // A test that removed it already, or a file a daemon still writes,
        // leaves nothing to report from here.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn scratch_dir(name: &str) -> ScratchDir {
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
    ScratchDir(dir)
}

/// Whether the tests run as root, who reads a folder of any mode: a test of a
/// permission refusal has nothing to show then.
#[allow(
    dead_code,
    reason = "every test binary compiles this module; only some test permissions"
)]
pub fn runs_as_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|process| process.uid() == 0)
}
