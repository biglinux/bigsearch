//! What lies outside the index: hidden paths, the built-in and personal ignore
//! rules every source shares, and the ignore files of each folder.

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

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

/// The scope rules, compiled once: what every pass asks about a path.
///
/// One decision serves every pass: the walk asks it about each entry, with the
/// ignore files of each folder read once on the way down, and the watcher and
/// a forced reindex ask it about each folder from below the source holding a
/// path down to the path, as the walk would have come to it. A source itself is
/// always in scope, and nothing above it is tested: the walk starts there.
///
/// An entry is out of scope when its name is hidden, when the effective rules
/// leave it out, matched below the source holding it, or when the first ignore
/// file with an opinion on it ignores it: the `.ignore` files of the folders
/// above it, nearest first, then git's (`.gitignore` up to the root of the
/// work tree, its `info/exclude`, the global excludes file). The effective
/// rules come first, so no folder's file brings back what they leave out.
#[derive(Clone)]
pub struct Scope {
    /// The effective rules, compiled at `.` and matched below the source.
    rules: Arc<Gitignore>,
}

impl Scope {
    pub fn compile() -> Self {
        Self::from_rules(&crate::config::effective_ignore())
    }

    pub(super) fn from_rules(rules: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(".");
        if rules.exists()
            && let Some(e) = builder.add(rules)
        {
            log::warn!("ignore rules unusable, scope falls back to hidden-only: {e:#}");
        }
        Self {
            rules: Arc::new(builder.build().unwrap_or_else(|_| Gitignore::empty())),
        }
    }

    /// Whether `path` is out of scope by its name and the folders' names
    /// alone: hidden or left out by the effective rules. Reads nothing from
    /// disk, so it answers for a gone path too.
    pub fn excluded(&self, path: &Path, is_dir: bool) -> bool {
        walk_down(
            path,
            is_dir,
            false,
            |files, source_len, entry, entry_is_dir| {
                self.leaves_out(files, source_len, entry, entry_is_dir)
            },
        )
    }

    /// Whether the walk from the source holding `path` leaves it out: what
    /// [`Self::excluded`] says, or an ignore file in a folder above it. Reads
    /// those files, so the watcher asks it only about paths new to the
    /// catalogue.
    pub fn walk_skips(&self, path: &Path, is_dir: bool) -> bool {
        walk_down(
            path,
            is_dir,
            true,
            |files, source_len, entry, entry_is_dir| {
                self.leaves_out(files, source_len, entry, entry_is_dir)
            },
        )
    }

    /// The decision as one walk from `root` takes it, entry by entry.
    pub(super) fn walk_filter(&self, root: &Path) -> WalkFilter {
        let nested_sources = crate::settings::sources()
            .all()
            .iter()
            .map(|source| source.path.clone())
            .filter(|source| source != root && source.starts_with(root))
            .collect();
        WalkFilter {
            scope: self.clone(),
            nested_sources,
            folders: vec![Folder {
                files: Arc::new(FolderFiles::above(root)),
                source_len: below_offset(rules_root(root)),
            }],
        }
    }

    /// Whether the walk leaves out `path`, its folders let in: `files` speak
    /// for its folder, and the path below its source starts at `source_len`.
    fn leaves_out(
        &self,
        files: &FolderFiles,
        source_len: usize,
        path: &Path,
        is_dir: bool,
    ) -> bool {
        let hidden = path
            .file_name()
            .is_some_and(|name| name.as_bytes().first() == Some(&b'.'));
        hidden
            || self
                .rules
                .matched(below(path, source_len), is_dir)
                .is_ignore()
            || files.ignore(path, is_dir)
    }
}

/// [`Scope`] as one walk applies it. The walk goes depth first, so the folders
/// above an entry are the last ones entered, each read once.
pub(super) struct WalkFilter {
    scope: Scope,
    /// Sources below the walk's root: the rules anchor at each one inside it.
    nested_sources: Vec<PathBuf>,
    /// The folders from the root down to the last entry's, by depth.
    folders: Vec<Folder>,
}

struct Folder {
    files: Arc<FolderFiles>,
    source_len: usize,
}

impl WalkFilter {
    /// Whether the walk keeps `path`, found `depth` folders below its root.
    pub(super) fn keeps(&mut self, path: &Path, depth: usize, is_dir: bool) -> bool {
        if depth == 0 {
            return true;
        }
        // What lies past `depth` belongs to the folders of an earlier entry.
        self.folders.truncate(depth);
        if is_dir && self.nested_sources.iter().any(|source| source == path) {
            return true;
        }
        while let Some(parent) = self.folders.last()
            && self.folders.len() < depth
            && let Some(folder) = path.ancestors().nth(depth - self.folders.len())
        {
            let files = parent
                .files
                .enter(folder)
                .map_or_else(|| Arc::clone(&parent.files), Arc::new);
            let source_len = if self.nested_sources.iter().any(|source| source == folder) {
                below_offset(folder)
            } else {
                parent.source_len
            };
            self.folders.push(Folder { files, source_len });
        }
        let Some(folder) = self.folders.last() else {
            return true;
        };
        !self
            .scope
            .leaves_out(&folder.files, folder.source_len, path, is_dir)
    }
}

/// Whether git ignores `path`, as the walk from the source holding it would
/// have read git's files on the way: the nearest `.gitignore` with an opinion,
/// up to the root of the work tree, then its `info/exclude`, then the global
/// excludes file. Nothing re-includes what lies in an ignored folder, which
/// the walk never enters.
pub fn repo_ignored(path: &Path, is_dir: bool) -> bool {
    walk_down(path, is_dir, true, |files, _, entry, entry_is_dir| {
        files.git_opinion(entry, entry_is_dir) == Some(true)
    })
}

/// Whether `leaves_out` leaves out `path` or a folder between it and the
/// source holding it, each asked with the files of the folder holding it
/// (none unless `read_files`) and where its path below the source starts.
fn walk_down(
    path: &Path,
    is_dir: bool,
    read_files: bool,
    leaves_out: impl Fn(&FolderFiles, usize, &Path, bool) -> bool,
) -> bool {
    let source = rules_root(path);
    let source_len = below_offset(source);
    // The ancestors of a path below the source that are longer than the
    // source lie below it too.
    let mut entries: Vec<&Path> = path
        .ancestors()
        .take_while(|entry| entry.as_os_str().len() > source.as_os_str().len())
        .collect();
    entries.reverse();
    let Some((&last, folders)) = entries.split_last() else {
        return false;
    };
    let mut files = if read_files {
        FolderFiles::above(source)
    } else {
        FolderFiles::default()
    };
    for &folder in folders {
        if leaves_out(&files, source_len, folder, true) {
            return true;
        }
        if read_files && let Some(entered) = files.enter(folder) {
            files = entered;
        }
    }
    leaves_out(&files, source_len, last, is_dir)
}

/// The ignore files that speak for the entries of one folder, the nearest
/// folder's first.
#[derive(Clone, Default)]
struct FolderFiles {
    dot_ignores: Vec<Arc<Gitignore>>,
    /// The `.gitignore` files up to the root of the work tree, then its
    /// `info/exclude`; empty outside a work tree, as git reads them.
    gitignores: Vec<Arc<Gitignore>>,
    /// Where a path below the work tree's root starts, for the global
    /// excludes; `None` outside a work tree.
    work_tree_len: Option<usize>,
}

impl FolderFiles {
    /// The files that speak for the entries of `dir`: those of every folder
    /// from `/` down to it.
    fn above(dir: &Path) -> Self {
        let folders: Vec<&Path> = dir.ancestors().collect();
        let mut files = Self::default();
        for folder in folders.into_iter().rev() {
            if let Some(entered) = files.enter(folder) {
                files = entered;
            }
        }
        files
    }

    /// The files that speak for the entries of `dir`, a folder these speak
    /// for; `None` when its own add nothing.
    fn enter(&self, dir: &Path) -> Option<Self> {
        let dot_git = std::fs::symlink_metadata(dir.join(".git")).ok();
        // A Jujutsu repository not colocated with git is a work tree too: its
        // `.gitignore` files and the global excludes apply, with no exclude file.
        let work_tree_root =
            dot_git.is_some() || std::fs::symlink_metadata(dir.join(".jj")).is_ok();
        let dot_ignore = read_ignore_file(dir, &dir.join(".ignore"));
        let gitignore = if work_tree_root || self.work_tree_len.is_some() {
            read_ignore_file(dir, &dir.join(".gitignore"))
        } else {
            None
        };
        if !work_tree_root && dot_ignore.is_none() && gitignore.is_none() {
            return None;
        }
        let mut files = self.clone();
        if let Some(rules) = dot_ignore {
            files.dot_ignores.insert(0, rules);
        }
        if work_tree_root {
            // A work tree inside another (a submodule, a nested clone) has
            // only its own git rules.
            files.gitignores.clear();
            if let Some(dot_git) = dot_git {
                let exclude = git_common_dir(dir, dot_git.is_file()).join("info/exclude");
                files.gitignores.extend(read_ignore_file(dir, &exclude));
            }
            files.work_tree_len = Some(below_offset(dir));
        }
        if let Some(rules) = gitignore {
            files.gitignores.insert(0, rules);
        }
        Some(files)
    }

    /// Whether the first file with an opinion on `path`, the `.ignore` files
    /// before git's, ignores it.
    fn ignore(&self, path: &Path, is_dir: bool) -> bool {
        first_opinion(&self.dot_ignores, path, is_dir)
            .or_else(|| self.git_opinion(path, is_dir))
            .unwrap_or(false)
    }

    /// Whether git ignores `path`, if any of its files has an opinion.
    fn git_opinion(&self, path: &Path, is_dir: bool) -> Option<bool> {
        let work_tree_len = self.work_tree_len?;
        first_opinion(&self.gitignores, path, is_dir).or_else(|| {
            let global = global_excludes().matched(below(path, work_tree_len), is_dir);
            (!global.is_none()).then(|| global.is_ignore())
        })
    }
}

fn first_opinion(files: &[Arc<Gitignore>], path: &Path, is_dir: bool) -> Option<bool> {
    files
        .iter()
        .map(|rules| rules.matched(path, is_dir))
        .find(|matched| !matched.is_none())
        .map(|matched| matched.is_ignore())
}

/// The rules of an ignore file, matched below `dir`; `None` when there is no
/// such file or it holds no rule.
fn read_ignore_file(dir: &Path, file: &Path) -> Option<Arc<Gitignore>> {
    if !file.is_file() {
        return None;
    }
    let mut builder = GitignoreBuilder::new(dir);
    // A line that does not parse leaves the others in force, as in the walk.
    let _ = builder.add(file);
    builder
        .build()
        .ok()
        .filter(|rules| !rules.is_empty())
        .map(Arc::new)
}

/// The global git excludes file, compiled at `.` and matched below the root of
/// each work tree, as git anchors it. Read once: like the effective rules, a
/// change applies from the next start, whose fingerprint covers this file.
fn global_excludes() -> &'static Gitignore {
    static GLOBAL: OnceLock<Gitignore> = OnceLock::new();
    GLOBAL.get_or_init(|| GitignoreBuilder::new(".").build_global().0)
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

/// Where the path of an entry below `folder` starts, its separator skipped.
fn below_offset(folder: &Path) -> usize {
    let bytes = folder.as_os_str().as_bytes();
    bytes.len() + usize::from(!bytes.ends_with(b"/"))
}

/// `path` from `offset` on: below the folder [`below_offset`] measured, which
/// holds it. Slices bytes, since the walk asks this about every entry.
fn below(path: &Path, offset: usize) -> &Path {
    let bytes = path.as_os_str().as_bytes();
    Path::new(OsStr::from_bytes(bytes.get(offset..).unwrap_or(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The walk's rules anchor at each source it enters, whatever folder it
    /// read before: a sibling read deeper first leaves no level behind.
    #[test]
    fn walk_anchors_at_nested_source() {
        let base = crate::test_env::scratch_dir("lsearch-walk-anchor");
        let rules = base.join("effective.ignore");
        std::fs::write(&rules, "/Archive/\n").unwrap();
        let home = Path::new("/nonexistent/home");
        let mut filter = WalkFilter {
            scope: Scope::from_rules(&rules),
            nested_sources: vec![home.join("sync")],
            folders: vec![Folder {
                files: Arc::default(),
                source_len: below_offset(home),
            }],
        };
        // Depth-first order, as the walk reports it.
        let walk = [
            ("Archive", 1, true, false),
            ("docs", 1, true, true),
            ("docs/Archive", 2, true, true),
            ("docs/Archive/deeper.txt", 3, false, true),
            ("sync", 1, true, true),
            ("sync/Archive", 2, true, false),
            ("sync/keep.txt", 2, false, true),
            ("sync/sub", 2, true, true),
            ("sync/sub/Archive", 3, true, true),
            (".hidden", 1, false, false),
        ];
        for (entry, depth, is_dir, kept) in walk {
            assert_eq!(
                filter.keeps(&home.join(entry), depth, is_dir),
                kept,
                "{entry}"
            );
        }
    }
}
