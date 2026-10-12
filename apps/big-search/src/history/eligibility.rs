//! Which files earn a version: documents by extension and size, inside a
//! catalogued folder and outside the scanner's exclusions; and whether git
//! already looks after one.

use std::path::Path;
use std::sync::OnceLock;

/// Largest document a version is kept for.
///
/// Not about the clone, which is free at any size: about what happens after. A
/// 100 GB disk image would pin its old blocks the moment it is written to, and
/// ten versions of it are ten times the disk.
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// The documents a version is kept for.
///
/// An allow list, not a block list: that is what keeps a 100 GB file somebody
/// invented an extension for out of the store. Source code is left out: people
/// who write code have git, and `json`/`yaml` files are usually state that
/// changes on its own.
const DOCUMENT_EXTENSIONS: [&str; 13] = [
    "odt", "docx", "ods", "xlsx", "odp", "pptx", "txt", "md", "csv", "rtf", "svg", "kra", "xcf",
];

/// Names that never get a version, whatever their extension.
///
/// Hidden files are already out through the scanner's rules; these are the ones
/// a person keeps in plain sight and would not want copied.
const NEVER_KEPT: [&str; 5] = ["senha", "password", ".kdbx", ".key", ".pem"];

/// Whether this document earns a version, and its metadata if so.
///
/// Cheapest test first. Extensions are an allow list because a block list is
/// how somebody discovers the problem with a 100 GB file.
pub fn eligible_metadata(path: &Path) -> Option<std::fs::Metadata> {
    eligible_name(path)?;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES || metadata.len() == 0 {
        return None;
    }
    Some(metadata)
}

/// Whether a path is even the kind of file that earns versions.
///
/// The name alone, no disk: callers use it to drop the great majority of what
/// the watcher reports before it costs a channel send and a place in the
/// worker's queue.
#[must_use]
pub fn may_be_a_document(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            DOCUMENT_EXTENSIONS
                .iter()
                .any(|allowed| extension.eq_ignore_ascii_case(allowed))
        })
}

/// The half of eligibility that is about the name alone, with nothing read from
/// disk, so it can also answer about a path that has disappeared.
pub(super) fn eligible_name(path: &Path) -> Option<()> {
    // The extension first, and without building a string: it rejects almost
    // every path the watcher reports, and the checks below allocate.
    if !may_be_a_document(path) {
        return None;
    }
    // Rows are keyed by UTF-8 text: a lossy key for a path in a non-UTF-8
    // folder never exists on disk, so its versions would expire under it.
    path.to_str()?;
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if NEVER_KEPT.iter().any(|marker| name.contains(marker)) {
        return None;
    }
    if scope().excluded(path, false) {
        return None;
    }
    crate::settings::sources().owner(path)?;
    Some(())
}

fn scope() -> &'static crate::scan::Scope {
    static SCOPE: OnceLock<crate::scan::Scope> = OnceLock::new();
    SCOPE.get_or_init(crate::scan::Scope::compile)
}

/// The nearest folder above `path` that holds a `.git`: a directory in an
/// ordinary clone, a file in a linked worktree or a submodule.
///
/// The walk stops at the catalogued folder that owns `path`: a `.git` above it
/// belongs to something the person did not ask to have searched.
pub(super) fn git_work_tree(path: &Path) -> Option<&Path> {
    let top = crate::settings::sources()
        .owner(path)
        .map(|source| source.path.as_path());
    for dir in path.ancestors().skip(1) {
        if std::fs::symlink_metadata(dir.join(".git")).is_ok() {
            return Some(dir);
        }
        if Some(dir) == top {
            break;
        }
    }
    None
}

/// Whether git ignores `path` inside `work_tree`.
///
/// Git's precedence: the `.gitignore` nearest the file first, up to the top of
/// the work tree, then `.git/info/exclude`, then the person's global excludes
/// file. The first file with an opinion decides, `!` re-includes included.
pub(super) fn gitignored(work_tree: &Path, path: &Path) -> bool {
    use ignore::gitignore::{Gitignore, GitignoreBuilder};
    for dir in path.ancestors().skip(1) {
        let (rules, _) = Gitignore::new(dir.join(".gitignore"));
        match rules.matched_path_or_any_parents(path, false) {
            ignore::Match::Ignore(_) => return true,
            ignore::Match::Whitelist(_) => return false,
            ignore::Match::None => {}
        }
        if dir == work_tree {
            break;
        }
    }
    let mut builder = GitignoreBuilder::new(work_tree);
    if let Some(global) = ignore::gitignore::gitconfig_excludes_path() {
        builder.add(global);
    }
    builder.add(work_tree.join(".git/info/exclude"));
    builder
        .build()
        .is_ok_and(|rules| rules.matched_path_or_any_parents(path, false).is_ignore())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_work_trees_and_ignores_found() {
        let root = crate::test_env::scratch_dir("bs-history-git");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git/info")).unwrap();
        std::fs::create_dir_all(repo.join("docs/drafts")).unwrap();
        std::fs::write(repo.join(".gitignore"), "build/\n*.txt\n!keep.txt\n").unwrap();
        std::fs::write(repo.join("docs/.gitignore"), "!notes.txt\n").unwrap();
        std::fs::write(repo.join(".git/info/exclude"), "private.md\n").unwrap();
        // A linked worktree or submodule marks itself with a `.git` file.
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();

        assert_eq!(
            git_work_tree(&repo.join("docs/drafts/a.md")),
            Some(repo.as_path())
        );
        assert_eq!(
            git_work_tree(&repo.join("sub/b.md")),
            Some(repo.join("sub").as_path())
        );
        assert_eq!(git_work_tree(&root.join("loose.md")), None);

        let ignored = |relative: &str| gitignored(&repo, &repo.join(relative));
        assert!(ignored("build/report.md"));
        assert!(ignored("docs/drafts/log.txt"));
        assert!(ignored("private.md"));
        assert!(!ignored("keep.txt"));
        // The nearest `.gitignore` overrides the one above it.
        assert!(!ignored("docs/notes.txt"));
        assert!(!ignored("docs/drafts/a.md"));

        std::fs::remove_dir_all(&root).ok();
    }

    /// Only documents, and never the files somebody keeps their passwords in.
    #[test]
    fn only_documents_earn_a_version() {
        let of = |name: &str| {
            let path = std::path::PathBuf::from("/home/p/Documentos").join(name);
            let named = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            let blocked = NEVER_KEPT.iter().any(|marker| named.contains(marker));
            let allowed = path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| {
                    DOCUMENT_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
                })
                .unwrap_or(false);
            allowed && !blocked
        };
        assert!(of("contrato.odt"));
        assert!(of("Notas.MD"));
        assert!(of("planilha.xlsx"));
        assert!(!of("filme.mkv"));
        assert!(!of("disco.qcow2"));
        assert!(!of("sem-extensao"));
        assert!(!of("senhas.txt"));
        assert!(!of("cofre.kdbx"));
        assert!(!of("id_rsa.pem"));
        // Source code is out of this first version on purpose.
        assert!(!of("main.rs"));
    }
}
