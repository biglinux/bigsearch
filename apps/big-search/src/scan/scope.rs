//! What lies outside the index: hidden paths, the built-in and personal ignore
//! rules every source shares, and the ignore files of each folder.

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};

/// Built-in excludes (build artifacts, vendored deps, generated/lock files) applied
/// regardless of whether a dir is a git repo. Users extend/override these in
/// `~/.config/big-search/ignore` (gitignore syntax; `!pattern` re-includes).
pub(super) const DEFAULT_IGNORE: &str = "\
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

/// Documented per-user ignore file, seeded once at `~/.config/big-search/ignore`
/// so the customization point is discoverable and self-explaining. Commented out
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

/// Seed `USER_IGNORE_TEMPLATE` at the user ignore path if it does not exist
/// yet. Never overwrites: the user's edits are kept. Called from the CLI only:
/// the service never writes under `$XDG_CONFIG_HOME`.
pub fn seed_user_ignore() {
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
/// overrides (so user lines win), and return what it wrote. Call once before
/// walking.
///
/// # Errors
///
/// Returns an error when the file or its folder cannot be written.
pub fn write_effective_ignore() -> Result<String> {
    let path = crate::config::effective_ignore();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut content = String::from(DEFAULT_IGNORE);
    if let Ok(user) = std::fs::read_to_string(crate::config::user_ignore()) {
        content.push_str("\n# --- user overrides ---\n");
        content.push_str(&user);
    }
    std::fs::write(&path, &content).context("write effective ignore")?;
    Ok(content)
}

/// The walker's scope rules, compiled once to answer for a single path.
///
/// The walker applies these rules to each entry it produces
/// ([`Self::excludes_entry`]), before the folders' own ignore files can bring
/// one back. Two callers need the same answer without walking: the watcher,
/// for each inotify event, and every subtree walk, for its own root. The
/// `ignore` crate never tests the root it is given, so without this a freshly
/// created `target/` would be walked whole and every directory in it watched.
/// One compiled matcher, shared, keeps the walker and the watcher in agreement.
///
/// The folders' own ignore files are not compiled in; [`walk_ignored`] checks
/// them for paths new to the catalogue only.
#[derive(Clone)]
pub struct Scope {
    ignore: std::sync::Arc<Gitignore>,
}

impl Scope {
    pub fn compile() -> Self {
        Self::from_rules(&crate::config::effective_ignore())
    }

    pub(super) fn from_rules(rules: &Path) -> Self {
        // Matched against paths below their source, as the walker applies them
        // from `rules_root`; `/` anchors a path no source holds.
        let mut builder = GitignoreBuilder::new("/");
        if rules.exists()
            && let Some(e) = builder.add(rules)
        {
            log::warn!("ignore rules unusable, scope falls back to hidden-only: {e:#}");
        }
        Self {
            ignore: std::sync::Arc::new(builder.build().unwrap_or_else(|_| Gitignore::empty())),
        }
    }

    /// Whether `path` is outside the indexed scope. Checks the path's own
    /// ancestors too, so a file event inside an excluded tree is refused even
    /// when the event names only the file.
    pub fn excluded(&self, path: &Path, is_dir: bool) -> bool {
        let below = below_source(path);
        is_hidden(below)
            || self
                .ignore
                .matched_path_or_any_parents(below, is_dir)
                .is_ignore()
    }

    /// Whether the rules leave out `path`, an entry a walk reached through
    /// folders they let in, so only its own name is matched. A source nested
    /// in the walked one anchors the rules for what lies below it.
    pub(super) fn excludes_entry(&self, path: &Path, is_dir: bool) -> bool {
        self.ignore.matched(below_source(path), is_dir).is_ignore()
    }
}

/// Whether git ignores `path` in the repository holding it, as the walker would
/// have decided had it come to `path` from the source holding it.
///
/// Git's precedence: the `.gitignore` nearest the path first, up to the root of
/// the work tree, then the repository's `info/exclude`, then the global
/// excludes file. The first with an opinion decides, `!` re-includes included;
/// nothing re-includes what lies in an ignored folder, which the walk never
/// enters.
pub fn repo_ignored(path: &Path, is_dir: bool) -> bool {
    ignore_files_exclude(path, is_dir, false)
}

/// Whether the ignore files in the folders above `path` leave it out of the
/// walk: the `.ignore` files, nearest first, in a repository or not, then
/// git's rules as [`repo_ignored`] reads them.
///
/// The walker never tests the root it is given, so a directory its repository
/// ignores, created again after the scan (`cargo fuzz` recreating
/// `fuzz/corpus/`), would be walked whole the moment it appears. Asked only for
/// paths the catalogue does not hold yet, so an event on a known file costs
/// nothing more.
pub fn walk_ignored(path: &Path, is_dir: bool) -> bool {
    ignore_files_exclude(path, is_dir, true)
}

fn ignore_files_exclude(path: &Path, is_dir: bool, with_dot_ignore: bool) -> bool {
    let files = IgnoreFilesAbove::read(path, with_dot_ignore);
    // The walk tests the folders below the source and never the source itself.
    let source = rules_root(path);
    path != source
        && (path
            .ancestors()
            .skip(1)
            .take_while(|folder| *folder != source)
            .any(|folder| files.ignore(folder, true))
            || files.ignore(path, is_dir))
}

/// The ignore files in the folders above a path, nearest first, each with the
/// folder it speaks for.
struct IgnoreFilesAbove<'a> {
    dot_ignores: Vec<(&'a Path, Gitignore)>,
    /// Empty outside a repository (the walker's `require_git`). They end with
    /// the repository's `info/exclude` and the global excludes, which speak
    /// for the whole work tree.
    gitignores: Vec<(&'a Path, Gitignore)>,
}

impl<'a> IgnoreFilesAbove<'a> {
    fn read(path: &'a Path, with_dot_ignore: bool) -> Self {
        let mut files = Self {
            dot_ignores: Vec::new(),
            gitignores: Vec::new(),
        };
        let mut in_repository = false;
        for dir in path.ancestors().skip(1) {
            if with_dot_ignore {
                let file = dir.join(".ignore");
                if file.is_file() {
                    files.dot_ignores.push((dir, Gitignore::new(&file).0));
                }
            }
            if in_repository {
                continue;
            }
            let file = dir.join(".gitignore");
            if file.is_file() {
                files.gitignores.push((dir, Gitignore::new(&file).0));
            }
            let Ok(dot_git) = std::fs::symlink_metadata(dir.join(".git")) else {
                continue;
            };
            let mut exclude = GitignoreBuilder::new(dir);
            exclude.add(git_common_dir(dir, dot_git.is_file()).join("info/exclude"));
            let exclude = exclude.build().unwrap_or_else(|_| Gitignore::empty());
            files.gitignores.push((dir, exclude));
            files
                .gitignores
                .push((dir, GitignoreBuilder::new(dir).build_global().0));
            in_repository = true;
            if !with_dot_ignore {
                break;
            }
        }
        if !in_repository {
            files.gitignores.clear();
        }
        files
    }

    /// Whether the first file with an opinion on `target`, the `.ignore` files
    /// before git's, ignores it.
    fn ignore(&self, target: &Path, is_dir: bool) -> bool {
        let first_opinion = |files: &[(&Path, Gitignore)]| {
            files
                .iter()
                .filter(|(owner, _)| target.ancestors().skip(1).any(|folder| folder == *owner))
                .map(|(_, rules)| rules.matched_path_or_any_parents(target, is_dir))
                .find(|matched| !matched.is_none())
                .map(|matched| matched.is_ignore())
        };
        first_opinion(&self.dot_ignores)
            .or_else(|| first_opinion(&self.gitignores))
            .unwrap_or(false)
    }
}

/// Where a work tree's repository keeps the files its worktrees share:
/// `.git` itself, or, when `.git` is a file (a linked worktree or a submodule),
/// the common folder of the git folder it names.
fn git_common_dir(work_tree: &Path, dot_git_is_file: bool) -> PathBuf {
    let dot_git = work_tree.join(".git");
    if !dot_git_is_file {
        return dot_git;
    }
    let Some(git_dir) = std::fs::read_to_string(&dot_git).ok().and_then(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("gitdir:"))
            .map(|named| work_tree.join(named.trim()))
    }) else {
        return dot_git;
    };
    // A submodule's git folder holds its own files and no `commondir`.
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(common) => git_dir.join(common.trim()),
        Err(_) => git_dir,
    }
}

/// Where the ignore rules are anchored for `path`: the source holding it, so a
/// source inside a folder the rules name (`/home/build`, `~/out/docs`) is still
/// indexed. `/` for a path no source holds.
fn rules_root(path: &Path) -> &'static Path {
    crate::settings::sources()
        .owner(path)
        .map_or(Path::new("/"), |source| &source.path)
}

/// `path` below the source that holds it. The walker tests only what lies
/// below its root, and a source may itself sit in a dot-folder.
fn below_source(path: &Path) -> &Path {
    path.strip_prefix(rules_root(path)).unwrap_or(path)
}

/// A dotfile or any path under a dot-directory: the churny majority the design
/// excludes. Mirrors the walker's `hidden(true)`.
pub fn is_hidden(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(c, std::path::Component::Normal(s) if s.as_encoded_bytes().first() == Some(&b'.'))
    })
}
