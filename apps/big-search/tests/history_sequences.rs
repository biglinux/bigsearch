//! Random sequences of what happens to documents (saves, folder moves, links
//! left in a folder's place, deletions, forgets, restores), with the service
//! running or stopped between steps, checked after each step against what the
//! versions store promises: one file has one history row, a forgotten file
//! leaves no version under any key, and no object outlives its version.
//!
//! Its own process, for the reasons `history_store.rs` gives, and like it,
//! unsupported on a disk that cannot share blocks.
#![cfg(not(miri))]

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use big_search::history::{History, Reason, Support};
use big_search::settings::{normalized_client_path, sources};
use rusqlite::{Connection, params};

const SEQUENCES: u64 = 48;
const STEPS: usize = 60;
const FOLDER_NAMES: [&str; 3] = ["p", "q", "r"];
const DOCUMENT_NAMES: [&str; 3] = ["a.odt", "b.odt", "c.odt"];

/// splitmix64: the same seed gives the same sequence on every machine.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        usize::try_from((mixed ^ (mixed >> 31)) % bound as u64).expect("below a usize bound")
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())].clone())
    }
}

/// What lies below the documents folder, links not followed, in path order.
#[derive(Default)]
struct Tree {
    documents: Vec<PathBuf>,
    folders: Vec<PathBuf>,
    /// Each link to a folder, with the folder it leads to.
    links: Vec<(PathBuf, PathBuf)>,
}

fn survey(folder: &Path, tree: &mut Tree) {
    let mut entries: Vec<_> = std::fs::read_dir(folder)
        .expect("list a folder")
        .map(|entry| entry.expect("read an entry"))
        .collect();
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type().expect("entry type");
        if kind.is_dir() {
            tree.folders.push(path.clone());
            survey(&path, tree);
        } else if kind.is_symlink() {
            if let Ok(target) = std::fs::canonicalize(&path)
                && target.is_dir()
            {
                tree.links.push((path, target));
            }
        } else if path.extension().is_some_and(|extension| extension == "odt") {
            tree.documents.push(path);
        }
    }
}

/// `(dev, ino, born)` as the store records a file.
fn identity(path: &Path) -> (i64, i64, Option<i64>) {
    let metadata = std::fs::symlink_metadata(path).expect("stat a document");
    let born = metadata
        .created()
        .ok()
        .and_then(|created| created.duration_since(UNIX_EPOCH).ok())
        .and_then(|since| i64::try_from(since.as_nanos()).ok());
    (
        i64::try_from(metadata.dev()).expect("device number"),
        i64::try_from(metadata.ino()).expect("inode number"),
        born,
    )
}

/// The ids of the rows recorded for the file with this identity.
fn rows_of(conn: &Connection, (dev, ino, born): (i64, i64, Option<i64>)) -> Vec<i64> {
    conn.prepare(
        "SELECT id FROM files
          WHERE dev = ?1 AND ino = ?2 AND (born IS NULL OR ?3 IS NULL OR born = ?3)",
    )
    .expect("prepare")
    .query_map(params![dev, ino, born], |row| row.get(0))
    .expect("query")
    .map(|id| id.expect("row id"))
    .collect()
}

fn row_at(conn: &Connection, path: &Path) -> Option<i64> {
    conn.query_row(
        "SELECT id FROM files WHERE current_path = ?1",
        params![path.to_str().expect("UTF-8 path")],
        |row| row.get(0),
    )
    .ok()
}

fn has_row(conn: &Connection, id: i64) -> bool {
    conn.query_row("SELECT 1 FROM files WHERE id = ?1", params![id], |_| Ok(()))
        .is_ok()
}

struct World {
    docs: PathBuf,
    history: Option<History>,
    /// The store as the checks read it, open for the whole test.
    store: Connection,
    rng: Rng,
    trace: String,
    step: usize,
}

impl World {
    fn tree(&self) -> Tree {
        let mut tree = Tree::default();
        survey(&self.docs, &mut tree);
        tree
    }

    /// Start a sequence from an empty documents folder and an empty store.
    fn reset(&mut self, seed: u64) {
        self.history = None;
        std::fs::remove_dir_all(&self.docs).expect("empty the documents folder");
        std::fs::create_dir(&self.docs).expect("create the documents folder");
        self.store
            .execute_batch("DELETE FROM versions; DELETE FROM files;")
            .expect("empty the store");
        let _ = std::fs::remove_dir_all(big_search::config::history_dir().join("objects"));
        self.history = Some(History::open(&big_search_config::FileHistory::default()));
        self.rng = Rng(seed);
        self.trace = format!("seed {seed}\n");
        self.step = 0;
    }

    fn note(&mut self, line: &str) {
        let _ = writeln!(self.trace, "{:3} {line}", self.step);
    }

    fn fail(&self, what: &str) -> ! {
        panic!("{what}\nsteps:\n{}", self.trace);
    }

    /// A path the person could use for `document`: its own, or one through a
    /// link that leads to a folder above it.
    fn spelling(&mut self, tree: &Tree, document: &Path) -> PathBuf {
        let mut spellings = vec![document.to_path_buf()];
        for (link, target) in &tree.links {
            if let Ok(below) = document.strip_prefix(target) {
                spellings.push(link.join(below));
            }
        }
        self.rng
            .pick(&spellings)
            .expect("at least the document itself")
    }

    /// A folder something can be created in: the documents folder, or one
    /// below it, at most two deep.
    fn parent(&mut self, tree: &Tree) -> PathBuf {
        let mut parents = vec![self.docs.clone()];
        parents.extend(
            tree.folders
                .iter()
                .filter(|folder| {
                    folder
                        .strip_prefix(&self.docs)
                        .map_or(0, |relative| relative.components().count())
                        < 2
                })
                .cloned(),
        );
        self.rng.pick(&parents).expect("the documents folder")
    }

    fn write(&self, path: &Path) {
        // What document editors do: a temporary file renamed over the
        // original, so every save is a new inode.
        let temporary = path.with_extension("tmp-save");
        std::fs::write(
            &temporary,
            format!("{} at step {}", path.display(), self.step),
        )
        .expect("write the temporary file");
        std::fs::rename(&temporary, path).expect("rename over the document");
    }

    /// What the watcher's save leads to while the service runs.
    fn saved(&mut self, path: &Path) {
        if let Some(history) = &mut self.history {
            history
                .save_version(path, Reason::Saved)
                .expect("save a version");
        }
    }

    fn step_once(&mut self) {
        let tree = self.tree();
        match self.rng.below(22) {
            0..=3 => {
                let path = self
                    .parent(&tree)
                    .join(self.rng.pick(&DOCUMENT_NAMES).expect("a name"));
                if std::fs::symlink_metadata(&path).is_err() {
                    self.note(&format!("create {}", path.display()));
                    self.write(&path);
                    self.saved(&path);
                }
            }
            4..=7 => {
                if let Some(document) = self.rng.pick(&tree.documents) {
                    self.note(&format!("save {}", document.display()));
                    self.write(&document);
                    self.saved(&document);
                }
            }
            8 | 9 => {
                let path = self
                    .parent(&tree)
                    .join(self.rng.pick(&FOLDER_NAMES).expect("a name"));
                if std::fs::symlink_metadata(&path).is_err() {
                    self.note(&format!("mkdir {}", path.display()));
                    std::fs::create_dir(&path).expect("create a folder");
                }
            }
            10..=12 => self.move_folder(&tree),
            13 => self.link_folder(&tree),
            14 => {
                if let Some(document) = self.rng.pick(&tree.documents) {
                    self.note(&format!("rm {}", document.display()));
                    std::fs::remove_file(&document).expect("delete a document");
                }
            }
            15 => {
                if let Some(folder) = self.rng.pick(&tree.folders) {
                    self.note(&format!("rm -r {}", folder.display()));
                    std::fs::remove_dir_all(&folder).expect("delete a folder");
                }
            }
            16 => self.rename_document(&tree),
            17 | 18 => self.forget(&tree),
            19 => self.restore(&tree),
            _ => self.stop_or_start(&tree),
        }
    }

    /// `mv from to`, sometimes with a link left where the folder was.
    fn move_folder(&mut self, tree: &Tree) {
        let Some(from) = self.rng.pick(&tree.folders) else {
            return;
        };
        let to = self
            .parent(tree)
            .join(self.rng.pick(&FOLDER_NAMES).expect("a name"));
        let free = match std::fs::symlink_metadata(&to) {
            Err(_) => true,
            // rename(2) replaces only an empty folder.
            Ok(metadata) => {
                metadata.is_dir() && std::fs::read_dir(&to).expect("list").next().is_none()
            }
        };
        if !free || to.starts_with(&from) {
            return;
        }
        let link = self.rng.below(2) == 0;
        self.note(&format!(
            "mv {} {}{}",
            from.display(),
            to.display(),
            if link { " and link" } else { "" }
        ));
        std::fs::rename(&from, &to).expect("move a folder");
        if let Some(history) = &self.history {
            history.note_rename(&from, &to).expect("follow the rename");
        }
        if link {
            std::os::unix::fs::symlink(&to, &from).expect("link the old place");
        }
    }

    /// A folder replaced by a link to another one.
    fn link_folder(&mut self, tree: &Tree) {
        let (Some(replaced), Some(target)) =
            (self.rng.pick(&tree.folders), self.rng.pick(&tree.folders))
        else {
            return;
        };
        if target.starts_with(&replaced) {
            return;
        }
        self.note(&format!(
            "ln -s {} over {}",
            target.display(),
            replaced.display()
        ));
        std::fs::remove_dir_all(&replaced).expect("delete the folder");
        std::os::unix::fs::symlink(&target, &replaced).expect("link in its place");
    }

    /// `mv` of a document within its folder, over another one or not.
    fn rename_document(&mut self, tree: &Tree) {
        let Some(from) = self.rng.pick(&tree.documents) else {
            return;
        };
        let to = from.with_file_name(self.rng.pick(&DOCUMENT_NAMES).expect("a name"));
        if to == from || std::fs::symlink_metadata(&to).is_ok_and(|metadata| !metadata.is_file()) {
            return;
        }
        self.note(&format!("mv {} {}", from.display(), to.display()));
        std::fs::rename(&from, &to).expect("rename a document");
        if let Some(history) = &self.history {
            history.note_rename(&from, &to).expect("follow the rename");
        }
        self.saved(&to);
    }

    /// A forget request, as the service answers it: rows of the first
    /// spelling with one, and every row of the file the path leads to,
    /// unless that first row is another file's kept under another path.
    fn forget(&mut self, tree: &Tree) {
        let Some(document) = self.rng.pick(&tree.documents) else {
            return;
        };
        if self.history.is_none() {
            return;
        }
        let spelled = normalized_client_path(&self.spelling(tree, &document)).expect("absolute");
        let resolved = sources().resolved_client_path(&spelled);
        let spellings: Vec<PathBuf> = std::iter::once(spelled.clone())
            .chain(resolved.clone())
            .collect();
        self.note(&format!("forget {}", spelled.display()));

        let conn = &self.store;
        let file = identity(&document);
        let stored = spellings
            .iter()
            .find_map(|path| Some((row_at(conn, path)?, path.clone())));
        let another_files = stored.as_ref().is_some_and(|(id, path)| {
            Some(path) != resolved.as_ref() && !rows_of(conn, file).contains(id)
        });
        self.history
            .as_mut()
            .expect("running")
            .forget(&spellings, resolved.as_deref())
            .expect("forget");

        if let Some((id, _)) = stored
            && has_row(conn, id)
        {
            self.fail("the row versions were listed from survived its forget");
        }
        if another_files {
            return;
        }
        if !rows_of(conn, file).is_empty() {
            self.fail("a row of the forgotten file survived");
        }
        if resolved.is_some_and(|resolved| row_at(conn, &resolved).is_some()) {
            self.fail("the row at the forgotten file's path survived");
        }
    }

    /// The file manager's restore: keep the state now through the spelling it
    /// was given, write the oldest version back, and the watcher's save.
    fn restore(&mut self, tree: &Tree) {
        let Some(document) = self.rng.pick(&tree.documents) else {
            return;
        };
        let spelled = normalized_client_path(&self.spelling(tree, &document)).expect("absolute");
        let Some(path) = sources().resolved_client_path(&spelled) else {
            return;
        };
        let Some(history) = &mut self.history else {
            return;
        };
        let outcome = if spelled == path {
            Ok(())
        } else {
            history.adopt_spelling(&spelled, &path)
        }
        .and_then(|()| history.save_version(&path, Reason::BeforeRestore));
        let oldest = history.versions(&[&path]).expect("versions").pop();
        self.note(&format!("restore {}: {outcome:?}", spelled.display()));
        if let (Ok(_), Some(oldest)) = (outcome, oldest) {
            let temporary = path.with_extension("tmp-save");
            std::fs::copy(&oldest.object, &temporary).expect("copy the version back");
            std::fs::rename(&temporary, &path).expect("put it in place");
            self.saved(&path);
        }
    }

    /// Stop the service, or start it with the pass over the catalogue that
    /// keeps what changed while it was stopped.
    fn stop_or_start(&mut self, tree: &Tree) {
        if self.history.take().is_some() {
            self.note("stop");
            return;
        }
        self.note("start");
        let mut history = History::open(&big_search_config::FileHistory::default());
        for document in &tree.documents {
            let metadata = std::fs::metadata(document).expect("stat");
            let unkept = history.unkept(vec![(document.clone(), metadata.mtime(), metadata.len())]);
            if !unkept.is_empty() {
                history
                    .save_version(document, Reason::Saved)
                    .expect("save a version");
            }
        }
        self.history = Some(history);
    }

    fn check(&self) {
        let conn = &self.store;
        for document in self.tree().documents {
            if rows_of(conn, identity(&document)).len() > 1 {
                self.fail(&format!(
                    "{} has more than one history row",
                    document.display()
                ));
            }
        }
        let mut referenced: Vec<String> = conn
            .prepare("SELECT object FROM versions")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .map(|object| object.expect("object name"))
            .collect();
        referenced.sort();
        let objects = big_search::config::history_dir().join("objects");
        let mut on_disk = Vec::new();
        for shard in std::fs::read_dir(&objects).expect("list the objects") {
            let shard = shard.expect("shard").path();
            for object in std::fs::read_dir(&shard).expect("list a shard") {
                let object = object.expect("object").path();
                let name = object.strip_prefix(&objects).expect("below the objects");
                on_disk.push(name.to_str().expect("UTF-8 name").to_string());
            }
        }
        on_disk.sort();
        if on_disk != referenced {
            self.fail(&format!(
                "objects on disk {on_disk:?}, referenced {referenced:?}"
            ));
        }
    }
}

#[test]
fn every_sequence_keeps_the_promises() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("history-sequences");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).expect("scratch data");
    // Canonical, as the service's own paths are.
    let home = std::fs::canonicalize(&root).expect("canonical scratch home");
    // SAFETY: this test binary is its own process and sets these once, before
    // anything reads them.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_DATA_HOME", home.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    }
    let docs = home.join("Documentos");
    std::fs::create_dir(&docs).expect("create the documents folder");
    let history = History::open(&big_search_config::FileHistory::default());
    let mut world = World {
        docs,
        history: Some(history),
        store: Connection::open(big_search::config::history_db_path())
            .expect("open the store to check"),
        rng: Rng(0),
        trace: String::new(),
        step: 0,
    };

    for seed in 0..SEQUENCES {
        world.reset(seed);
        let probe = world.docs.join("probe.odt");
        world.write(&probe);
        world.saved(&probe);
        if let Some(Support::Unavailable(reason)) = world.history.as_ref().map(History::support) {
            eprintln!(
                "this filesystem keeps no versions ({}); test declared unsupported",
                reason.as_str()
            );
            return;
        }
        for step in 1..=STEPS {
            world.step = step;
            world.step_once();
            world.check();
        }
    }
}
