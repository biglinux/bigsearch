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

/// Whether this document earns a version: its metadata if so, else why not.
///
/// Cheapest test first. Extensions are an allow list because a block list is
/// how somebody discovers the problem with a 100 GB file.
pub(super) fn eligible_metadata(path: &Path) -> Result<std::fs::Metadata, &'static str> {
    eligible_name(path)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "it cannot be read")?;
    if !metadata.is_file() {
        return Err("it is not a regular file");
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err("it is over the size versions are kept up to");
    }
    if metadata.len() == 0 {
        return Err("it is empty");
    }
    // The watcher's paths pass no link, but a folder on the way may have been
    // replaced by one since: the file there is another document, whose
    // version would join this path's history and take on its identity.
    let folder = path.parent().ok_or("it has no folder")?;
    let real = std::fs::canonicalize(folder).map_err(|_| "its folder cannot be resolved")?;
    if real != folder {
        return Err("a folder on its path is a link");
    }
    Ok(metadata)
}

/// Whether a path is even the kind of file that earns versions.
///
/// The name alone, no disk: callers use it to drop most of what the watcher
/// reports before it costs a channel send and a place in the worker's queue.
#[must_use]
pub fn may_be_a_document(path: &Path) -> bool {
    name_refusal(path).is_none()
}

/// Why the name alone keeps `path` out of the versions.
fn name_refusal(path: &Path) -> Option<&'static str> {
    // The extension first, without building a string: it rejects almost every
    // path, and the name check allocates.
    let document = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            DOCUMENT_EXTENSIONS
                .iter()
                .any(|allowed| extension.eq_ignore_ascii_case(allowed))
        });
    if !document {
        return Some("it has no document extension");
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Some("its name is not UTF-8");
    };
    let name = name.to_ascii_lowercase();
    NEVER_KEPT
        .iter()
        .any(|marker| name.contains(marker))
        .then_some("its name marks a password or a key")
}

/// The half of eligibility that is about the name alone, with nothing read from
/// disk, so it can also answer about a path that has disappeared.
pub(super) fn eligible_name(path: &Path) -> Result<(), &'static str> {
    if let Some(refusal) = name_refusal(path) {
        return Err(refusal);
    }
    // Rows are keyed by UTF-8 text: a lossy key for a path in a non-UTF-8
    // folder never exists on disk, so its versions would expire under it.
    if path.to_str().is_none() {
        return Err("its folder's name is not UTF-8");
    }
    if scope().excluded(path, false) {
        return Err("an exclusion rule covers it");
    }
    if crate::settings::sources().owner(path).is_none() {
        return Err("it is not in a searched folder");
    }
    Ok(())
}

fn scope() -> &'static crate::scan::Scope {
    static SCOPE: OnceLock<crate::scan::Scope> = OnceLock::new();
    SCOPE.get_or_init(crate::scan::Scope::compile)
}

/// The nearest folder above `path` that is the root of a work tree
/// ([`crate::scan::work_tree_root`]).
///
/// The walk stops at the catalogued folder that owns `path`: a repository
/// above it belongs to something the person did not ask to have searched.
pub(super) fn work_tree(path: &Path) -> Option<&Path> {
    let top = crate::settings::sources()
        .owner(path)
        .map(|source| source.path.as_path());
    for dir in path.ancestors().skip(1) {
        if crate::scan::work_tree_root(dir).is_some() {
            return Some(dir);
        }
        if Some(dir) == top {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_trees_and_ignores_found() {
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

        // A Jujutsu repository with no `.git` is one too.
        std::fs::create_dir_all(root.join("jj/.jj")).unwrap();

        assert_eq!(
            work_tree(&repo.join("docs/drafts/a.md")),
            Some(repo.as_path())
        );
        assert_eq!(
            work_tree(&repo.join("sub/b.md")),
            Some(repo.join("sub").as_path())
        );
        assert_eq!(
            work_tree(&root.join("jj/c.md")),
            Some(root.join("jj").as_path())
        );
        assert_eq!(work_tree(&root.join("loose.md")), None);

        let ignored = |relative: &str| crate::scan::repo_ignored(&repo.join(relative), false);
        assert!(ignored("build/report.md"));
        assert!(ignored("docs/drafts/log.txt"));
        assert!(ignored("private.md"));
        assert!(!ignored("keep.txt"));
        // The nearest `.gitignore` overrides the one above it.
        assert!(!ignored("docs/notes.txt"));
        assert!(!ignored("docs/drafts/a.md"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only documents, and never the files somebody keeps their passwords in.
    #[test]
    fn only_documents_earn_a_version() {
        let of = |name: &str| may_be_a_document(&Path::new("/home/p/Documentos").join(name));
        assert!(of("contrato.odt"));
        assert!(of("Notas.MD"));
        assert!(of("planilha.xlsx"));
        assert!(!of("filme.mkv"));
        assert!(!of("disco.qcow2"));
        assert!(!of("sem-extensao"));
        assert!(!of("senhas.txt"));
        assert!(!of("Password.md"));
        assert!(!of("cofre.kdbx"));
        assert!(!of("id_rsa.pem"));
        // Source code is left to git.
        assert!(!of("main.rs"));
    }
}
