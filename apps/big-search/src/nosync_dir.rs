//! Tantivy directory that never calls `fsync`.
//!
//! Both indexes are a cache derived from the filesystem, so the worst a forced
//! reboot can cost is re-indexing the files whose commits did not reach the
//! disk — work the startup reconcile already does. Paying `fsync` to avoid that
//! costs far more than the work it saves: tantivy issues two per commit (the
//! `meta.json` temporary at `MmapDirectory::atomic_write`, then the index
//! directory at `sync_directory`), and on btrfs each one is a log-tree
//! transaction. Measured on this daemon, ten minutes of near-idle churn: 145
//! `fsync` calls to write a few kilobytes, and 4.07 GB of logical writes landing
//! as 17.9 GB at the block layer.
//!
//! What we give up is stated plainly: after a power cut the index may be torn
//! rather than merely stale. That is survivable only because a torn index fails
//! to open and [`crate::index::open_or_create`] recreates it — the recovery path
//! is what makes this trade honest, not the absence of the syncs.
//!
//! All three of tantivy's sync sites are covered: the `meta.json` temporary and
//! the directory (95 % of the daemon's calls, since most commits write no new
//! segment) plus the per-segment sync in `SafeFileWriter`, which dominates
//! instead during a bulk reindex.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tantivy::Directory;
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    AntiCallToken, DirectoryLock, FileHandle, Lock, MmapDirectory, TerminatingWrite, WatchCallback,
    WatchHandle, WritePtr,
};

/// Tantivy's own `SafeFileWriter` minus the `sync_data` on terminate. The bytes
/// still reach the page cache; only the barrier is dropped.
struct UnsyncedFile(std::fs::File);

impl Write for UnsyncedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl TerminatingWrite for UnsyncedFile {
    fn terminate_ref(&mut self, _: AntiCallToken) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[derive(Clone, Debug)]
pub struct NoFsyncDirectory {
    inner: MmapDirectory,
    root: PathBuf,
}

impl NoFsyncDirectory {
    pub fn open(root: &Path) -> tantivy::Result<Self> {
        Ok(Self {
            inner: MmapDirectory::open(root)?,
            root: root.to_path_buf(),
        })
    }
}

impl Directory for NoFsyncDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.inner.get_file_handle(path)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        self.inner.delete(path)
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.inner.exists(path)
    }

    /// Same as `MmapDirectory`, except the finished segment file is not synced.
    /// Reimplemented rather than delegated because the sync lives inside the
    /// writer tantivy hands back, so there is no way to strip it afterwards.
    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.root.join(path))
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => {
                    OpenWriteError::FileAlreadyExists(path.to_path_buf())
                }
                _ => OpenWriteError::wrap_io_error(e, path.to_path_buf()),
            })?;
        Ok(std::io::BufWriter::new(Box::new(UnsyncedFile(file))))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.inner.atomic_read(path)
    }

    /// Write-then-rename, like `MmapDirectory`, minus the `fsync` of the
    /// temporary. The rename still makes the swap atomic against a *reader*; it
    /// is only a power cut that may leave the new content unwritten.
    ///
    /// The temporary is named after the target plus our pid rather than taken
    /// from `tempfile`: only the process holding the tantivy writer lock ever
    /// writes here, so that is unique enough, and the rename must stay inside
    /// the same directory to be atomic.
    fn atomic_write(&self, path: &Path, data: &[u8]) -> std::io::Result<()> {
        let target = self.root.join(path);
        let mut temp_name = target
            .file_name()
            .unwrap_or(std::ffi::OsStr::new("atomic"))
            .to_os_string();
        temp_name.push(format!(".tmp{}", std::process::id()));
        let temp = target.with_file_name(temp_name);

        let mut file = std::fs::File::create(&temp)?;
        if let Err(e) = file.write_all(data).and_then(|()| file.flush()) {
            drop(file);
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
        drop(file);
        std::fs::rename(&temp, &target)
    }

    fn sync_directory(&self) -> std::io::Result<()> {
        Ok(())
    }

    /// Delegated on purpose: `MmapDirectory` locks with `flock`, and the daemon's
    /// single-writer guarantee depends on it. The trait's default is a different
    /// (file-existence) scheme, so inheriting it would silently weaken the lock.
    fn acquire_lock(&self, lock: &Lock) -> Result<DirectoryLock, LockError> {
        self.inner.acquire_lock(lock)
    }

    fn watch(&self, watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        self.inner.watch(watch_callback)
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("bs-nosync-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let directory = NoFsyncDirectory::open(&dir).unwrap();

        directory
            .atomic_write(Path::new("meta.json"), b"first")
            .unwrap();
        assert_eq!(
            directory.atomic_read(Path::new("meta.json")).unwrap(),
            b"first"
        );

        // Overwrite must replace, not append or fail on an existing target.
        directory
            .atomic_write(Path::new("meta.json"), b"second")
            .unwrap();
        assert_eq!(
            directory.atomic_read(Path::new("meta.json")).unwrap(),
            b"second"
        );

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Commit repeatedly through the real index stack. Run under `strace` this is
    /// also the fsync-count probe; as a test it guards the part that matters
    /// either way — dropping the syncs must not drop the data.
    #[test]
    fn commits_through_the_index_stay_searchable() {
        let dir = std::env::temp_dir().join(format!("bs-nosync-idx-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let index = crate::index::open_or_create(&dir).unwrap();
        let f = crate::index::fields(&index).unwrap();
        let mut writer = crate::index::background_writer(&index).unwrap();

        for n in 0..20 {
            crate::scan::add_name_only(&writer, &f, Path::new(&format!("/docs/nota-{n}.md")), None)
                .unwrap();
            writer.commit().unwrap();
        }
        drop(writer);

        assert_eq!(
            crate::index::document_count(&index.reader().unwrap()).unwrap(),
            20
        );
        assert_eq!(
            crate::query::search(
                &index.reader().unwrap(),
                "nota-7",
                &big_indexd_client::Filter::default(),
                10
            )
            .unwrap()
            .len(),
            1
        );

        // Reopening must still succeed: the integrity probe in `open_existing`
        // runs here, so a meta.json/segment mismatch would fail the test.
        drop(index);
        let reopened = crate::index::open_or_create(&dir).unwrap();
        assert_eq!(
            crate::index::document_count(&reopened.reader().unwrap()).unwrap(),
            20
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
