//! A start's catch-up: the saved or walked folders watched again, and what
//! changed while no watch covered them compared with the catalogue.

use super::*;

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
    use std::os::unix::fs::MetadataExt;
    let mut catch_up = CatchUp {
        replaced: Vec::new(),
        changed: Vec::new(),
        watched: HashSet::new(),
    };
    let mut watched = 0usize;
    let since = match &dirs {
        StartDirs::Walked(_) => epoch_secs(started) - 1,
        StartDirs::Saved(_, last_started) => epoch_secs(*last_started) - 1,
    };
    match dirs {
        StartDirs::Walked(dirs) => {
            let dirs = if dirs.is_empty() {
                roots
                    .iter()
                    .flat_map(|root| scan::visible_dirs(root))
                    .collect()
            } else {
                dirs
            };
            for dir in dirs {
                watched += usize::from(watches.watch(&dir));
                // Read after the watch: a change after it is an event.
                if std::fs::symlink_metadata(&dir).is_ok_and(|m| changed_since(&m, since)) {
                    catch_up.changed.push(dir.clone());
                }
                catch_up.watched.insert(dir);
            }
        }
        StartDirs::Saved(dirs, _) => {
            // A saved folder stays only while it is a directory, under a kept
            // folder or as a root: inotify follows a symlink, at the folder or
            // above it, and the walker never does. The list is in path order,
            // a parent before its folders, which leave with it.
            for (dir, saved_inode) in dirs {
                let in_kept = roots.contains(&dir)
                    || dir.parent().is_some_and(|p| catch_up.watched.contains(p));
                if !in_kept {
                    watches.dirs.remove(&dir);
                    continue;
                }
                // A root is never a catalogue entry, so it is never evicted: one
                // made anew is a changed folder, its old entries leave by its
                // pass, and its folders all have new inodes.
                let same_folder = match std::fs::symlink_metadata(&dir) {
                    Ok(m) => m.is_dir() && (roots.contains(&dir) || m.ino() == saved_inode),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        false
                    }
                    // Not readable now (EACCES, EIO on a disk going away): its
                    // entries stay, and a failed watch keeps this run from being
                    // recorded, so the next start walks.
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
                // Read again after the watch: a change after it is an event.
                if std::fs::symlink_metadata(&dir).is_ok_and(|m| changed_since(&m, since)) {
                    catch_up.changed.push(dir.clone());
                }
                catch_up.watched.insert(dir);
            }
        }
    }
    log::info!("watching {watched} visible directories");
    let unwatched = watches.failed.load(Ordering::Relaxed);
    if unwatched > 0 {
        log::warn!(
            "{unwatched} directories could not be watched; raise fs.inotify.max_user_watches"
        );
    }
    catch_up
}

/// Whether a folder's entries may have changed since `since` (seconds since
/// 1970). The change time counts too: `cp -a`, `rsync -a`, `tar` and a restore
/// set the modification time back, and nothing sets the change time back.
fn changed_since(metadata: &std::fs::Metadata, since: i64) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.mtime().max(metadata.ctime()) >= since
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
    let mut outcome = scan::ReconcileOutcome::default();
    let mut offline_sources = None;
    for dir in catch_up.replaced {
        if evict(
            active_index_writer(writer),
            f,
            state,
            watches,
            &dir,
            false,
            &mut offline_sources,
        ) {
            outcome.index_changed = true;
            outcome.state_changed = true;
            outcome.deleted = true;
        }
        outcome.absorb(reconcile(
            active_index_writer(writer),
            f,
            watches,
            state,
            path_scope,
            HashSet::from([dir]),
            None,
        )?);
    }
    for dir in catch_up.changed {
        let listing = match std::fs::read_dir(&dir) {
            Ok(listing) => listing,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(e) => {
                // Its changes would go unseen once this run is recorded.
                log::warn!("cannot list {}: {e}", dir.display());
                watches.missed = true;
                continue;
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
        entries.extend(state.children(&dir)?);
        outcome.absorb(reconcile(
            active_index_writer(writer),
            f,
            watches,
            state,
            path_scope,
            entries,
            Some(&catch_up.watched),
        )?);
    }
    if outcome.index_changed {
        active_index_writer(writer)
            .commit()
            .context("commit startup catch-up")?;
    }
    watches.persist()?;
    if outcome.any() {
        state.persist()?;
    }
    Ok(())
}
