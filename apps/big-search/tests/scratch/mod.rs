//! Scratch directories for the integration tests.

use std::path::PathBuf;

/// An empty directory in `/tmp` for the home the binary indexes. The walk
/// skips dot-directories and honours the `.gitignore` of a repository above
/// it, and `TMPDIR` can be either (`~/.cache/…`, a repository's `target/`).
pub fn scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    dir
}
