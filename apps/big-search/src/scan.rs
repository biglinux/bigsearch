//! Walk visible (non-hidden, non-gitignored) entries and (re)index them.
//! Shared by the full reindex and the incremental watcher.
use crate::index::Fields;
use crate::state::{self, State};
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

/// Built-in excludes (build artifacts, vendored deps, generated/lock files) applied
/// regardless of whether a dir is a git repo. Users extend/override these in
/// `~/.config/big-search/ignore` (gitignore syntax; `!pattern` re-includes).
const DEFAULT_IGNORE: &str = "\
# big-search default ignores — edit ~/.config/big-search/ignore to customize.
target/
**/target/**
node_modules/
**/node_modules/**
dist/
**/dist/**
build/
**/build/**
out/
**/out/**
vendor/
**/vendor/**
AppData/
ProgramData/
Program Files/
Program Files (x86)/
Windows/
$Recycle.Bin/
System Volume Information/
Recovery/
EFI/
__pycache__/
**/__pycache__/**
.venv/
**/.venv/**
venv/
**/venv/**
bower_components/
**/bower_components/**
.next/
**/.next/**
.nuxt/
**/.nuxt/**
.svelte-kit/
**/.svelte-kit/**
.gradle/
**/.gradle/**
.tox/
**/.tox/**
coverage/
**/coverage/**
*.lock
package-lock.json
yarn.lock
pnpm-lock.yaml
*.min.js
*.min.css
*.map
*.rs.html
";

const STATE_FLUSH_ROWS: usize = 2048;
const STALE_DELETE_BATCH: usize = 2048;

/// Documented per-user ignore file, seeded once at `~/.config/big-search/ignore`
/// so the customisation point is discoverable and self-explaining. Commented out
/// by default → no extra excludes until the user opts in.
const USER_IGNORE_TEMPLATE: &str = "\
# big-search — your personal ignore list (gitignore syntax).
#
# These patterns are added ON TOP of the built-in ignores (build artifacts like
# target/, node_modules/, *.lock and all hidden dot-files are already skipped).
# Re-read on the next reindex / daemon start. Edit freely — big-search never
# overwrites this file.
#
# Syntax (same as .gitignore):
#   foo/         skip every directory named foo, anywhere
#   /foo/        skip foo only at a scan-root top level
#   *.bak        skip files by glob
#   !keep.bak    re-include something a previous rule excluded
#
# IMPORTANT: excluding a directory also hides the files inside it from name
# search. Exclude only trees whose contents you never look for (caches,
# extracted container images, old build/working dirs) — not folders whose files
# you still want to find (your VMs, ROMs, ISOs and documents stay findable).
#
# Examples — uncomment / adapt:
# *.iso              # don't index ISO images
# github/            # skip cloned source repos
# Caches/
#
# Related settings live in config.toml, not here:
#   big-search config set metadata true        # extract media/image metadata
#   big-search config set follow-symlinks true # index top-level symlinked dirs
";

/// Seed [`USER_IGNORE_TEMPLATE`] at the user ignore path if it does not exist
/// yet. Never overwrites — preserves the user's edits.
fn seed_user_ignore() {
    let path = crate::config::user_ignore();
    if path.exists() {
        return;
    }
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = std::fs::write(&path, USER_IGNORE_TEMPLATE);
}

/// Write the effective ignore file: built-in defaults followed by the user's
/// overrides (so user lines win). Seeds the documented user ignore file on first
/// run. Call once before walking.
pub fn write_effective_ignore() -> Result<()> {
    seed_user_ignore();
    let path = crate::config::effective_ignore();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut content = String::from(DEFAULT_IGNORE);
    if let Ok(user) = std::fs::read_to_string(crate::config::user_ignore()) {
        content.push_str("\n# --- user overrides ---\n");
        content.push_str(&user);
    }
    std::fs::write(&path, content).context("write effective ignore")?;
    Ok(())
}

/// The scope rules, compiled once so they can be asked about a single path.
///
/// The walker gets these rules through `add_ignore`, but a walker can only
/// answer for entries it produces. Two callers need the same answer without
/// walking: the watcher, for each inotify event, and every subtree walk, for its
/// own root — the `ignore` crate never tests the root it is given, so a freshly
/// created `target/` was walked whole and every directory in it got a watch.
/// That is how 130 871 `target/` paths reached a catalogue whose rules exclude
/// them. One compiled matcher, shared, is what stops the walker and the watcher
/// from disagreeing again.
///
/// Per-repository `.gitignore` files are not compiled in: they are asked of
/// [`repo_ignored`] for paths new to the catalogue only, rather than re-read for
/// every event.
pub struct Scope {
    ignore: ignore::gitignore::Gitignore,
}

impl Scope {
    pub fn compile() -> Self {
        Self::from_rules(&crate::config::effective_ignore())
    }

    fn from_rules(rules: &Path) -> Self {
        // Root `/` so the patterns match anywhere, as `add_ignore` applies them.
        let mut builder = ignore::gitignore::GitignoreBuilder::new("/");
        if rules.exists()
            && let Some(e) = builder.add(rules)
        {
            log::warn!("ignore rules unusable, scope falls back to hidden-only: {e:#}");
        }
        Self {
            ignore: builder
                .build()
                .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty()),
        }
    }

    /// Whether `path` is outside the indexed scope. Checks the path's own
    /// ancestors too, so a file event inside an excluded tree is refused even
    /// when the event names only the file.
    pub fn excluded(&self, path: &Path, is_dir: bool) -> bool {
        is_hidden(path)
            || self
                .ignore
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore()
    }
}

/// Whether a `.gitignore` of the repository holding `path` excludes it, as the
/// walker would have decided had it come to `path` from the repository's root.
///
/// The walker never tests the root it is given, so a directory its repository
/// ignores, created again after the scan, was walked whole the moment it
/// appeared: `cargo fuzz` recreating `fuzz/corpus/` put 79 176 files into a
/// catalogue of 170 000. Asked only for paths the catalogue does not hold yet,
/// so an event on a known file costs nothing more.
pub fn repo_ignored(path: &Path, is_dir: bool) -> bool {
    // Nearest first: a deeper `.gitignore` overrides a shallower one.
    let mut rules = Vec::new();
    for dir in path.ancestors().skip(1) {
        let file = dir.join(".gitignore");
        if file.is_file() {
            rules.push(ignore::gitignore::Gitignore::new(&file).0);
        }
        // Outside a repository its ignore files do not apply (the walker's
        // `require_git`).
        if dir.join(".git").exists() {
            return rules
                .iter()
                .map(|rules| rules.matched_path_or_any_parents(path, is_dir))
                .find(|matched| !matched.is_none())
                .is_some_and(|matched| matched.is_ignore());
        }
    }
    false
}

/// A dotfile or any path under a dot-directory — the churny 90 % the design
/// excludes. Mirrors the walker's `hidden(true)`.
pub fn is_hidden(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(c, std::path::Component::Normal(s) if s.to_str().is_some_and(|x| x.starts_with('.')))
    })
}

/// `ignore` walker: skip hidden + every gitignore source + our effective ignore.
/// Symlinks are never followed during the walk (`follow_links(false)`) — symlinked
/// directories the user deliberately placed at a scan root are instead resolved
/// into separate roots by [`crate::scan_roots::resolve_roots`], which avoids chasing the thousands
/// of deep symlinks a tree contains (and the loops/duplication that brings).
fn walker(root: &Path) -> ignore::Walk {
    let mut b = WalkBuilder::new(root);
    b.hidden(true) // skip dotfiles/dirs — the churny, non-indexed 90%
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .follow_links(false);
    let eff = crate::config::effective_ignore();
    if eff.exists() {
        b.add_ignore(&eff); // highest precedence, prunes the noise dirs everywhere
    }
    b.build()
}

/// Index one entry by name only (no content extraction) — the fast scan path.
pub fn add_name_only(
    writer: &IndexWriter,
    f: &Fields,
    path: &Path,
    metadata: Option<&std::fs::Metadata>,
) -> Result<()> {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        let mut document = TantivyDocument::default();
        document.add_text(f.path, path.to_string_lossy());
        document.add_text(f.name, name);
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

/// Drop any document for `path` (the `path` field is a single raw token).
pub fn delete_path(writer: &IndexWriter, f: &Fields, path: &Path) {
    writer.delete_term(path_term(f, path));
}

/// Replace the document for `path` by name only. Content/metadata extraction is
/// intentionally deferred to the throttled background backfill; doing it here
/// would make inotify bursts read large files synchronously.
pub fn upsert_path(
    writer: &IndexWriter,
    f: &Fields,
    path: &Path,
    metadata: Option<&std::fs::Metadata>,
) -> Result<()> {
    delete_path(writer, f, path);
    add_name_only(writer, f, path, metadata)
}

pub fn path_term(f: &Fields, path: &Path) -> Term {
    Term::from_field_text(f.path, &path.to_string_lossy())
}

/// What a cache-aware reconcile actually touched. A name document only holds
/// `path`/`name`/`source`, so a metadata-only change (same path, new
/// mtime/size) updates the state cache and schedules content work without
/// rewriting the name index at all — `index_changed` demands a Tantivy commit,
/// `state_changed` a state persist + content backfill.
#[derive(Clone, Copy, Default)]
pub struct ReconcileOutcome {
    pub index_changed: bool,
    pub state_changed: bool,
    /// A catalogue row was dropped. Only a deletion can strand a content
    /// document, so this is what decides whether the orphan prune has anything
    /// to look for.
    pub deleted: bool,
}

impl ReconcileOutcome {
    pub fn any(self) -> bool {
        self.index_changed || self.state_changed
    }

    pub fn absorb(&mut self, other: ReconcileOutcome) {
        self.index_changed |= other.index_changed;
        self.state_changed |= other.state_changed;
        self.deleted |= other.deleted;
    }
}

pub struct SyncOutcome {
    pub content_candidates: usize,
    pub visible_dirs: Vec<PathBuf>,
}

/// Reconcile current subtree entries without blindly appending duplicate docs.
/// Used when a directory appears or changes after the initial scan.
pub fn reconcile_subtree_inline(
    writer: &IndexWriter,
    f: &Fields,
    state: &mut State,
    scope: &Scope,
    root: &Path,
) -> Result<(Vec<PathBuf>, ReconcileOutcome)> {
    let mut dirs = Vec::new();
    let mut outcome = ReconcileOutcome::default();
    // The walker exempts its own root from the ignore rules, so an excluded
    // directory would otherwise be indexed whole the moment it is created.
    if scope.excluded(root, true) || (!state.contains(root) && repo_ignored(root, true)) {
        return Ok((dirs, outcome));
    }
    for entry in walker(root).flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let Ok(md) = entry.metadata() else { continue };
        let (mtime, size) = state::meta_pair(&md);
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            dirs.push(path.to_path_buf());
        }
        if state.unchanged(path, mtime, size) {
            continue;
        }
        if !state.contains(path) {
            upsert_path(writer, f, path, Some(&md))?;
            outcome.index_changed = true;
        }
        state.set(path, mtime, size);
        outcome.state_changed = true;
    }
    Ok((dirs, outcome))
}

/// Every visible directory in `root` including `root` itself (watch targets).
pub fn visible_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for entry in walker(root).flatten() {
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
pub fn sync(index: &Index, roots: &[PathBuf], state: &mut State) -> Result<SyncOutcome> {
    let f = crate::index::fields(index)?;
    let mut writer = crate::index::bulk_writer(index)?;
    let mut content_candidates = 0usize;
    let mut visible_dirs = Vec::new();
    state.begin_scan()?;

    for root in roots {
        visible_dirs.push(root.clone());
        for entry in walker(root).flatten() {
            if entry.depth() == 0 {
                continue;
            }
            let path = entry.path();
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                visible_dirs.push(path.to_path_buf());
            }
            let Ok(md) = entry.metadata() else { continue };
            state.mark_seen(path)?;
            let (mtime, size) = state::meta_pair(&md);
            if state.unchanged(path, mtime, size) {
                continue; // doc (and body) already current
            }
            if !state.contains(path) {
                // New path → index its name. A known path with new mtime/size
                // keeps its identical name doc; only content work is scheduled.
                delete_path(&writer, &f, path);
                add_name_only(&writer, &f, path, Some(&md))?;
            }
            state.set(path, mtime, size);
            let (do_content, do_metadata) = crate::settings::policy_for(path);
            if (do_content && crate::extract::is_content_path(path))
                || (do_metadata && crate::meta::is_meta_candidate(path))
            {
                content_candidates = content_candidates.saturating_add(1);
            }
            persist_state_if_needed(state)?;
        }
    }

    // Offline removable/network sources: keep their catalogue rather than treat
    // the unmounted tree as deleted, so their files stay findable (and, later,
    // are reported as living on that device).
    state.mark_seen_prefixes(&retained_offline_roots())?;

    // Deletions: cached paths no longer on disk. Query in ordered batches so the
    // daemon never allocates a HashSet/Vec containing the whole catalogue.
    let mut after: Option<String> = None;
    loop {
        let stale = state.unseen_paths_after(after.as_deref(), STALE_DELETE_BATCH)?;
        if stale.is_empty() {
            break;
        }
        after = stale.last().map(|path| path.to_string_lossy().into_owned());
        for old in stale {
            delete_path(&writer, &f, &old);
            state.remove(&old);
            persist_state_if_needed(state)?;
        }
    }

    writer.commit().context("commit sync")?;
    state.persist()?;
    Ok(SyncOutcome {
        content_candidates,
        visible_dirs,
    })
}

/// Cache-aware full reconcile using a caller-provided writer. Used by the watcher
/// to recover from an inotify queue overflow (`need_rescan`) and by lifecycle
/// rebuild/resume. Does not commit the Tantivy writer.
pub fn reconcile_inline(
    writer: &IndexWriter,
    f: &Fields,
    state: &mut State,
    roots: &[PathBuf],
) -> Result<ReconcileOutcome> {
    let mut outcome = ReconcileOutcome::default();
    state.begin_scan()?;
    for root in roots {
        for entry in walker(root).flatten() {
            if entry.depth() == 0 {
                continue;
            }
            let path = entry.path();
            let Ok(md) = entry.metadata() else { continue };
            state.mark_seen(path)?;
            let (mtime, size) = state::meta_pair(&md);
            if state.unchanged(path, mtime, size) {
                continue;
            }
            if !state.contains(path) {
                upsert_path(writer, f, path, Some(&md))?;
                outcome.index_changed = true;
            }
            state.set(path, mtime, size);
            outcome.state_changed = true;
            persist_state_if_needed(state)?;
        }
    }
    // Keep offline removable/network sources catalogued (see `sync`).
    state.mark_seen_prefixes(&retained_offline_roots())?;
    let mut after: Option<String> = None;
    loop {
        let stale = state.unseen_paths_after(after.as_deref(), STALE_DELETE_BATCH)?;
        if stale.is_empty() {
            break;
        }
        after = stale.last().map(|path| path.to_string_lossy().into_owned());
        for old in stale {
            delete_path(writer, f, &old);
            state.remove(&old);
            outcome.index_changed = true;
            outcome.state_changed = true;
            outcome.deleted = true;
            persist_state_if_needed(state)?;
        }
    }
    Ok(outcome)
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

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::index::{fields, open_or_create};
    use crate::test_env::{ENV_LOCK, EnvVarGuard};

    #[test]
    fn subtree_indexes_visible_skips_hidden() {
        let base = std::env::temp_dir().join(format!("lsearch-scan-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
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
        let mut state = State::empty(base.join("state.json"));
        let (dirs, _) =
            reconcile_subtree_inline(&writer, &f, &mut state, &Scope::compile(), &tree).unwrap();
        writer.commit().unwrap();

        assert!(dirs.iter().any(|d| d.ends_with("sub")));
        assert_eq!(
            crate::query::search(
                &index.reader().unwrap(),
                "nested",
                &big_indexd_client::Filter::default(),
                10
            )
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            crate::query::search(
                &index.reader().unwrap(),
                "visible",
                &big_indexd_client::Filter::default(),
                10
            )
            .unwrap()
            .len(),
            1
        );
        assert!(
            crate::query::search_content(&content_index.reader().unwrap(), "bodyonlyneedle", 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            crate::query::search(
                &index.reader().unwrap(),
                "hidden",
                &big_indexd_client::Filter::default(),
                10
            )
            .unwrap()
            .is_empty()
        );

        // delete_path drops the document.
        delete_path(&writer, &f, &tree.join("sub/nested.md"));
        writer.commit().unwrap();
        assert!(
            crate::query::search(
                &index.reader().unwrap(),
                "nested",
                &big_indexd_client::Filter::default(),
                10
            )
            .unwrap()
            .is_empty()
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn reconcile_subtree_skips_unchanged_children() {
        let base =
            std::env::temp_dir().join(format!("lsearch-scan-reconcile-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let tree = base.join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("sub/nested.md"), b"x").unwrap();

        let index = open_or_create(&base.join("idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut state = State::empty(base.join("state.json"));
        let mut writer = index.writer(15_000_000).unwrap();

        let (_, outcome) =
            reconcile_subtree_inline(&writer, &f, &mut state, &Scope::compile(), &tree).unwrap();
        assert!(outcome.index_changed && outcome.state_changed);
        writer.commit().unwrap();
        drop(writer);
        state.persist().unwrap();
        assert_eq!(
            crate::index::document_count(&index.reader().unwrap()).unwrap(),
            2
        );

        let mut writer = index.writer(15_000_000).unwrap();
        let (_, outcome) =
            reconcile_subtree_inline(&writer, &f, &mut state, &Scope::compile(), &tree).unwrap();
        assert!(!outcome.any());
        writer.commit().unwrap();
        drop(writer);
        assert_eq!(
            crate::index::document_count(&index.reader().unwrap()).unwrap(),
            2
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_directory_its_repository_ignores_is_not_walked_when_it_appears() {
        let base =
            std::env::temp_dir().join(format!("lsearch-scan-repo-ignore-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
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
        let mut state = State::empty(base.join("state.json"));
        let mut writer = index.writer(15_000_000).unwrap();
        let (dirs, outcome) = reconcile_subtree_inline(
            &writer,
            &f,
            &mut state,
            &Scope::compile(),
            &repo.join("fuzz/corpus"),
        )
        .unwrap();
        assert!(dirs.is_empty() && !outcome.any());
        writer.commit().unwrap();
        assert_eq!(
            crate::index::document_count(&index.reader().unwrap()).unwrap(),
            0
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn metadata_only_change_skips_name_index_rewrite() {
        let base =
            std::env::temp_dir().join(format!("lsearch-scan-metaonly-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let tree = base.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let file = tree.join("nota.md");
        std::fs::write(&file, b"conteudo original").unwrap();

        let index = open_or_create(&base.join("idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut state = State::empty(base.join("state.json"));
        let writer = index.writer(15_000_000).unwrap();
        let (_, first) =
            reconcile_subtree_inline(&writer, &f, &mut state, &Scope::compile(), &tree).unwrap();
        assert!(first.index_changed);

        // Grow the file: same path/name, new mtime+size. The name doc is
        // unchanged, so only content/state work may be scheduled.
        std::fs::write(&file, b"conteudo original mais uma linha nova").unwrap();
        let (_, second) =
            reconcile_subtree_inline(&writer, &f, &mut state, &Scope::compile(), &tree).unwrap();
        assert!(
            !second.index_changed,
            "metadata-only change must not rewrite the name index"
        );
        assert!(second.state_changed, "content work must still be scheduled");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn hidden_detection() {
        assert!(is_hidden(Path::new("/home/u/.cache/x")));
        assert!(is_hidden(Path::new("/home/u/.config")));
        assert!(!is_hidden(Path::new("/home/u/docs/file.txt")));
        assert!(!is_hidden(Path::new("/home/u/Report.md")));
    }

    /// The leak this scope exists to close: the `ignore` walker exempts its own
    /// root, so a `target/` created after startup used to be walked whole and
    /// watched. The subtree walk must refuse the root itself.
    #[test]
    fn subtree_walk_refuses_an_excluded_root() {
        let base = std::env::temp_dir().join(format!("lsearch-scope-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let tree = base.join("proj");
        std::fs::create_dir_all(tree.join("target/debug")).unwrap();
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::write(tree.join("target/debug/build.o"), b"artifact").unwrap();
        std::fs::write(tree.join("src/main.rs"), b"code").unwrap();

        // The built-in rules, read from an explicit file: this test must not
        // touch XDG_*, which other tests share through a lock it cannot see.
        let rules = base.join("effective.ignore");
        std::fs::write(&rules, DEFAULT_IGNORE).unwrap();
        let scope = Scope::from_rules(&rules);

        assert!(scope.excluded(&tree.join("target"), true), "the dir itself");
        assert!(
            scope.excluded(&tree.join("target/debug/build.o"), false),
            "a file inside it, named alone by an inotify event"
        );
        assert!(!scope.excluded(&tree.join("src/main.rs"), false));

        let index = open_or_create(&base.join("idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut state = State::empty(base.join("state.json"));
        let writer = index.writer(15_000_000).unwrap();

        let (dirs, outcome) =
            reconcile_subtree_inline(&writer, &f, &mut state, &scope, &tree.join("target"))
                .unwrap();
        assert!(dirs.is_empty(), "no watch targets inside an excluded tree");
        assert!(!outcome.any(), "nothing indexed from an excluded tree");

        // The sibling that is in scope must still be walked normally.
        let (_, in_scope) =
            reconcile_subtree_inline(&writer, &f, &mut state, &scope, &tree.join("src")).unwrap();
        assert!(in_scope.index_changed);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn seed_user_ignore_creates_template_then_preserves_edits() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = std::env::temp_dir().join(format!("lsearch-ignore-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
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

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn default_ignore_prunes_nested_dependency_and_build_dirs() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base =
            std::env::temp_dir().join(format!("lsearch-default-ignore-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
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

        std::fs::remove_dir_all(&base).ok();
    }
}
