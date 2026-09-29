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
//!
//! It also watches `meta.json` with inotify instead of letting `MmapDirectory`
//! poll it. Tantivy's watcher opens, reads and checksums the file every 500 ms
//! for as long as the index is open — one thread per index, so an idle daemon
//! woke four times a second to learn that nothing had changed.
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{RecursiveMode, Watcher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tantivy::Directory;
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    AntiCallToken, DirectoryLock, FileHandle, Lock, MmapDirectory, TerminatingWrite, WatchCallback,
    WatchCallbackList, WatchHandle, WritePtr,
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
    /// Started by the first reader that asks to be told about commits; `None`
    /// when inotify could not be set up, and tantivy's polling takes over.
    meta_watch: Arc<OnceLock<Option<MetaWatch>>>,
}

impl NoFsyncDirectory {
    pub fn open(root: &Path) -> tantivy::Result<Self> {
        Ok(Self {
            inner: MmapDirectory::open(root)?,
            root: root.to_path_buf(),
            meta_watch: Arc::default(),
        })
    }
}

/// The readers to reload, and the inotify watch that tells when to.
struct MetaWatch {
    callbacks: Arc<WatchCallbackList>,
    _watcher: notify::RecommendedWatcher,
}

impl std::fmt::Debug for MetaWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MetaWatch")
    }
}

impl MetaWatch {
    fn start(root: &Path) -> Option<Self> {
        let callbacks = Arc::new(WatchCallbackList::default());
        let fired = callbacks.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok_and(|event| is_meta_commit(&event)) {
                    // Same as tantivy's poller: wait for the reloads before the next event.
                    let _ = fired.broadcast().wait();
                }
            })
            .map_err(|e| log::warn!("index watch unavailable, polling instead: {e}"))
            .ok()?;
        watcher
            .watch(root, RecursiveMode::NonRecursive)
            .map_err(|e| log::warn!("index watch unavailable, polling instead: {e}"))
            .ok()?;
        Some(Self {
            callbacks,
            _watcher: watcher,
        })
    }
}

/// Whether `event` is a new `meta.json` landing — the rename that publishes a
/// commit. Opens and reads are left out on purpose: a reload reads the file, and
/// counting that as a change would reload forever. Of a rename only the arrival
/// counts: inotify reports it again as the `Both` pair, which would reload twice.
fn is_meta_commit(event: &notify::Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Modify(
                ModifyKind::Name(RenameMode::To) | ModifyKind::Data(_) | ModifyKind::Any
            )
    ) && event
        .paths
        .iter()
        .any(|path| path.file_name() == Some(std::ffi::OsStr::new("meta.json")))
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
        match self.meta_watch.get_or_init(|| MetaWatch::start(&self.root)) {
            Some(meta_watch) => {
                let handle = meta_watch.callbacks.subscribe(watch_callback);
                // A reader loads `meta.json` before it subscribes, and a commit in
                // between is announced to nobody — tantivy's poller hid that by
                // firing once at start and noticing changes only 500 ms later.
                // Reloading once now closes the gap; a reload with nothing new
                // opens nothing.
                let _ = meta_watch.callbacks.broadcast().wait();
                Ok(handle)
            }
            None => self.inner.watch(watch_callback),
        }
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

    /// A commit wakes the readers once, and reading `meta.json` — which is what a
    /// reload does — wakes nobody: an open counted as a change reloads forever.
    #[test]
    fn a_commit_wakes_the_watch_and_a_read_does_not() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = std::env::temp_dir().join(format!("bs-nosync-watch-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let directory = NoFsyncDirectory::open(&dir).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let _handle = directory
            .watch(WatchCallback::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            }))
            .unwrap();
        assert!(
            matches!(directory.meta_watch.get(), Some(Some(_))),
            "inotify watch did not start"
        );
        // Subscribing reloads once, synchronously, to cover a commit that landed
        // before the subscription.
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no reload on subscribe");

        directory
            .atomic_write(Path::new("meta.json"), b"commit")
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) == 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a commit must wake the watch exactly once"
        );

        for _ in 0..5 {
            directory.atomic_read(Path::new("meta.json")).unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a read woke the watch");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A reader opened before the commits sees them without being reopened —
    /// the daemon's query readers live for its whole life.
    #[test]
    fn a_long_lived_reader_follows_commits() {
        let dir = std::env::temp_dir().join(format!("bs-nosync-reload-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let index = crate::index::open_or_create(&dir).unwrap();
        let f = crate::index::fields(&index).unwrap();
        let reader = index.reader().unwrap();
        let mut writer = crate::index::background_writer(&index).unwrap();
        crate::scan::add_name_only(&writer, &f, Path::new("/docs/recarregado.md"), None).unwrap();
        writer.commit().unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while reader.searcher().num_docs() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(reader.searcher().num_docs(), 1);

        drop(writer);
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
