//! The reconcile of a batch of paths an event, a start's catch-up or a
//! forced reindex names: each is compared with the catalogue by existence and
//! scope, and a folder new to it is walked.

use super::Watches;
use crate::catalogue::{self, State};
use crate::index::Fields;
use crate::scan;
use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tantivy::IndexWriter;

/// Apply one coalesced batch. The outcome says whether the name index needs a
/// commit and whether state/content work happened. A changed catalogued
/// folder among `watched`, which has a pass of its own, only gets its row
/// refreshed; any other is walked whole.
pub(super) fn reconcile_batch(
    writer: &mut IndexWriter,
    f: &Fields,
    watches: &mut Watches,
    state: &mut State,
    scope: &scan::Scope,
    batch: HashSet<PathBuf>,
    watched: Option<&HashSet<PathBuf>>,
) -> Result<scan::ReconcileOutcome> {
    let mut pass = ReconcilePass {
        writer,
        f,
        scope,
        watched,
        forced: false,
        outcome: scan::ReconcileOutcome::default(),
        offline_sources: None,
    };
    for path in batch {
        pass.path(watches, state, &path)?;
    }
    // A folder the pass could not read leaves this run unable to vouch for it.
    watches.missed |= pass.outcome.unread;
    Ok(pass.outcome)
}

/// Reconcile the one path a forced reindex names, judged as new.
pub(super) fn reconcile_forced(
    writer: &mut IndexWriter,
    f: &Fields,
    watches: &mut Watches,
    state: &mut State,
    scope: &scan::Scope,
    path: &Path,
) -> Result<scan::ReconcileOutcome> {
    let mut pass = ReconcilePass {
        writer,
        f,
        scope,
        watched: None,
        forced: true,
        outcome: scan::ReconcileOutcome::default(),
        offline_sources: None,
    };
    pass.path(watches, state, path)?;
    watches.missed |= pass.outcome.unread;
    Ok(pass.outcome)
}

/// One batch's reconcile: what stays the same from path to path.
struct ReconcilePass<'a> {
    writer: &'a mut IndexWriter,
    f: &'a Fields,
    scope: &'a scan::Scope,
    watched: Option<&'a HashSet<PathBuf>>,
    /// Each path is judged as new, which is what a forced reindex asks; its
    /// record then only says that there is something to evict.
    forced: bool,
    outcome: scan::ReconcileOutcome,
    offline_sources: Option<crate::query::OfflineSources>,
}

impl ReconcilePass<'_> {
    fn path(&mut self, watches: &mut Watches, state: &mut State, path: &Path) -> Result<()> {
        // Catalogue keys are text: a name with invalid bytes shares its key
        // with others, and acting on it would touch theirs.
        if path.to_str().is_none() {
            return Ok(());
        }
        let stat = std::fs::symlink_metadata(path);
        let gone = matches!(&stat, Err(error) if error.kind() == std::io::ErrorKind::NotFound);
        // A lookup that failed otherwise leaves the path unread, and what the
        // catalogue holds for it stays.
        self.outcome.unread |= matches!(&stat, Err(error) if !matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
        ));
        let metadata = stat.ok();
        let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());
        let recorded = state.recorded(path);
        let judged = recorded.filter(|_| !self.forced);
        let out_of_scope = self.scope.excluded(path, is_dir)
            || (judged.is_none() && scan::walk_ignored(path, is_dir));
        if out_of_scope && recorded.is_none() {
            return Ok(()); // out of scope and never catalogued: nothing to do
        }
        // Below this line an excluded path is treated exactly like a vanished
        // one, so the rules also evict what an earlier version indexed before
        // they covered it. Skipping it instead would strand those documents.
        if let Some(metadata) = metadata.filter(|_| !out_of_scope) {
            return self.present(watches, state, path, &metadata, judged);
        }
        if recorded.is_some()
            && (gone || out_of_scope)
            && evict(
                self.writer,
                self.f,
                state,
                watches,
                path,
                gone,
                &mut self.offline_sources,
            )
        {
            self.outcome.removed();
        }
        // Otherwise never catalogued, or not readable now (EACCES, EIO on a
        // disk going away): nothing to drop on that evidence.
        Ok(())
    }

    /// A path that exists and is in scope: index what changed about it.
    fn present(
        &mut self,
        watches: &mut Watches,
        state: &mut State,
        path: &Path,
        metadata: &std::fs::Metadata,
        mut recorded: Option<(i64, u64)>,
    ) -> Result<()> {
        // A folder renamed over a listed one can match its time and size (a
        // small ext4 folder is one block): the inode tells them apart, and the
        // old folder's rows leave before the new one is read.
        if recorded.is_some() && watches.listed_as_other(path, metadata) {
            evict(
                self.writer,
                self.f,
                state,
                watches,
                path,
                false,
                &mut self.offline_sources,
            );
            self.outcome.removed();
            recorded = None;
        }
        let (mtime, size) = catalogue::meta_pair(metadata);
        // A listed folder this user could not read may have been made
        // readable (chmod, chown): only its change time moved.
        let reopened = metadata.is_dir() && watches.unreadable.contains(path);
        if recorded == Some((mtime, size)) && !reopened {
            return Ok(()); // metadata-only event (e.g. ATTRIB): nothing to reindex
        }
        if scan::name_doc_stale(recorded, mtime) {
            // A new path, or one changed on another day: (re)index its name.
            // A file rewritten the same day keeps its name doc (content work
            // is scheduled through the state change below).
            scan::upsert_path(self.writer, self.f, path, Some(metadata))?;
            self.outcome.index_changed = true;
        }
        state.set(path, mtime, size);
        self.outcome.state_changed = true;
        let own_pass =
            recorded.is_some() && self.watched.is_some_and(|watched| watched.contains(path));
        if metadata.is_dir() && !own_pass {
            let subtree_outcome = self.walk_folder(watches, state, path)?;
            self.outcome.absorb(subtree_outcome);
        }
        Ok(())
    }

    /// A new or moved-in folder: watched before it is read, and each subfolder
    /// as the walk reaches it, so an entry created during the walk is either
    /// read or reported. A forced one is judged whole again: what the walk no
    /// longer reaches leaves the folder list with its rows.
    fn walk_folder(
        &mut self,
        watches: &mut Watches,
        state: &mut State,
        path: &Path,
    ) -> Result<scan::ReconcileOutcome> {
        if self.forced {
            watches.dirs.remove_tree(path);
            return scan::reconcile_folder(self.writer, self.f, state, path, &mut |dir| {
                watches.watch(dir);
            });
        }
        watches.watch(path);
        scan::reconcile_subtree_inline(self.writer, self.f, state, self.scope, path, &mut |dir| {
            watches.watch(dir);
        })
    }
}

/// Drop a catalogued path that vanished or left the scope, with its tree.
/// `false` when it is kept: a vanished path on an unplugged source.
pub(super) fn evict(
    writer: &IndexWriter,
    f: &Fields,
    state: &mut State,
    watches: &mut Watches,
    path: &Path,
    gone: bool,
    offline_sources: &mut Option<crate::query::OfflineSources>,
) -> bool {
    // An unplugged removable or network source keeps its catalogue;
    // its files are reported as offline, not deleted.
    if gone
        && offline_sources
            .get_or_insert_with(crate::query::OfflineSources::default)
            .is_retained_offline(path)
    {
        return false;
    }
    // A folder moved away, to the trash included, is one event naming
    // the folder and none for what it held: its whole tree goes.
    scan::delete_tree(writer, f, path);
    state.remove_tree(path);
    watches.dirs.remove_tree(path);
    true
}
