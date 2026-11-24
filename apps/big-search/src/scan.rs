//! Walk visible (non-hidden, non-gitignored) entries and (re)index them.
//! Shared by the full reindex and the incremental watcher.
use crate::catalogue::{self, ScanChange, State};
use crate::index::Fields;
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use std::io::ErrorKind::{NotFound, PermissionDenied};
use std::path::{Path, PathBuf};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

mod scope;
pub use scope::{
    Scope, is_hidden, repo_ignored, seed_user_ignore, walk_ignored, write_effective_ignore,
};

const STATE_FLUSH_ROWS: usize = 2048;
/// Paths read from each side per page when a pass compares the disk with the
/// catalogue: bounded memory on a catalogue of millions.
const SCAN_PAGE: usize = 2048;

/// `ignore` walker: skip hidden entries, every folder's ignore files and the
/// `scope` rules, which the walker applies after the folders' files so that
/// none of those brings back what the rules leave out.
/// Symlinks are never followed during the walk (`follow_links(false)`); symlinked
/// directories the user placed at a scan root are instead resolved
/// into separate roots by [`crate::scan_roots::resolve_roots_with_policy`], which avoids chasing the thousands
/// of deep symlinks a tree contains (and the loops/duplication that brings).
fn walker(root: &Path, scope: &Scope) -> ignore::Walk {
    let scope = scope.clone();
    let mut b = WalkBuilder::new(root);
    b.hidden(true) // skip dotfiles/dirs: churny and never indexed
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .follow_links(false)
        .filter_entry(move |entry| {
            !scope.excludes_entry(entry.path(), entry.file_type().is_some_and(|t| t.is_dir()))
        });
    b.build()
}

/// Index one entry by name only (no content extraction): the fast scan path.
pub(crate) fn add_name_only(
    writer: &IndexWriter,
    f: &Fields,
    path: &Path,
    metadata: Option<&std::fs::Metadata>,
) -> Result<()> {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        // One `stat` serves the day of the last change and the type.
        let stat;
        let metadata = match metadata {
            Some(metadata) => Some(metadata),
            None => {
                stat = std::fs::symlink_metadata(path).ok();
                stat.as_ref()
            }
        };
        let mut document = TantivyDocument::default();
        document.add_text(f.path, path.to_string_lossy());
        document.add_text(f.name, crate::index::name_text(name));
        document.add_text(f.stem, crate::index::stem_key(name));
        document.add_text(f.source, crate::settings::source_name(path));
        // The root holds everything: a term for it would only cost postings.
        for dir in path
            .ancestors()
            .skip(1)
            .filter(|dir| dir.parent().is_some())
        {
            document.add_text(f.dir, dir.to_string_lossy());
        }
        if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
            document.add_text(f.ext, ext.to_lowercase());
        }
        let modified = metadata.map_or(0, |m| catalogue::meta_pair(m).0);
        document.add_u64(f.mtime_day, mtime_day(modified));
        if let Some(mime) = crate::mime::of(path, metadata) {
            // The exact type first: it is the stored value a hit reads.
            document.add_text(f.mime, &mime);
            if let Some((major, _)) = mime.split_once('/') {
                document.add_text(f.mime, format!("{major}/*"));
            }
        }
        writer.add_document(document)?;
    }
    Ok(())
}

/// The day (since the epoch, UTC) a modification time falls on.
pub fn mtime_day(mtime_secs: i64) -> u64 {
    u64::try_from(mtime_secs.div_euclid(86_400)).unwrap_or(0)
}

/// Whether the name document of a path needs writing: the path is new, or it
/// changed on another day than the one its document records. A name document
/// holds the name, the folders, the type and the day of the last change, so a
/// file saved again the same day keeps it and costs the name index nothing.
pub fn name_doc_stale(recorded: Option<(i64, u64)>, mtime: i64) -> bool {
    recorded.is_none_or(|(old, _)| mtime_day(old) != mtime_day(mtime))
}

/// Drop any document for `path` (the `path` field is a single raw token).
pub fn delete_path(writer: &IndexWriter, f: &Fields, path: &Path) {
    writer.delete_term(path_term(f.path, path));
}

/// Drop the document for `path` and, when it was a folder, every document
/// under it: each carries a `dir` term for every folder above it, so one term
/// delete covers the tree without reading it.
pub fn delete_tree(writer: &IndexWriter, f: &Fields, path: &Path) {
    delete_path(writer, f, path);
    writer.delete_term(Term::from_field_text(f.dir, &path.to_string_lossy()));
}

/// Replace the document for `path` by name only. Content/metadata extraction is
/// deferred to the throttled background backfill; doing it here
/// would make inotify bursts read large files synchronously.
pub(crate) fn upsert_path(
    writer: &IndexWriter,
    f: &Fields,
    path: &Path,
    metadata: Option<&std::fs::Metadata>,
) -> Result<()> {
    delete_path(writer, f, path);
    add_name_only(writer, f, path, metadata)
}

/// The term a document is keyed by in either index: its path as text.
#[must_use]
pub fn path_term(field: tantivy::schema::Field, path: &Path) -> Term {
    Term::from_field_text(field, &path.to_string_lossy())
}

/// What a cache-aware reconcile actually touched. A metadata-only change (same
/// path, new mtime/size, same day) updates the state cache and schedules
/// content work without rewriting the name index at all: `index_changed`
/// demands a Tantivy commit, `state_changed` a state persist + content
/// backfill.
#[derive(Clone, Copy, Default)]
pub struct ReconcileOutcome {
    pub index_changed: bool,
    pub state_changed: bool,
    /// A catalogue row was dropped; only then can the orphan prune find work.
    pub deleted: bool,
    /// Changed files whose content or metadata may need extracting, for the log.
    pub content_candidates: usize,
    /// A folder the walk could not read for a reason other than permission:
    /// what changed in it went unseen.
    pub unread: bool,
}

impl ReconcileOutcome {
    /// A catalogued path left the index and the catalogue.
    pub fn removed(&mut self) {
        self.index_changed = true;
        self.state_changed = true;
        self.deleted = true;
    }

    pub fn any(self) -> bool {
        self.index_changed || self.state_changed
    }

    pub fn absorb(&mut self, other: ReconcileOutcome) {
        self.index_changed |= other.index_changed;
        self.state_changed |= other.state_changed;
        self.deleted |= other.deleted;
        self.unread |= other.unread;
        self.content_candidates = self
            .content_candidates
            .saturating_add(other.content_candidates);
    }
}

pub struct SyncOutcome {
    pub content_candidates: usize,
    pub visible_dirs: Vec<PathBuf>,
    /// See [`ReconcileOutcome::unread`].
    pub unread: bool,
}

/// Reconcile current subtree entries without blindly appending duplicate docs.
/// Used when a directory appears or changes after the initial scan.
///
/// `on_dir` gets each visible subdirectory as the walk reaches it, before its
/// entries are read, so a watch placed there misses nothing created later.
pub(crate) fn reconcile_subtree_inline(
    writer: &IndexWriter,
    f: &Fields,
    state: &mut State,
    scope: &Scope,
    root: &Path,
    on_dir: &mut dyn FnMut(&Path),
) -> Result<ReconcileOutcome> {
    let mut outcome = ReconcileOutcome::default();
    // The walker exempts its own root from the ignore rules, so an excluded
    // directory would otherwise be indexed whole the moment it is created.
    if scope.excluded(root, true) || (!state.contains(root) && walk_ignored(root, true)) {
        return Ok(outcome);
    }
    for entry in walker(root, scope) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                outcome.unread |= unread_folder(&error)
                    .is_some_and(|(_, kind)| !matches!(kind, NotFound | PermissionDenied));
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        // Catalogue keys are text: two names that differ only in invalid bytes
        // would share one row, and the lossy path opens nothing.
        if path.to_str().is_none() {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        let (mtime, size) = catalogue::meta_pair(&md);
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            on_dir(path);
        }
        let recorded = state.recorded(path);
        if recorded == Some((mtime, size)) {
            continue;
        }
        if name_doc_stale(recorded, mtime) {
            upsert_path(writer, f, path, Some(&md))?;
            outcome.index_changed = true;
        }
        state.set(path, mtime, size);
        outcome.state_changed = true;
    }
    Ok(outcome)
}

/// Every visible directory in `root` including `root` itself (watch targets).
pub fn visible_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for entry in walker(root, &Scope::compile()).flatten() {
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            dirs.push(entry.path().to_path_buf());
        }
    }
    dirs
}

/// Reconcile the index with the filesystem against the `state` cache: index names
/// for new/changed entries, drop vanished ones, and skip unchanged ones entirely.
/// Returns content/metadata candidate count plus visible directories found during
/// the same walk; the content pass reads the catalogue from SQLite in bounded
/// batches instead of receiving a huge Vec.
///
/// # Errors
///
/// Returns an error when the catalogue cannot be read or written, or the index cannot be
/// committed.
pub fn sync(index: &Index, roots: &[PathBuf], state: &mut State) -> Result<SyncOutcome> {
    let f = crate::index::fields(index)?;
    let mut writer = crate::index::bulk_writer(index)?;
    let mut visible_dirs = Vec::new();
    let outcome = reconcile_roots(&mut writer, &f, state, roots, None, &mut |dir| {
        visible_dirs.push(dir.to_path_buf())
    })?;
    state.persist()?;
    Ok(SyncOutcome {
        content_candidates: outcome.content_candidates,
        visible_dirs,
        unread: outcome.unread,
    })
}

/// Cache-aware full reconcile using a caller-provided writer. Used by the watcher
/// to recover from an inotify queue overflow (`need_rescan`) and by lifecycle
/// rebuild/resume. `on_dir` gets every directory walked: after an overflow or
/// a pause, folders created meanwhile have no watch yet.
pub(crate) fn reconcile_inline(
    writer: &mut IndexWriter,
    f: &Fields,
    state: &mut State,
    roots: &[PathBuf],
    on_dir: &mut dyn FnMut(&Path),
) -> Result<ReconcileOutcome> {
    reconcile_roots(writer, f, state, roots, None, on_dir)
}

/// The full pass limited to `folder`, a catalogued folder in scope: what left
/// it, or left the scope, goes with its rows, as a walk of the sources would
/// drop it. The sources nested in it are walked as their own roots, since the
/// rules anchored at them may let in what the folder's walk leaves out.
pub(crate) fn reconcile_folder(
    writer: &mut IndexWriter,
    f: &Fields,
    state: &mut State,
    folder: &Path,
    on_dir: &mut dyn FnMut(&Path),
) -> Result<ReconcileOutcome> {
    let mut roots = vec![folder.to_path_buf()];
    roots.extend(
        crate::settings::available_sources()
            .into_iter()
            .map(|source| source.path.clone())
            .filter(|source| source != folder && source.starts_with(folder)),
    );
    reconcile_roots(writer, f, state, &roots, Some(folder), on_dir)
}

/// One pass over `roots`: walk the disk recording each entry's `(mtime, size)`,
/// then compare that with the catalogue in path order (see
/// [`State::scan_changes_after`]) and update name documents and `state`. With
/// `within`, only the catalogue rows below that folder are compared.
/// `on_dir` gets every directory walked.
///
/// The index is committed inside the pass, before the catalogue: a catalogue
/// that ran ahead of the index would never report the difference again.
fn reconcile_roots(
    writer: &mut IndexWriter,
    f: &Fields,
    state: &mut State,
    roots: &[PathBuf],
    within: Option<&Path>,
    on_dir: &mut dyn FnMut(&Path),
) -> Result<ReconcileOutcome> {
    let mut outcome = ReconcileOutcome::default();
    let scope = Scope::compile();
    state.scan_pass(|state| {
        // The comparison reads the catalogue as persisted.
        state.persist()?;
        state.begin_scan()?;
        outcome.unread = mark_walked_entries(state, &scope, roots, on_dir)?;
        // Offline removable/network sources: keep their catalogue rather than treat
        // the unmounted tree as deleted, so their files stay findable (and, later,
        // are reported as living on that device).
        let mut offline = retained_offline_roots();
        offline.retain(|root| within.is_none_or(|folder| root.starts_with(folder)));
        state.mark_seen_prefixes(&offline)?;

        // The rows below a folder sort from `<folder>/` to the first one
        // outside it.
        let mut after = Some(within.map_or_else(String::new, |folder| {
            format!("{}/", folder.to_string_lossy().trim_end_matches('/'))
        }));
        'pages: while let Some(cursor) = after {
            let (changes, next) = state.scan_changes_after(&cursor, SCAN_PAGE)?;
            after = next;
            for change in changes {
                let (ScanChange::Seen { path, .. } | ScanChange::Gone(path)) = &change;
                if within.is_some_and(|folder| !path.starts_with(folder)) {
                    break 'pages;
                }
                apply_scan_change(writer, f, state, change, &mut outcome)?;
                persist_state_if_needed(state)?;
            }
        }
        if outcome.index_changed {
            writer.commit().context("commit reconcile")?;
        }
        Ok(())
    })?;
    Ok(outcome)
}

/// Walk `roots` and mark every entry seen with its `(mtime, size)`; whether a
/// folder could not be read for a reason other than permission.
///
/// What a folder the walk cannot read held stays marked seen: an unreadable
/// folder is no evidence that its files are gone.
fn mark_walked_entries(
    state: &mut State,
    scope: &Scope,
    roots: &[PathBuf],
    on_dir: &mut dyn FnMut(&Path),
) -> Result<bool> {
    let mut unread = false;
    for root in roots {
        on_dir(root);
        for entry in walker(root, scope) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if let Some((folder, kind)) = unread_folder(&error)
                        && kind != NotFound
                    {
                        state.mark_seen_prefixes(&[folder.to_path_buf()])?;
                        unread |= kind != PermissionDenied;
                    }
                    continue;
                }
            };
            if entry.depth() == 0 {
                continue;
            }
            let path = entry.path();
            // Not marked seen, so a row an older version kept under a
            // lossy key goes as Gone.
            if path.to_str().is_none() {
                continue;
            }
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                on_dir(path);
            }
            let md = match entry.metadata() {
                Ok(md) => md,
                Err(error) => {
                    let kind = error.io_error().map(std::io::Error::kind);
                    // Gone between the listing and the lookup: it goes as Gone.
                    if kind == Some(NotFound) {
                        continue;
                    }
                    // A failed lookup is no evidence the file is gone.
                    state.mark_seen(path, None)?;
                    unread |= kind != Some(PermissionDenied);
                    continue;
                }
            };
            state.mark_seen(path, Some(catalogue::meta_pair(&md)))?;
        }
    }
    Ok(unread)
}

/// The path a walk could not read and why; `None` for an error that is not a
/// failed read, such as an ignore file that does not parse.
fn unread_folder(error: &ignore::Error) -> Option<(&Path, std::io::ErrorKind)> {
    match error {
        ignore::Error::WithPath { path, err } => Some((path, err.io_error()?.kind())),
        ignore::Error::WithDepth { err, .. } => unread_folder(err),
        _ => None,
    }
}

/// Bring the name index and `state` in line with one difference the scan found.
fn apply_scan_change(
    writer: &IndexWriter,
    f: &Fields,
    state: &mut State,
    change: ScanChange,
    outcome: &mut ReconcileOutcome,
) -> Result<()> {
    match change {
        ScanChange::Seen {
            path,
            mtime,
            size,
            recorded,
        } => {
            if name_doc_stale(recorded, mtime) {
                // A new path, or one changed on another day: (re)index
                // its name. Otherwise only content work is scheduled.
                // The walk kept no metadata; only changed paths get here.
                let md = std::fs::symlink_metadata(&path).ok();
                upsert_path(writer, f, &path, md.as_ref())?;
                outcome.index_changed = true;
            }
            state.set(&path, mtime, size);
            outcome.state_changed = true;
            let (do_content, do_metadata) = crate::settings::policy_for(&path);
            if (do_content && crate::extract::is_content_path(&path))
                || (do_metadata && crate::meta::is_meta_candidate(&path))
            {
                outcome.content_candidates = outcome.content_candidates.saturating_add(1);
            }
        }
        ScanChange::Gone(path) => {
            delete_path(writer, f, &path);
            state.remove(&path);
            outcome.removed();
        }
    }
    Ok(())
}

fn retained_offline_roots() -> Vec<PathBuf> {
    crate::settings::sources()
        .all()
        .iter()
        .filter(|s| s.retained_when_unavailable() && !crate::settings::is_available(s))
        .map(|s| s.path.clone())
        .collect()
}

fn persist_state_if_needed(state: &mut State) -> Result<()> {
    if state.pending_len() >= STATE_FLUSH_ROWS {
        state.persist()?;
    }
    Ok(())
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::index::{fields, open_or_create};
    use crate::test_env::{ENV_LOCK, EnvVarGuard, scratch_dir};

    fn name_hits(index: &tantivy::Index, query: &str) -> usize {
        let filter = big_indexd_client::Filter::default();
        crate::query::search(&index.reader().unwrap(), query, &filter, 10)
            .unwrap()
            .len()
    }

    #[test]
    fn subtree_indexes_visible_skips_hidden() {
        let base = scratch_dir("lsearch-scan");
        let tree = base.join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("visible_file.txt"), b"bodyonlyneedle").unwrap();
        std::fs::write(tree.join(".hidden_file"), b"x").unwrap();
        std::fs::write(tree.join("sub/nested.md"), b"x").unwrap();

        let index = open_or_create(&base.join("idx")).unwrap();
        let content_index =
            crate::index::open_content_or_create(&base.join("content-idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut writer = index.writer(15_000_000).unwrap();
        // The production walk, not a parallel one: `reconcile_subtree_inline` is
        // what the watcher calls when a directory appears.
        let mut state = State::empty(&base.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
        let mut dirs = Vec::new();
        let _ = reconcile_subtree_inline(
            &writer,
            &f,
            &mut state,
            &Scope::compile(),
            &tree,
            &mut |dir| dirs.push(dir.to_path_buf()),
        )
        .unwrap();
        writer.commit().unwrap();

        assert!(dirs.iter().any(|d| d.ends_with("sub")));
        assert_eq!(name_hits(&index, "nested"), 1);
        assert_eq!(name_hits(&index, "visible"), 1);
        assert!(
            crate::query::search_content(&content_index.reader().unwrap(), "bodyonlyneedle", 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(name_hits(&index, "hidden"), 0);

        // delete_path drops the document.
        delete_path(&writer, &f, &tree.join("sub/nested.md"));
        writer.commit().unwrap();
        assert_eq!(name_hits(&index, "nested"), 0);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reconcile_subtree_skips_unchanged_children() {
        let base = scratch_dir("lsearch-scan-reconcile");
        let tree = base.join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("sub/nested.md"), b"x").unwrap();

        let index = open_or_create(&base.join("idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut state = State::empty(&base.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
        let mut writer = index.writer(15_000_000).unwrap();

        let outcome = reconcile_subtree_inline(
            &writer,
            &f,
            &mut state,
            &Scope::compile(),
            &tree,
            &mut |_| {},
        )
        .unwrap();
        assert!(outcome.index_changed && outcome.state_changed);
        writer.commit().unwrap();
        drop(writer);
        state.persist().unwrap();
        assert_eq!(crate::index::document_count(&index.reader().unwrap()), 2);

        let mut writer = index.writer(15_000_000).unwrap();
        let outcome = reconcile_subtree_inline(
            &writer,
            &f,
            &mut state,
            &Scope::compile(),
            &tree,
            &mut |_| {},
        )
        .unwrap();
        assert!(!outcome.any());
        writer.commit().unwrap();
        drop(writer);
        assert_eq!(crate::index::document_count(&index.reader().unwrap()), 2);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ignored_parent_and_worktree_exclude() {
        let base = scratch_dir("lsearch-scan-git-layouts");
        // A rule below an ignored folder brings nothing back (gitignore(5)).
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(repo.join(".git/info")).unwrap();
        std::fs::create_dir_all(repo.join("cache")).unwrap();
        std::fs::write(repo.join(".gitignore"), "cache/\n").unwrap();
        std::fs::write(repo.join("cache/.gitignore"), "!report.txt\n").unwrap();
        // A linked worktree reads the exclude file of the repository it shares.
        std::fs::write(repo.join(".git/info/exclude"), "*.secret\n").unwrap();
        std::fs::write(repo.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        let worktree = base.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let git_dir = repo.join(".git/worktrees/wt");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();

        let report = repo_ignored(&repo.join("cache/report.txt"), false);
        let secret = repo_ignored(&worktree.join("notes.secret"), false);
        let plain = repo_ignored(&worktree.join("notes.txt"), false);
        let _ = std::fs::remove_dir_all(&base);
        assert!(report, "a file in an ignored folder came back");
        assert!(secret, "the worktree skipped the shared exclude file");
        assert!(!plain);
    }

    #[test]
    fn repo_ignored_directory_is_skipped() {
        let base = scratch_dir("lsearch-scan-repo-ignore");
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("fuzz/corpus/archive_index")).unwrap();
        std::fs::create_dir_all(repo.join("fuzz/seeds")).unwrap();
        std::fs::write(repo.join("fuzz/.gitignore"), "corpus/\n!keep\n").unwrap();
        std::fs::write(repo.join("fuzz/corpus/archive_index/0a1b2c"), b"x").unwrap();
        std::fs::write(repo.join("fuzz/seeds/seed"), b"x").unwrap();
        // The same rules outside a repository do not apply.
        std::fs::create_dir_all(base.join("plain/corpus")).unwrap();
        std::fs::write(base.join("plain/.gitignore"), "corpus/\n").unwrap();

        assert!(repo_ignored(&repo.join("fuzz/corpus"), true));
        assert!(repo_ignored(
            &repo.join("fuzz/corpus/archive_index/0a1b2c"),
            false
        ));
        assert!(!repo_ignored(&repo.join("fuzz/seeds/seed"), false));
        assert!(!repo_ignored(&repo.join("fuzz/keep"), false));
        assert!(!repo_ignored(&base.join("plain/corpus"), true));

        let index = open_or_create(&base.join("idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut state = State::empty(&base.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
        let mut writer = index.writer(15_000_000).unwrap();
        let mut dirs = Vec::new();
        let outcome = reconcile_subtree_inline(
            &writer,
            &f,
            &mut state,
            &Scope::compile(),
            &repo.join("fuzz/corpus"),
            &mut |dir| dirs.push(dir.to_path_buf()),
        )
        .unwrap();
        assert!(dirs.is_empty() && !outcome.any());
        writer.commit().unwrap();
        assert_eq!(crate::index::document_count(&index.reader().unwrap()), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn metadata_change_skips_name_rewrite() {
        let base = scratch_dir("lsearch-scan-metaonly");
        let tree = base.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let file = tree.join("nota.md");
        std::fs::write(&file, b"conteudo original").unwrap();

        let index = open_or_create(&base.join("idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut state = State::empty(&base.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
        let writer = index.writer(15_000_000).unwrap();
        let first = reconcile_subtree_inline(
            &writer,
            &f,
            &mut state,
            &Scope::compile(),
            &tree,
            &mut |_| {},
        )
        .unwrap();
        assert!(first.index_changed);

        // Grow the file: same path/name, new mtime+size. The name doc is
        // unchanged, so only content/state work may be scheduled.
        std::fs::write(&file, b"conteudo original mais uma linha nova").unwrap();
        let second = reconcile_subtree_inline(
            &writer,
            &f,
            &mut state,
            &Scope::compile(),
            &tree,
            &mut |_| {},
        )
        .unwrap();
        assert!(
            !second.index_changed,
            "metadata-only change must not rewrite the name index"
        );
        assert!(second.state_changed, "content work must still be scheduled");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn hidden_detection() {
        assert!(is_hidden(Path::new("/home/u/.cache/x")));
        assert!(is_hidden(Path::new("/home/u/.config")));
        assert!(!is_hidden(Path::new("/home/u/docs/file.txt")));
        assert!(!is_hidden(Path::new("/home/u/Report.md")));
    }

    /// The `ignore` walker exempts its own root, so a `target/` created after
    /// startup would be walked whole and watched. The subtree walk must refuse
    /// the root itself.
    #[test]
    fn subtree_walk_refuses_an_excluded_root() {
        let base = scratch_dir("lsearch-scope");
        let tree = base.join("proj");
        std::fs::create_dir_all(tree.join("target/debug")).unwrap();
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::write(tree.join("target/debug/build.o"), b"artifact").unwrap();
        std::fs::write(tree.join("src/main.rs"), b"code").unwrap();

        // The built-in rules, read from an explicit file: this test must not
        // touch XDG_*, which other tests share through a lock it cannot see.
        let rules = base.join("effective.ignore");
        std::fs::write(&rules, scope::DEFAULT_IGNORE).unwrap();
        let scope = Scope::from_rules(&rules);

        assert!(scope.excluded(&tree.join("target"), true), "the dir itself");
        assert!(
            scope.excluded(&tree.join("target/debug/build.o"), false),
            "a file inside it, named alone by an inotify event"
        );
        assert!(!scope.excluded(&tree.join("src/main.rs"), false));

        let index = open_or_create(&base.join("idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut state = State::empty(&base.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
        let writer = index.writer(15_000_000).unwrap();

        let mut dirs = Vec::new();
        let outcome = reconcile_subtree_inline(
            &writer,
            &f,
            &mut state,
            &scope,
            &tree.join("target"),
            &mut |dir| dirs.push(dir.to_path_buf()),
        )
        .unwrap();
        assert!(dirs.is_empty(), "no watch targets inside an excluded tree");
        assert!(!outcome.any(), "nothing indexed from an excluded tree");

        // The sibling that is in scope must still be walked normally.
        let in_scope = reconcile_subtree_inline(
            &writer,
            &f,
            &mut state,
            &scope,
            &tree.join("src"),
            &mut |_| {},
        )
        .unwrap();
        assert!(in_scope.index_changed);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn user_ignore_seed_keeps_edits() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = crate::test_env::scratch_dir("lsearch-ignore");
        let _config = EnvVarGuard::set("XDG_CONFIG_HOME", base.to_str().unwrap());

        let path = crate::config::user_ignore();
        assert!(!path.exists());
        seed_user_ignore();
        assert!(path.exists(), "template seeded on first run");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("gitignore syntax"),
            "template is documented"
        );

        // A second run must not clobber the user's edits.
        std::fs::write(&path, "github/\n").unwrap();
        seed_user_ignore();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "github/\n");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn default_ignore_prunes_build_dirs() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = scratch_dir("lsearch-default-ignore");
        let home = base.join("home");
        let config = base.join("config");
        let xdg_storage_home = base.join("data");
        std::fs::create_dir_all(home.join("project/node_modules/pkg")).unwrap();
        std::fs::create_dir_all(home.join("project/app/build/generated")).unwrap();
        std::fs::create_dir_all(home.join("project/docs")).unwrap();
        std::fs::write(home.join("project/node_modules/pkg/index.js"), "ignored").unwrap();
        std::fs::write(home.join("project/app/build/generated/out.txt"), "ignored").unwrap();
        std::fs::write(home.join("project/docs/readme.md"), "visible").unwrap();

        let _env = [
            EnvVarGuard::set("XDG_CONFIG_HOME", config.to_str().unwrap()),
            EnvVarGuard::set("XDG_DATA_HOME", xdg_storage_home.to_str().unwrap()),
        ];
        write_effective_ignore().unwrap();
        let paths: Vec<String> = visible_dirs(&home)
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();

        assert!(paths.iter().any(|path| path.ends_with("/project/docs")));
        assert!(
            !paths.iter().any(|path| path.contains("node_modules")),
            "node_modules should be pruned: {paths:?}"
        );
        assert!(
            !paths.iter().any(|path| path.contains("/build/")),
            "build directories should be pruned: {paths:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
