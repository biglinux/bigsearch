//! The folders watched and the record of the run that watched them: what
//! lets the next start trust the saved list instead of walking.

use super::*;

/// `meta` keys of the last run's record: the [`fingerprint`] its watches were
/// placed under and when it started. A start whose fingerprint matches watches
/// the saved list instead of walking the sources.
const RUN_FINGERPRINT: &str = "watch_fingerprint";
const RUN_STARTED: &str = "watch_started";

/// Fingerprint of the sources, the effective ignore rules and the global git
/// ignore file: when any differs from the last run's, the next start walks. A
/// toolchain whose hasher differs costs one walk.
pub fn fingerprint(roots: &[PathBuf], ignore_rules: &str, git_excludes: &[u8]) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    roots.hash(&mut hasher);
    ignore_rules.hash(&mut hasher);
    git_excludes.hash(&mut hasher);
    hasher.finish().cast_signed()
}

/// When the last run started, if its record in `watch_list` carries
/// `fingerprint`: every folder it listed was watched from then on.
pub fn saved_run_started(watch_list: &State, fingerprint: i64) -> Option<SystemTime> {
    if watch_list.meta(RUN_FINGERPRINT)? != fingerprint {
        return None;
    }
    let secs = u64::try_from(watch_list.meta(RUN_STARTED)?).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

/// Seconds since 1970 of `time`; 0, which makes every folder look changed,
/// for an earlier one.
pub(super) fn epoch_secs(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since_epoch| i64::try_from(since_epoch.as_secs()).ok())
        .unwrap_or(0)
}

/// Drop the last run's record, so the next start walks.
pub fn forget_saved_run(watch_list: &State) -> Result<()> {
    watch_list.set_meta(RUN_FINGERPRINT, None)
}

/// The watcher and how many watches it failed to place, shared with `status`.
pub(super) struct Watches {
    pub(super) watcher: RecommendedWatcher,
    pub(super) failed: Arc<AtomicU64>,
    /// Every folder watched, kept in the state database for the next start
    /// with its inode in the size column.
    pub(super) dirs: State,
    pub(super) fingerprint: Option<i64>,
    /// Whether the database may hold a run record.
    pub(super) recorded: bool,
    /// When the run this one recorded started, as written in the record.
    pub(super) run_started: Option<SystemTime>,
    /// A folder this run could not read or list where it had to, or could not
    /// save: what changes in it would go unseen by a start that trusts the run.
    pub(super) missed: bool,
    /// Listed folders this user could not read, so no watch is on them: one
    /// made readable is watched and walked when the catch-up or an event
    /// reaches it.
    pub(super) unreadable: HashSet<PathBuf>,
    /// Set by the IPC thread; a paused watcher drops events, so no run it
    /// records would be covered.
    pub(super) paused: Arc<AtomicBool>,
}

impl Watches {
    /// Watch a folder a walk or an event reached, and list it, also when this
    /// user cannot read it (see [`Self::unreadable`]).
    pub(super) fn watch(&mut self, dir: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let placed = self.place(dir);
        // The list keys paths as text: a name that is not UTF-8 would be saved
        // as another path, so a run holding one leaves the next start a walk.
        if dir.to_str().is_none() {
            self.missed = true;
            return placed;
        }
        if (placed || self.unreadable.contains(dir))
            && let Ok(metadata) = std::fs::symlink_metadata(dir)
            && self.dirs.recorded(dir) != Some((0, metadata.ino()))
        {
            self.dirs.set(dir, 0, metadata.ino());
        }
        placed
    }

    /// Place the watch on `dir`. An unreadable folder stays listed for the
    /// catch-up; any other failure but a vanished folder keeps this run from
    /// being recorded, and only the first is logged.
    pub(super) fn place(&mut self, dir: &Path) -> bool {
        let Err(error) = self.watcher.watch(dir, RecursiveMode::NonRecursive) else {
            self.unreadable.remove(dir);
            return true;
        };
        let unreadable = matches!(&error.kind,
            notify::ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied);
        if unreadable {
            self.unreadable.insert(dir.to_path_buf());
            return false;
        }
        if !matches!(error.kind, notify::ErrorKind::MaxFilesWatch) {
            self.dirs.remove(dir);
        }
        if !matches!(error.kind, notify::ErrorKind::PathNotFound)
            && self.failed.fetch_add(1, Ordering::Relaxed) == 0
        {
            log::warn!(
                "cannot watch {}: {error}; changes in it and in other unwatched \
                 folders are seen only by `big-search rescan`",
                dir.display()
            );
        }
        false
    }

    /// Save the folder list, before the catalogue at every persist site, two
    /// transactions a kill can come between. A folder listed but not
    /// catalogued is walked whole by the next start's catch-up; one catalogued
    /// but not listed would look known to it and stay unwatched. After a
    /// failed watch it also drops the run record.
    pub(super) fn persist(&mut self) -> Result<()> {
        self.dirs.persist()?;
        if self.failed.load(Ordering::Relaxed) > 0 || self.missed {
            self.forget_run()?;
        }
        // A clock set back since the run started: the record's start moves back
        // with it, or the next start's catch-up would miss what changes from
        // now until the clock reaches it again.
        let now = SystemTime::now();
        if self.recorded && self.run_started.is_some_and(|started| now < started) {
            self.dirs.set_meta(RUN_STARTED, Some(epoch_secs(now)))?;
            self.run_started = Some(now);
        }
        Ok(())
    }

    /// Record for the next start that every folder is watched since `started`,
    /// unless a watch failed in this run or indexing is paused.
    pub(super) fn record_run(&mut self, started: SystemTime) -> Result<()> {
        let Some(fingerprint) = self.fingerprint else {
            return Ok(());
        };
        if self.failed.load(Ordering::Relaxed) > 0
            || self.missed
            || self.paused.load(Ordering::Relaxed)
        {
            return Ok(());
        }
        self.dirs.set_meta(RUN_STARTED, Some(epoch_secs(started)))?;
        self.dirs.set_meta(RUN_FINGERPRINT, Some(fingerprint))?;
        self.recorded = true;
        self.run_started = Some(started);
        Ok(())
    }

    /// Before a walk: it places every watch and reads every folder again, so
    /// the failures counted so far are counted again by it.
    pub(super) fn walk_begins(&mut self) -> Result<()> {
        self.failed.store(0, Ordering::Relaxed);
        self.missed = false;
        self.forget_run()
    }

    /// Drop the run record, before a walk too: the walk commits the catalogue
    /// before the folder list is saved, and a start after a walk cut short
    /// between the two must walk.
    pub(super) fn forget_run(&mut self) -> Result<()> {
        if self.recorded {
            forget_saved_run(&self.dirs)?;
            self.recorded = false;
            self.run_started = None;
        }
        Ok(())
    }
}
