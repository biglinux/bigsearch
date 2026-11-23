//! A start's catch-up: the saved or walked folders watched again, and what
//! changed while no watch covered them compared with the catalogue.

use super::{StartDirs, Watches, active_index_writer, epoch_secs, evict, reconcile};
use crate::catalogue::State;
use crate::index::Fields;
use crate::scan;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use tantivy::IndexWriter;

/// What a start's catch-up compares again.
pub(super) struct CatchUp {
    /// Saved folders another folder took the place of while the daemon was
    /// stopped (`mv proj.new proj`), or a file, a symlink or nothing did. Their
    /// catalogue rows describe the old one.
    replaced: Vec<PathBuf>,
    /// Folders whose entries changed since a watch last covered them.
    changed: Vec<PathBuf>,
    /// Every folder watched at start; each one that changed has its own pass.
    watched: HashSet<PathBuf>,
}

/// Watch the folders a start begins with and say what the catch-up compares:
/// a folder changed since the walk began, or since the last run started for
/// the saved list. The walk's folders are every visible folder under `roots`
/// when it found none.
pub(super) fn watch_initial_dirs(
    watches: &mut Watches,
    roots: &[PathBuf],
    dirs: StartDirs,
    started: SystemTime,
) -> CatchUp {
    let mut catch_up = CatchUp {
        replaced: Vec::new(),
        changed: Vec::new(),
        watched: HashSet::new(),
    };
    let watched = match dirs {
        StartDirs::Walked(dirs, unread) => {
            watches.missed |= unread;
            let since = epoch_secs(started) - 1;
            let dirs = if dirs.is_empty() {
                roots
                    .iter()
                    .flat_map(|root| scan::visible_dirs(root))
                    .collect()
            } else {
                dirs
            };
            let mut watched = 0usize;
            for dir in dirs {
                watched += usize::from(watches.watch(&dir));
                catch_up.note_watched(dir, since);
            }
            watched
        }
        StartDirs::Saved(dirs, last_started) => watch_saved(
            watches,
            roots,
            dirs,
            epoch_secs(last_started) - 1,
            &mut catch_up,
        ),
    };
    log::info!("watching {watched} visible directories");
    let unwatched = watches.failed.load(Ordering::Relaxed);
    if unwatched > 0 {
        log::warn!(
            "{unwatched} directories could not be watched; raise fs.inotify.max_user_watches"
        );
    }
    catch_up
}

/// Watch the saved folders that are still the folders saved, and count the
/// watches placed. A saved folder stays only while it is a directory, under a
/// kept folder or as a root: inotify follows a symlink, at the folder or above
/// it, and the walker never does. The list is in path order, a parent before
/// its folders, which leave with it.
fn watch_saved(
    watches: &mut Watches,
    roots: &[PathBuf],
    dirs: Vec<(PathBuf, u64)>,
    since: i64,
    catch_up: &mut CatchUp,
) -> usize {
    let mut watched = 0usize;
    for (dir, saved_inode) in dirs {
        let in_kept =
            roots.contains(&dir) || dir.parent().is_some_and(|p| catch_up.watched.contains(p));
        if !in_kept {
            watches.dirs.remove(&dir);
            continue;
        }
        // A root is never a catalogue entry, so it is never evicted: one made
        // anew is a changed folder, its old entries leave by its pass, and its
        // folders all have new inodes.
        let same_folder = match still_listed(&dir, saved_inode, roots.contains(&dir)) {
            Ok(same) => same,
            // Not readable now (EACCES, EIO on a disk going away): its entries
            // stay, and this run is not recorded, so the next start walks.
            Err(e) => {
                log::warn!("cannot read {}: {e}", dir.display());
                watches.missed = true;
                continue;
            }
        };
        if !same_folder {
            watches.dirs.remove(&dir);
            catch_up.replaced.push(dir);
            continue;
        }
        // Already listed: no lookup to list it again.
        watched += usize::from(watches.place(&dir));
        catch_up.note_watched(dir, since);
    }
    watched
}

impl CatchUp {
    /// `dir` is watched, and changed when its entries may have moved since
    /// `since`; read after the watch, so a change after it is an event.
    fn note_watched(&mut self, dir: PathBuf, since: i64) {
        if std::fs::symlink_metadata(&dir).is_ok_and(|m| changed_since(&m, since)) {
            self.changed.push(dir.clone());
        }
        self.watched.insert(dir);
    }
}

/// Whether `dir` is still the folder listed with `saved_inode`: a directory,
/// the same one unless it is a root. An error when it cannot be read now.
fn still_listed(dir: &Path, saved_inode: u64, is_root: bool) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => Ok(metadata.is_dir() && (is_root || metadata.ino() == saved_inode)),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Whether a folder's entries may have changed since `since` (seconds since
/// 1970). The change time counts too: `cp -a`, `rsync -a`, `tar` and a restore
/// set the modification time back, and nothing sets the change time back.
fn changed_since(metadata: &std::fs::Metadata, since: i64) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.mtime().max(metadata.ctime()) >= since
}

/// The entries of `dir` on disk; `None` when it is gone or this user cannot
/// read it. Any other failure marks the run as having missed a folder, whose
/// changes would go unseen once the run is recorded.
fn listed_entries(dir: &Path, watches: &mut Watches) -> Option<HashSet<PathBuf>> {
    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return None;
        }
        Err(e) => {
            log::warn!("cannot list {}: {e}", dir.display());
            watches.missed = true;
            return None;
        }
    };
    let mut entries = HashSet::new();
    for entry in listing {
        match entry {
            Ok(entry) => {
                entries.insert(entry.path());
            }
            Err(e) => {
                log::warn!("cannot list {}: {e}", dir.display());
                watches.missed = true;
            }
        }
    }
    Some(entries)
}

/// What changed in a folder while it had no watch: its entries on disk and in
/// the catalogue are compared again, one folder at a time. Only its direct
/// entries; a new folder among them is walked whole, a vanished one leaves
/// with its tree. A replaced folder leaves with its tree, and what took its
/// place is indexed: a folder walked whole. A file rewritten in place changes
/// no folder and waits for `big-search rescan`.
pub(super) fn catch_up_at_start(
    writer: &mut Option<IndexWriter>,
    f: &Fields,
    watches: &mut Watches,
    state: &mut State,
    path_scope: &scan::Scope,
    catch_up: CatchUp,
) -> Result<()> {
    let index_writer = active_index_writer(writer);
    let mut outcome = read_replaced_anew(
        index_writer,
        f,
        watches,
        state,
        path_scope,
        catch_up.replaced,
    )?;
    for dir in catch_up.changed {
        let Some(mut entries) = listed_entries(&dir, watches) else {
            continue;
        };
        entries.extend(state.children(&dir)?);
        outcome.absorb(reconcile(
            index_writer,
            f,
            watches,
            state,
            path_scope,
            entries,
            Some(&catch_up.watched),
        )?);
    }
    if outcome.index_changed {
        index_writer.commit().context("commit startup catch-up")?;
    }
    watches.persist()?;
    if outcome.any() {
        state.persist()?;
    }
    Ok(())
}

/// Each replaced folder leaves with its tree, then is read as new.
fn read_replaced_anew(
    index_writer: &IndexWriter,
    f: &Fields,
    watches: &mut Watches,
    state: &mut State,
    path_scope: &scan::Scope,
    replaced: Vec<PathBuf>,
) -> Result<scan::ReconcileOutcome> {
    let mut outcome = scan::ReconcileOutcome::default();
    let mut offline_sources = None;
    for dir in replaced {
        if evict(
            index_writer,
            f,
            state,
            watches,
            &dir,
            false,
            &mut offline_sources,
        ) {
            outcome.removed();
        }
        outcome.absorb(reconcile(
            index_writer,
            f,
            watches,
            state,
            path_scope,
            HashSet::from([dir]),
            None,
        )?);
    }
    Ok(outcome)
}
