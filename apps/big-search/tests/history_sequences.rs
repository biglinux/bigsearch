//! Random sequences of what happens to documents (saves the service hears or
//! misses, moves of documents and folders, links left in a folder's place or
//! to a document, deletions, forgets, restores), with the service running or
//! stopped between steps and its pass over the catalogue taking documents in
//! any order, checked after each step against what the versions store
//! promises: a forget leaves nothing seen through the path, no object
//! outlives its version, a parked row never gets a path back, the versions
//! listed for a document were taken of it or of a document that is gone, a
//! living document keeps a version once it had one until it is forgotten,
//! and one whose every change was heard keeps them all at its path.
//!
//! `HISTORY_SEQUENCES_SEED=N` runs only sequence N, as a failure names it.
//! Its own process, for the reasons `history_store.rs` gives, and like it,
//! unsupported on a disk that cannot share blocks.
#![cfg(not(miri))]

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use big_search::history::{History, Reason, SharedHistory, Support, check_kept_paths};
use big_search::settings::{normalized_client_path, sources};
use rusqlite::{Connection, params};

const SEQUENCES: u64 = 96;
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

    /// Fisher-Yates.
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for last in (1..items.len()).rev() {
            items.swap(last, self.below(last + 1));
        }
    }
}

/// What lies below the documents folder, links not followed, in path order.
#[derive(Default)]
struct Tree {
    documents: Vec<PathBuf>,
    folders: Vec<PathBuf>,
    /// Each link to a folder, with the folder it leads to.
    links: Vec<(PathBuf, PathBuf)>,
    /// Each link to a document, with the document it leads to.
    document_links: Vec<(PathBuf, PathBuf)>,
    /// Links that lead nowhere.
    dangling: Vec<PathBuf>,
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
            match std::fs::canonicalize(&path) {
                Ok(target) if target.is_dir() => tree.links.push((path, target)),
                Ok(target) => tree.document_links.push((path, target)),
                Err(_) => tree.dangling.push(path),
            }
        } else if path.extension().is_some_and(|extension| extension == "odt") {
            tree.documents.push(path);
        }
    }
}

/// `(fs, ino, born)` as the store records a file: the filesystem is its
/// `f_fsid` (`statvfs(3)`), or the device number where it reports none.
#[expect(
    clippy::disallowed_methods,
    reason = "an oracle apart from the store's own identity"
)]
fn identity(path: &Path) -> (i64, i64, Option<i64>) {
    use std::os::unix::ffi::OsStrExt;
    let metadata = std::fs::symlink_metadata(path).expect("stat a document");
    let born = metadata
        .created()
        .ok()
        .and_then(|created| created.duration_since(UNIX_EPOCH).ok())
        .and_then(|since| i64::try_from(since.as_nanos()).ok());
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL");
    // SAFETY: `stats` is a correctly sized output buffer and `c_path` a valid
    // NUL-terminated string for the duration of the call.
    let fsid = unsafe {
        let mut stats = std::mem::zeroed::<libc::statvfs>();
        assert_eq!(libc::statvfs(c_path.as_ptr(), &raw mut stats), 0, "statvfs");
        stats.f_fsid
    };
    let fs = if fsid == 0 { metadata.dev() } else { fsid };
    (
        fs.cast_signed(),
        i64::try_from(metadata.ino()).expect("inode number"),
        born,
    )
}

/// The ids of the rows recorded for the file with this identity.
fn rows_of(conn: &Connection, (fs, ino, born): (i64, i64, Option<i64>)) -> Vec<i64> {
    conn.prepare(
        "SELECT id FROM files
          WHERE fs = ?1 AND ino = ?2 AND (born IS NULL OR ?3 IS NULL OR born = ?3)",
    )
    .expect("prepare")
    .query_map(params![fs, ino, born], |row| row.get(0))
    .expect("query")
    .map(|id| id.expect("row id"))
    .collect()
}

/// The ids of the parked rows taken from `path`.
fn parked_from(conn: &Connection, path: &Path) -> Vec<i64> {
    conn.prepare("SELECT id FROM files WHERE parked_from = ?1")
        .expect("prepare")
        .query_map(params![path.to_str().expect("UTF-8 path")], |row| {
            row.get(0)
        })
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

/// The objects of every version, or of row `id`'s.
fn objects(conn: &Connection, id: Option<i64>) -> Vec<String> {
    conn.prepare("SELECT object FROM versions WHERE ?1 IS NULL OR file_id = ?1")
        .expect("prepare")
        .query_map(params![id], |row| row.get(0))
        .expect("query")
        .map(|object| object.expect("object name"))
        .collect()
}

/// Every key the store holds.
fn keys(conn: &Connection) -> Vec<PathBuf> {
    conn.prepare("SELECT current_path FROM files WHERE current_path IS NOT NULL")
        .expect("prepare")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .map(|key| PathBuf::from(key.expect("key")))
        .collect()
}

/// The ids of the parked rows.
fn parked(conn: &Connection) -> Vec<i64> {
    conn.prepare("SELECT id FROM files WHERE current_path IS NULL")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .map(|id| id.expect("row id"))
        .collect()
}

/// The service as it starts: its store opened and the paths it keeps
/// checked before anything it hears.
fn started() -> SharedHistory {
    let history = SharedHistory::new(History::open(&big_search_config::FileHistory::default()));
    check_kept_paths(&history).expect("check the kept paths");
    history
}

struct World {
    docs: PathBuf,
    history: Option<SharedHistory>,
    /// The store as the checks read it, open for the whole test.
    store: Connection,
    rng: Rng,
    trace: String,
    step: usize,
    /// Each document's lineage, by its path: a number given when it is
    /// created, kept through saves, restores and moves.
    lineage: HashMap<PathBuf, u32>,
    lineages: u32,
    /// The lineage of the document each object was taken of.
    taken: HashMap<String, u32>,
    /// Lineages with a version kept since a forget last erased theirs.
    kept: HashSet<u32>,
    /// Lineages with a change the service did not hear: their versions may
    /// lie parked, or at a path they left.
    scattered: HashSet<u32>,
    /// Rows seen parked, which no step may key again.
    parked: HashSet<i64>,
    /// Writes so far, which sets each write's length: no two states of
    /// different writes look alike to the catalogue's `(mtime, size)`.
    writes: usize,
    /// Documents the pass after a start has yet to reach. Requests are
    /// served meanwhile, as the service serves them.
    catch_up: Vec<PathBuf>,
}

impl World {
    fn tree(&self) -> Tree {
        let mut tree = Tree::default();
        survey(&self.docs, &mut tree);
        tree
    }

    fn new(docs: PathBuf) -> Self {
        // The service creates the store the checks then read.
        let history = started();
        Self {
            docs,
            history: Some(history),
            store: Connection::open(big_search::config::history_db_path())
                .expect("open the store to check"),
            rng: Rng(0),
            trace: String::new(),
            step: 0,
            lineage: HashMap::new(),
            lineages: 0,
            taken: HashMap::new(),
            kept: HashSet::new(),
            scattered: HashSet::new(),
            parked: HashSet::new(),
            writes: 0,
            catch_up: Vec::new(),
        }
    }

    /// Start a sequence from an empty store and a documents folder holding
    /// one kept document.
    fn reset(&mut self, seed: u64) {
        self.history = None;
        std::fs::remove_dir_all(&self.docs).expect("empty the documents folder");
        std::fs::create_dir(&self.docs).expect("create the documents folder");
        self.store
            .execute_batch("DELETE FROM versions; DELETE FROM files;")
            .expect("empty the store");
        let _ = std::fs::remove_dir_all(big_search::config::history_dir().join("objects"));
        self.history = Some(started());
        self.rng = Rng(seed);
        self.trace = format!("seed {seed} (HISTORY_SEQUENCES_SEED={seed} runs it alone)\n");
        self.step = 0;
        self.lineage.clear();
        self.taken.clear();
        self.kept.clear();
        self.scattered.clear();
        self.parked.clear();
        self.writes = 0;
        self.catch_up.clear();
        let probe = self.docs.join("probe.odt");
        self.lineages += 1;
        self.lineage.insert(probe.clone(), self.lineages);
        self.write(&probe);
        self.saved(&probe);
    }

    fn note(&mut self, line: &str) {
        let _ = writeln!(self.trace, "{:3} {line}", self.step);
    }

    fn fail(&self, what: &str) -> ! {
        panic!("{what}\nsteps:\n{}", self.trace);
    }

    fn alive(&self, lineage: u32) -> bool {
        self.lineage.values().any(|&living| living == lineage)
    }

    /// Give the lineage of `from` and everything below it to `to`.
    /// Unheard while the service is stopped, so the lineages moved scatter.
    fn carry(&mut self, from: &Path, to: &Path) {
        let moved: Vec<PathBuf> = self
            .lineage
            .keys()
            .filter(|path| path.starts_with(from))
            .cloned()
            .collect();
        for path in moved {
            let lineage = self.lineage.remove(&path).expect("listed");
            let below = path.strip_prefix(from).expect("below the folder");
            self.lineage.insert(to.join(below), lineage);
            if self.history.is_none() {
                self.scattered.insert(lineage);
            }
        }
    }

    fn bury(&mut self, folder: &Path) {
        self.lineage.retain(|path, _| !path.starts_with(folder));
    }

    /// A path the person could use for `document`: its own, one through a
    /// link that leads to a folder above it, or a link to it.
    fn spelling(&mut self, tree: &Tree, document: &Path) -> PathBuf {
        let mut spellings = vec![document.to_path_buf()];
        for (link, target) in &tree.links {
            if let Ok(below) = document.strip_prefix(target) {
                spellings.push(link.join(below));
            }
        }
        for (link, target) in &tree.document_links {
            if target == document {
                spellings.push(link.clone());
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

    fn write(&mut self, path: &Path) {
        self.writes += 1;
        // What document editors do: a temporary file renamed over the
        // original, so every save is a new inode.
        let temporary = path.with_extension("tmp-save");
        let contents = format!("write {} {}", self.writes, ".".repeat(self.writes));
        std::fs::write(&temporary, contents).expect("write the temporary file");
        std::fs::rename(&temporary, path).expect("rename over the document");
    }

    /// Note the objects kept since the last call as taken of the document
    /// at `path`.
    fn attribute(&mut self, path: &Path) {
        let lineage = *self.lineage.get(path).expect("a living document");
        for object in objects(&self.store, None) {
            if let Entry::Vacant(untaken) = self.taken.entry(object) {
                untaken.insert(lineage);
                self.kept.insert(lineage);
            }
        }
    }

    /// The lineages some version in the store was taken of.
    fn lineages_kept(&self) -> HashSet<u32> {
        objects(&self.store, None)
            .iter()
            .filter_map(|object| self.taken.get(object).copied())
            .collect()
    }

    /// What the watcher's save leads to while the service runs: the row at
    /// the path settled at once, the version kept once the file is quiet.
    fn saved(&mut self, path: &Path) {
        if let Some(history) = &self.history {
            let mut store = history.lock();
            store.note_change(path).expect("note the change");
            store
                .save_version(path, Reason::Saved)
                .expect("save a version");
            drop(store);
            self.attribute(path);
        }
    }

    fn step_once(&mut self) {
        let reached = self.rng.below(self.catch_up.len() + 1);
        self.catch_up_on(reached);
        let tree = self.tree();
        match self.rng.below(28) {
            0..=3 => self.create(&tree),
            4..=7 => self.save(&tree, true),
            8 => self.save(&tree, false),
            9 | 10 => {
                let path = self
                    .parent(&tree)
                    .join(self.rng.pick(&FOLDER_NAMES).expect("a name"));
                if std::fs::symlink_metadata(&path).is_err() {
                    self.note(&format!("mkdir {}", path.display()));
                    std::fs::create_dir(&path).expect("create a folder");
                }
            }
            11..=13 => self.move_folder(&tree),
            14 => self.link_folder(&tree),
            15 => self.link_document(&tree),
            16 => {
                if let Some(document) = self.rng.pick(&tree.documents) {
                    self.note(&format!("rm {}", document.display()));
                    std::fs::remove_file(&document).expect("delete a document");
                    self.lineage.remove(&document);
                }
            }
            17 => {
                if let Some(folder) = self.rng.pick(&tree.folders) {
                    self.note(&format!("rm -r {}", folder.display()));
                    std::fs::remove_dir_all(&folder).expect("delete a folder");
                    self.bury(&folder);
                }
            }
            18 | 19 => self.rename_document(&tree),
            20 | 21 => self.swap_documents(&tree),
            22 | 23 => self.forget(&tree),
            24 => self.restore(&tree),
            _ => self.stop_or_start(&tree),
        }
    }

    /// A path for a document: half the time one the store keeps rows under,
    /// where an arriving document meets them.
    fn document_path(&mut self, tree: &Tree) -> PathBuf {
        let kept: Vec<PathBuf> = keys(&self.store)
            .into_iter()
            .filter(|key| {
                key.parent().is_some_and(|folder| {
                    *folder == self.docs || tree.folders.iter().any(|real| real == folder)
                })
            })
            .collect();
        if self.rng.below(2) == 0
            && let Some(key) = self.rng.pick(&kept)
        {
            return key;
        }
        self.parent(tree)
            .join(self.rng.pick(&DOCUMENT_NAMES).expect("a name"))
    }

    fn create(&mut self, tree: &Tree) {
        let path = self.document_path(tree);
        if std::fs::symlink_metadata(&path).is_ok() {
            return;
        }
        self.note(&format!("create {}", path.display()));
        self.lineages += 1;
        self.lineage.insert(path.clone(), self.lineages);
        self.write(&path);
        self.saved(&path);
    }

    /// A save of a document, heard by the service or not.
    fn save(&mut self, tree: &Tree, heard: bool) {
        let Some(document) = self.rng.pick(&tree.documents) else {
            return;
        };
        self.note(&format!(
            "save {}{}",
            document.display(),
            if heard { "" } else { " unheard" }
        ));
        if !heard || self.history.is_none() {
            self.scattered.insert(self.lineage[&document]);
        }
        self.write(&document);
        if heard {
            self.saved(&document);
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
        // A folder lands only where nothing is: rows below it are those of
        // documents that left, which the service parks.
        let left: Vec<(i64, PathBuf)> = keys(&self.store)
            .into_iter()
            .filter(|key| key.starts_with(&to) && *key != to)
            .filter_map(|key| Some((row_at(&self.store, &key)?, key)))
            .collect();
        std::fs::rename(&from, &to).expect("move a folder");
        self.carry(&from, &to);
        if let Some(history) = &self.history {
            history
                .lock()
                .note_rename(&from, &to)
                .expect("follow the rename");
            for (id, key) in &left {
                if !parked_from(&self.store, key).contains(id) {
                    self.fail(&format!(
                        "the row below the destination at {} was not parked",
                        key.display()
                    ));
                }
            }
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
        self.bury(&replaced);
        std::os::unix::fs::symlink(&target, &replaced).expect("link in its place");
    }

    /// A link to a document, under a document's name.
    fn link_document(&mut self, tree: &Tree) {
        let Some(target) = self.rng.pick(&tree.documents) else {
            return;
        };
        let link = self
            .parent(tree)
            .join(self.rng.pick(&DOCUMENT_NAMES).expect("a name"));
        if std::fs::symlink_metadata(&link).is_ok() {
            return;
        }
        self.note(&format!("ln -s {} {}", target.display(), link.display()));
        std::os::unix::fs::symlink(&target, &link).expect("link a document");
    }

    /// `mv` of a document, over another one or not, within its folder or
    /// to another.
    fn rename_document(&mut self, tree: &Tree) {
        let Some(from) = self.rng.pick(&tree.documents) else {
            return;
        };
        let to = self.document_path(tree);
        self.rename(&from, &to);
    }

    /// Two documents trading names through a third, as a person swaps them:
    /// each lands where the other's rows are.
    fn swap_documents(&mut self, tree: &Tree) {
        let (Some(first), Some(second)) = (
            self.rng.pick(&tree.documents),
            self.rng.pick(&tree.documents),
        ) else {
            return;
        };
        let aside = first.with_file_name("x.odt");
        if first != second
            && std::fs::symlink_metadata(&aside).is_err()
            && self.rename(&first, &aside)
            && self.rename(&second, &first)
        {
            self.rename(&aside, &second);
        }
    }

    /// `mv from to` of a document, onto a document or not; whether it moved.
    fn rename(&mut self, from: &Path, to: &Path) -> bool {
        if to == from || std::fs::symlink_metadata(to).is_ok_and(|metadata| !metadata.is_file()) {
            return false;
        }
        self.note(&format!("mv {} {}", from.display(), to.display()));
        std::fs::rename(from, to).expect("rename a document");
        let lineage = self.lineage.remove(from).expect("a living document");
        self.lineage.insert(to.to_path_buf(), lineage);
        match &self.history {
            Some(history) => history
                .lock()
                .note_rename(from, to)
                .expect("follow the rename"),
            None => {
                self.scattered.insert(lineage);
            }
        }
        self.saved(to);
        true
    }

    /// A forget request, as the service answers it, through a living
    /// document's spelling, a key the store holds or a dangling link: every
    /// row kept under the path as spelled or where it leads, parked from
    /// either, or recorded for the file where it leads.
    fn forget(&mut self, tree: &Tree) {
        if self.history.is_none() {
            return;
        }
        let Some(spelled) = self.forget_target(tree) else {
            return;
        };
        let resolved = sources().resolved_client_path(&spelled);
        let spellings: Vec<PathBuf> = std::iter::once(spelled.clone())
            .chain(resolved.clone())
            .collect();
        self.note(&format!("forget {}", spelled.display()));
        let file = resolved
            .as_ref()
            .filter(|resolved| std::fs::symlink_metadata(resolved).is_ok())
            .map(|resolved| identity(resolved));
        let lineages_before = self.lineages_kept();
        self.history
            .as_ref()
            .expect("running")
            .lock()
            .forget(&spellings, resolved.as_deref())
            .expect("forget");
        let forgotten: Vec<u32> = lineages_before
            .difference(&self.lineages_kept())
            .copied()
            .collect();
        self.kept.retain(|lineage| !forgotten.contains(lineage));

        let conn = &self.store;
        for path in &spellings {
            if row_at(conn, path).is_some() {
                self.fail(&format!(
                    "the row at {} survived its forget",
                    path.display()
                ));
            }
            if !parked_from(conn, path).is_empty() {
                self.fail(&format!(
                    "a row parked from {} survived its forget",
                    path.display()
                ));
            }
        }
        if file.is_some_and(|file| !rows_of(conn, file).is_empty()) {
            self.fail("a row of the forgotten file survived");
        }
    }

    /// What a forget names: a living document as the person could spell it,
    /// a key the store holds, or a dangling link.
    fn forget_target(&mut self, tree: &Tree) -> Option<PathBuf> {
        let mut targets: Vec<PathBuf> = tree
            .documents
            .iter()
            .cloned()
            .chain(keys(&self.store))
            .chain(tree.dangling.iter().cloned())
            .collect();
        targets.sort();
        targets.dedup();
        let target = self.rng.pick(&targets)?;
        let spelled = if tree.documents.contains(&target) {
            self.spelling(tree, &target)
        } else {
            target
        };
        Some(normalized_client_path(&spelled).expect("absolute"))
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
        let Some(history) = &self.history else {
            return;
        };
        let mut store = history.lock();
        let outcome = if spelled == path {
            Ok(())
        } else {
            store.check_restore_through(&spelled, &path)
        }
        .and_then(|()| store.save_version(&path, Reason::BeforeRestore));
        let oldest = store.versions(&[&path]).expect("versions").pop();
        drop(store);
        self.note(&format!("restore {}: {outcome:?}", spelled.display()));
        if outcome.is_ok() {
            self.attribute(&path);
        }
        if let (Ok(_), Some(oldest)) = (outcome, oldest) {
            let temporary = path.with_extension("tmp-save");
            std::fs::copy(&oldest.object, &temporary).expect("copy the version back");
            std::fs::rename(&temporary, &path).expect("put it in place");
            self.saved(&path);
        }
    }

    /// Stop the service, or start it with the pass over the catalogue that
    /// keeps what changed while it was stopped, in no particular order and
    /// with requests served meanwhile.
    fn stop_or_start(&mut self, tree: &Tree) {
        if self.history.is_some() {
            // The pass takes seconds; nobody stops the service that soon.
            self.catch_up_on(usize::MAX);
            self.check();
            self.history = None;
            self.note("stop");
            return;
        }
        self.note("start");
        self.catch_up = tree.documents.clone();
        self.rng.shuffle(&mut self.catch_up);
        self.history = Some(started());
    }

    /// Up to `documents` more of the pass after a start: each document still
    /// there whose newest version is not its state now is saved.
    fn catch_up_on(&mut self, documents: usize) {
        for _ in 0..documents {
            let Some(document) = self.catch_up.pop() else {
                return;
            };
            let Some(history) = &self.history else {
                return;
            };
            if !self.lineage.contains_key(&document) {
                continue;
            }
            let metadata = std::fs::metadata(&document).expect("stat");
            let mut store = history.lock();
            let unkept = store.unkept(vec![(document.clone(), metadata.mtime(), metadata.len())]);
            if !unkept.is_empty() {
                store
                    .save_version(&document, Reason::Saved)
                    .expect("save a version");
                drop(store);
                self.attribute(&document);
            }
        }
    }

    fn check(&mut self) {
        let documents = self.tree().documents;
        self.check_store(&documents);
        self.check_lineage(&documents);
        self.check_preserved(&documents);
        self.check_parked();
    }

    /// The model tracks the documents on disk, and the objects on disk are
    /// those versions name.
    fn check_store(&self, documents: &[PathBuf]) {
        let conn = &self.store;
        let mut modeled: Vec<&PathBuf> = self.lineage.keys().collect();
        modeled.sort();
        if modeled != documents.iter().collect::<Vec<_>>() {
            self.fail(&format!("the model lost track: {modeled:?}"));
        }
        let mut referenced = objects(conn, None);
        referenced.sort();
        let folder = big_search::config::history_dir().join("objects");
        let mut on_disk = Vec::new();
        for shard in std::fs::read_dir(&folder).expect("list the objects") {
            let shard = shard.expect("shard").path();
            for object in std::fs::read_dir(&shard).expect("list a shard") {
                let object = object.expect("object").path();
                let name = object.strip_prefix(&folder).expect("below the objects");
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

    /// Each living document that had a version since its last forget still
    /// has one in the store, and one whose every change was heard has each
    /// of its versions in the row at its path.
    fn check_preserved(&self, documents: &[PathBuf]) {
        let in_store = self.lineages_kept();
        for document in documents {
            let lineage = self.lineage[document];
            if self.kept.contains(&lineage) && !in_store.contains(&lineage) {
                self.fail(&format!("{} lost every version it had", document.display()));
            }
            if self.scattered.contains(&lineage) {
                continue;
            }
            let at_path = row_at(&self.store, document)
                .map(|id| objects(&self.store, Some(id)))
                .unwrap_or_default();
            let elsewhere = objects(&self.store, None).into_iter().find(|object| {
                self.taken.get(object) == Some(&lineage) && !at_path.contains(object)
            });
            if let Some(object) = elsewhere {
                self.fail(&format!(
                    "{} has its version {object} away from its path",
                    document.display()
                ));
            }
        }
    }

    /// No row seen parked has a path again. An erased row's id may be
    /// given to a new row, and a forget erases in a step of its own.
    fn check_parked(&mut self) {
        let now: HashSet<i64> = parked(&self.store).into_iter().collect();
        self.parked.retain(|id| has_row(&self.store, *id));
        if let Some(id) = self.parked.iter().find(|id| !now.contains(id)) {
            self.fail(&format!("the parked row {id} has a path again"));
        }
        self.parked.extend(now);
    }

    /// The versions listed for each document were taken of it, or of a
    /// document that is gone.
    fn check_lineage(&self, documents: &[PathBuf]) {
        let Some(history) = &self.history else {
            return;
        };
        let folder = big_search::config::history_dir().join("objects");
        for document in documents {
            let lineage = self.lineage[document];
            let listed = history.lock().versions(&[document]).expect("versions");
            for version in listed {
                let name = version
                    .object
                    .strip_prefix(&folder)
                    .expect("below the objects");
                let Some(&taken) = self.taken.get(name.to_str().expect("UTF-8 name")) else {
                    self.fail(&format!("{} was kept with no save", name.display()));
                };
                if taken != lineage && self.alive(taken) {
                    self.fail(&format!(
                        "{} lists a version of document {taken}, which lives on",
                        document.display()
                    ));
                }
            }
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
    // Asked before the checks open the store, which an unsupported disk never gets.
    let support = started().lock().support();
    if let Support::Unavailable(reason) = support {
        eprintln!(
            "this filesystem keeps no versions ({}); test declared unsupported",
            reason.as_str()
        );
        return;
    }
    let docs = home.join("Documentos");
    std::fs::create_dir(&docs).expect("create the documents folder");
    let mut world = World::new(docs);
    let seeds = match std::env::var("HISTORY_SEQUENCES_SEED") {
        Ok(seed) => {
            let seed = seed
                .parse()
                .expect("HISTORY_SEQUENCES_SEED is a sequence number");
            seed..seed + 1
        }
        Err(_) => 0..SEQUENCES,
    };

    for seed in seeds {
        world.reset(seed);
        for step in 1..=STEPS {
            world.step = step;
            world.step_once();
            world.check();
        }
        if world.history.is_none() {
            world.step += 1;
            world.stop_or_start(&world.tree());
        }
        world.catch_up_on(usize::MAX);
        world.check();
    }
}
