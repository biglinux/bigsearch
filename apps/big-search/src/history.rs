//! Earlier versions of the person's documents, kept by reflink.
//!
//! Saving over a file is, without this, permanent loss: the Rubbish Bin only
//! covers deletion and the file manager's undo only covers the last operation.
//! This keeps a copy of a document before each save, cheaply enough to leave on
//! by default, and stops by itself long before it can fill a disk.
//!
//! ## What a version costs
//!
//! `FICLONE` makes the filesystem write a second file pointing at the same
//! blocks; nothing is copied and nothing is charged. On the machine this was
//! written for, cloning a thousand documents took 1,3 s and 0 MiB.
//!
//! **That is the price of the FIRST version only.** Every document editor saves
//! by writing a temporary file and renaming it over the original, so the file
//! that comes out shares no block with the version before it — and the old
//! version starts costing its full size. The real bill is the sum of every
//! superseded version, which is why the budget below is the part that makes this
//! shippable rather than the clone.
//!
//! ## What it never does
//!
//! * Never falls back to a physical copy when the clone is refused: on a large
//!   file that is exactly the disaster the feature exists to avoid.
//! * Never writes outside `$XDG_DATA_HOME/big-search`. The service runs with
//!   `ProtectHome=read-only`; putting a version *back* is the file manager's
//!   job, not this one's.
//! * Never delays the search. A failure here is a version that does not exist,
//!   and nothing else.

use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Largest document a version is kept for.
///
/// Not about the clone, which is free at any size: about what happens after. A
/// 100 GB disk image would pin its old blocks the moment it is written to, and
/// ten versions of it are ten times the disk.
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// Free disk below which versions stop and space is given back, whatever the
/// store costs: the machine must stay usable.
const LOW_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Free disk must also be this many times what the versions cost. A share of
/// the disk would stop a 2 TB disk with 200 GB free for a store of a few
/// megabytes; what matters is whether the versions are a real part of what is
/// left.
const FREE_TO_STORE_RATIO: u64 = 10;

/// Free space under which versions stop, for a store whose superseded
/// versions cost `store_bytes`.
fn low_disk_threshold(store_bytes: u64) -> u64 {
    LOW_DISK_BYTES.max(store_bytes.saturating_mul(FREE_TO_STORE_RATIO))
}

/// Free space the eviction climbs back to, a quarter above the threshold, so
/// the thermostat does not sit on its own threshold switching on and off.
fn recovered_disk_target(store_bytes: u64) -> u64 {
    let threshold = low_disk_threshold(store_bytes);
    threshold.saturating_add(threshold / 4)
}

/// Most of the budget one document may hold.
///
/// Without it, one large file saved ten times in a day owns the whole history
/// and evicts the small letter somebody actually wanted back. Age alone is the
/// wrong yardstick when sizes differ a thousandfold.
const MAX_BUDGET_SHARE_PERCENT: u64 = 10;

/// How long a file is kept in the catalogue after its path disappears.
const MISSING_GRACE_SECONDS: i64 = 7 * 24 * 60 * 60;

/// The documents a version is kept for.
///
/// An allow list, not a block list: that is what keeps a 100 GB file somebody
/// invented an extension for out of the store. Source code is deliberately
/// absent from this first version — people who write code have git, and
/// `json`/`yaml` files are usually state that changes on its own.
const DOCUMENT_EXTENSIONS: [&str; 13] = [
    "odt", "docx", "ods", "xlsx", "odp", "pptx", "txt", "md", "csv", "rtf", "svg", "kra", "xcf",
];

/// Names that never get a version, whatever their extension.
///
/// Hidden files are already out through the scanner's rules; these are the ones
/// a person keeps in plain sight and would not want copied.
const NEVER_KEPT: [&str; 5] = ["senha", "password", ".kdbx", ".key", ".pem"];

/// Why a version was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The state a document was in before this feature ever saw it.
    Baseline,
    /// A save the service watched happen.
    Saved,
    /// The state a document was in just before somebody put an older one back.
    BeforeRestore,
}

impl Reason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Saved => "saved",
            Self::BeforeRestore => "before_restore",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "baseline" => Self::Baseline,
            "before_restore" => Self::BeforeRestore,
            _ => Self::Saved,
        }
    }
}

// The reason lives in `big-indexd-client`, with the screens that have to turn it
// into a sentence: one type, one list of reasons, no catch-all arms.
pub use big_indexd_client::Unavailable;

/// What the store knows about its own ability to work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// Nothing has been tried yet: no eligible document has turned up.
    Untested,
    Ready,
    Unavailable(Unavailable),
}

/// One kept version, as a screen needs it.
#[derive(Clone, Debug)]
pub struct Version {
    pub id: i64,
    pub saved_at: i64,
    pub size: u64,
    pub reason: Reason,
    /// The file this version can be read from. Readable by the person, never
    /// writable: opening it must not let a program save over the version.
    pub object: PathBuf,
    /// The name the document had when this version was written, when it is not
    /// the name it has now.
    pub was_named: Option<String>,
}

/// The one versions store, as everything that touches it holds it.
///
/// One store and one lock, not one store per user of it: whether this disk can
/// share blocks at all, and whether space has run out, are learned by *trying*
/// to write a version. A second store opened beside this one would answer both
/// questions with "not tested yet" for ever, and the screens that exist to say
/// "this computer does not keep earlier versions" would never say it.
pub type SharedHistory = std::sync::Arc<std::sync::Mutex<History>>;

/// The rows that actually cost space: every version except the newest of its
/// document, which shares the live file's blocks.
const NOT_THE_NEWEST: &str = "v.id <> (SELECT id FROM versions
                                        WHERE file_id = v.file_id
                                        ORDER BY saved_at DESC, id DESC LIMIT 1)";

/// A version [`History::prepare_version`] decided to keep, before its copy.
struct VersionToCopy {
    path: PathBuf,
    object: PathBuf,
    name: String,
    file_id: i64,
    metadata: std::fs::Metadata,
    reason: Reason,
}

fn not_kept_error(why: &str) -> anyhow::Error {
    anyhow::anyhow!("the current state was not kept: {why}")
}

/// The versions store.
pub struct History {
    conn: Option<Connection>,
    objects: PathBuf,
    support: Support,
    /// Devices already known not to share blocks. One refusal is enough: trying
    /// again per file on a network folder costs an `open` every time.
    hopeless_devices: Vec<u64>,
    budget_bytes: u64,
    keep_versions: u32,
    keep_days: u32,
    /// Set when space ran out. The screen says so instead of showing an empty
    /// list, and the next successful eviction clears it.
    paused_for_space: bool,
    /// A version was written since the budget was last enforced. Enforcing it
    /// sums the whole `versions` table, so it runs once per housekeeping turn
    /// rather than per version: per version, a folder of new documents cost a
    /// full-table pass each, quadratic in the size of the store.
    budget_unchecked: bool,
    skip_git_repositories: bool,
    respect_gitignore: bool,
}

impl History {
    /// Open the store, or a disabled one when the person turned it off.
    ///
    /// Never fails outward: a store that cannot be opened is a feature that does
    /// nothing, not a service that stops.
    pub fn open(settings: &big_search_config::FileHistory) -> Self {
        let disabled = |reason| Self {
            conn: None,
            objects: crate::config::history_dir().join("objects"),
            support: Support::Unavailable(reason),
            hopeless_devices: Vec::new(),
            budget_bytes: 0,
            keep_versions: settings.keep_versions,
            keep_days: settings.keep_days,
            paused_for_space: false,
            budget_unchecked: false,
            skip_git_repositories: settings.skip_git_repositories,
            respect_gitignore: settings.respect_gitignore,
        };
        if !settings.enabled {
            return disabled(Unavailable::Disabled);
        }
        if is_live_session() {
            return disabled(Unavailable::LiveSession);
        }
        let objects = crate::config::history_dir().join("objects");
        if let Err(error) = prepare_store(&objects) {
            log::warn!("file history disabled: {error:#}");
            return disabled(Unavailable::NoReflink);
        }
        let conn = match open_store(&crate::config::history_db_path()) {
            Ok(conn) => conn,
            Err(error) => {
                log::warn!("file history disabled: {error:#}");
                return disabled(Unavailable::NoReflink);
            }
        };
        let disk = disk_capacity(&crate::config::data_dir())
            .map(|capacity| capacity.total_bytes)
            .unwrap_or(0);
        Self {
            conn: Some(conn),
            objects,
            support: Support::Untested,
            hopeless_devices: Vec::new(),
            budget_bytes: big_search_config::history_budget_bytes(settings.max_total_gib, disk),
            keep_versions: settings.keep_versions,
            keep_days: settings.keep_days,
            paused_for_space: false,
            budget_unchecked: false,
            skip_git_repositories: settings.skip_git_repositories,
            respect_gitignore: settings.respect_gitignore,
        }
    }

    pub const fn support(&self) -> Support {
        self.support
    }

    pub const fn is_paused_for_space(&self) -> bool {
        self.paused_for_space
    }

    /// Throw away everything ever kept.
    ///
    /// Objects first, rows second: a `DELETE` needs metadata space too, and if
    /// this is running because the disk is full, the objects are what frees it.
    /// A crash in between leaves rows without objects, which the orphan sweep
    /// removes on the next start.
    ///
    /// A store turned off still holds what it kept before, so it is erased from
    /// disk: the database files and the whole objects directory.
    pub fn forget_everything(&mut self) -> Result<()> {
        let Some(conn) = &self.conn else {
            match std::fs::remove_dir_all(&self.objects) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(error)
                        .with_context(|| format!("remove {}", self.objects.display()));
                }
                _ => {}
            }
            let db = crate::config::history_db_path();
            for suffix in ["", "-wal", "-shm"] {
                let mut file = db.clone().into_os_string();
                file.push(suffix);
                match std::fs::remove_file(&file) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => {
                        return Err(error).with_context(|| format!("remove {}", db.display()));
                    }
                    _ => {}
                }
            }
            log::info!("every kept version was forgotten");
            return Ok(());
        };
        for name in &self.all_object_names()? {
            let _ = std::fs::remove_file(self.objects.join(name));
        }
        conn.execute("DELETE FROM versions", [])?;
        conn.execute("DELETE FROM files", [])?;
        conn.execute_batch("VACUUM")?;
        self.paused_for_space = false;
        log::info!("every kept version was forgotten");
        Ok(())
    }

    /// Forget the versions of one document, also from a store turned off.
    pub fn forget(&mut self, path: &Path) -> Result<()> {
        let opened;
        let conn = match &self.conn {
            Some(conn) => conn,
            None => {
                let db = crate::config::history_db_path();
                if !db.exists() {
                    return Ok(());
                }
                opened = open_store(&db)?;
                &opened
            }
        };
        let Some(file_id) = file_id_for_path(conn, path)? else {
            return Ok(());
        };
        let objects: Vec<String> = conn
            .prepare("SELECT object FROM versions WHERE file_id = ?1")?
            .query_map(params![file_id], |row| row.get::<_, String>(0))?
            .flatten()
            .collect();
        for name in &objects {
            let _ = std::fs::remove_file(self.objects.join(name));
        }
        conn.execute("DELETE FROM versions WHERE file_id = ?1", params![file_id])?;
        conn.execute("DELETE FROM files WHERE id = ?1", params![file_id])?;
        Ok(())
    }

    /// A document was renamed: keep its history under the new name.
    ///
    /// The watcher hears the rename as a pair — the kernel gives both names in
    /// one event — and that pair is the only reliable way to follow a document
    /// that is renamed and then saved: the save replaces the inode, so by the
    /// time anything looks, neither the old name nor the old identity is there
    /// to match on.
    pub fn note_rename(&self, from: &Path, to: &Path) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        // Only to a place the versions could be asked for again. A document
        // renamed into a hidden name is on its way out — that is exactly how a
        // file manager deletes for good, one rename to `.something-delete-...`
        // and then an unlink — and following it there would file the history
        // under a name nobody can type, out of reach of the request to forget it.
        if eligible_name(to).is_none() {
            return Ok(());
        }
        follow_rename(conn, from, to)
    }

    /// The **earlier** versions of one document, newest first.
    ///
    /// The state the document is in right now is left out, and that is the
    /// difference between a useful list and a confusing one: the newest version
    /// is normally a clone taken seconds after the last save, so it is the
    /// document itself. Offering "go back to this version" for the state you are
    /// already in is an action that does nothing, sitting at the top of the list
    /// where the eye lands first.
    pub fn versions(&self, path: &Path) -> Result<Vec<Version>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        let Some(file_id) = file_id_for_path(conn, path)? else {
            return Ok(Vec::new());
        };
        let live = std::fs::metadata(path).ok();
        let mut statement = conn.prepare(
            "SELECT id, saved_at, size, reason, object, was_named, mtime_s, mtime_ns
               FROM versions WHERE file_id = ?1 ORDER BY saved_at DESC, id DESC",
        )?;
        let rows = statement.query_map(params![file_id], |row| {
            Ok((
                Version {
                    id: row.get(0)?,
                    saved_at: row.get(1)?,
                    size: row.get::<_, i64>(2)?.max(0) as u64,
                    reason: Reason::parse(&row.get::<_, String>(3)?),
                    object: self.objects.join(row.get::<_, String>(4)?),
                    was_named: row.get(5)?,
                },
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?;
        Ok(rows
            .flatten()
            .filter(|(version, mtime_s, mtime_ns)| {
                !live.as_ref().is_some_and(|metadata| {
                    *mtime_s == metadata.mtime()
                        && *mtime_ns == metadata.mtime_nsec()
                        && version.size == metadata.len()
                })
            })
            .map(|(version, _, _)| version)
            .collect())
    }

    /// Keep the state `path` is in now, if it earns a version.
    ///
    /// `Ok(false)` is the ordinary answer for a file that is not a document, has
    /// not changed, or lives on a disk that cannot share blocks.
    ///
    /// Before a restore, a state that exists but could not be kept is an error:
    /// the caller is about to overwrite it, and `Ok` would let it.
    pub fn save_version(&mut self, path: &Path, reason: Reason) -> Result<bool> {
        let Some(version) = self.prepare_version(path, reason)? else {
            return Ok(false);
        };
        let copied = clone_file(path, &version.object);
        self.finish_version(version, copied)
    }

    /// Everything before the copy: whether `path` earns a version now, and the
    /// object it will be copied to. `None` when there is nothing to copy.
    ///
    /// Split from the copy so the idle-priority worker can copy without
    /// holding the store's lock, which a file manager's request may be
    /// waiting on.
    fn prepare_version(&mut self, path: &Path, reason: Reason) -> Result<Option<VersionToCopy>> {
        let not_kept = |why: &str| {
            if reason == Reason::BeforeRestore {
                Err(not_kept_error(why))
            } else {
                Ok(None)
            }
        };
        let Some(metadata) = eligible_metadata(path) else {
            return Ok(None);
        };
        if self.conn.is_none() {
            return not_kept("versions are turned off");
        }
        // A restore still keeps its undo: the person is putting back a version
        // this store already holds, whatever the setting says now.
        if reason != Reason::BeforeRestore && self.left_to_git(path) {
            return Ok(None);
        }
        if self.hopeless_devices.contains(&metadata.dev()) {
            return not_kept("this disk cannot share blocks");
        }
        // The baseline is allowed even when space is short: it shares the live
        // file's blocks and costs no data, and it is what leaves somebody with
        // at least one version on a disk that was already full when they
        // started.
        if reason != Reason::Baseline && self.paused_for_space {
            return not_kept("the disk is short of space");
        }
        let file_id = self.upsert_file(path, &metadata)?;
        if self.matches_newest_version(file_id, &metadata)? {
            // The state is already kept, so there is nothing to clone — but when
            // the file manager is about to write an older version over this one,
            // *this* row is the undo, and thinning would drop it as an ordinary
            // save minutes later. Marking it is what keeps the promise the
            // confirmation dialog makes.
            if reason == Reason::BeforeRestore {
                self.mark_newest_as_the_undo(file_id)?;
            }
            return Ok(None);
        }
        let name = object_name();
        Ok(Some(VersionToCopy {
            object: self.objects.join(&name),
            name,
            path: path.to_path_buf(),
            file_id,
            metadata,
            reason,
        }))
    }

    /// Record a version once its copy was attempted.
    fn finish_version(
        &mut self,
        version: VersionToCopy,
        copied: std::result::Result<(), Refusal>,
    ) -> Result<bool> {
        if let Err(refusal) = copied {
            self.note_refusal(&version.metadata, refusal);
            return if version.reason == Reason::BeforeRestore {
                Err(not_kept_error("the copy was refused"))
            } else {
                Ok(false)
            };
        }
        if self.support == Support::Untested {
            self.support = Support::Ready;
        }
        self.record_version(
            version.file_id,
            &version.path,
            &version.metadata,
            &version.name,
            version.reason,
        )?;
        self.prune_file(version.file_id)?;
        self.budget_unchecked = true;
        Ok(true)
    }

    /// Whether git, not this store, looks after `path`, as far as the settings
    /// say to leave it there.
    fn left_to_git(&self, path: &Path) -> bool {
        if !self.skip_git_repositories && !self.respect_gitignore {
            return false;
        }
        let Some(work_tree) = git_work_tree(path) else {
            return false;
        };
        self.skip_git_repositories || gitignored(work_tree, path)
    }

    /// Call the newest kept version what it is about to become: the way back.
    fn mark_newest_as_the_undo(&self, file_id: i64) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        conn.execute(
            "UPDATE versions SET reason = ?2
              WHERE id = (SELECT id FROM versions WHERE file_id = ?1
                           ORDER BY saved_at DESC, id DESC LIMIT 1)",
            params![file_id, Reason::BeforeRestore.as_str()],
        )?;
        Ok(())
    }

    /// Whether this document is already kept as it stands.
    fn matches_newest_version(&self, file_id: i64, metadata: &std::fs::Metadata) -> Result<bool> {
        let Some(conn) = &self.conn else {
            return Ok(true);
        };
        let newest: Option<(i64, i64, i64)> = conn
            .query_row(
                "SELECT mtime_s, mtime_ns, size FROM versions
                  WHERE file_id = ?1 ORDER BY saved_at DESC, id DESC LIMIT 1",
                params![file_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        // Nanoseconds, not the seconds the search index keeps: two saves inside
        // one second are one save to `state::meta_pair`, and here they are two.
        Ok(newest.is_some_and(|(mtime_s, mtime_ns, size)| {
            mtime_s == metadata.mtime()
                && mtime_ns == metadata.mtime_nsec()
                && size == metadata.len() as i64
        }))
    }

    /// The catalogue entries no version holds yet: a document whose newest
    /// version has the catalogue's `(mtime, size)` is left out. Seconds, as the
    /// catalogue keeps them: a change at the same size within the second of the
    /// last version waits for the document's next event.
    pub fn unkept(&self, entries: Vec<(PathBuf, i64, u64)>) -> Vec<PathBuf> {
        let Some(conn) = &self.conn else {
            return Vec::new();
        };
        // One read transaction for the page rather than one per document.
        let read = conn.unchecked_transaction();
        let newest = conn.prepare_cached(
            "SELECT v.mtime_s, v.size FROM files f JOIN versions v ON v.file_id = f.id
              WHERE f.current_path = ?1 ORDER BY v.saved_at DESC, v.id DESC LIMIT 1",
        );
        let (Ok(_read), Ok(mut newest)) = (read, newest) else {
            return entries.into_iter().map(|(path, _, _)| path).collect();
        };
        entries
            .into_iter()
            .filter(|(path, mtime, size)| {
                let kept = newest
                    .query_row(params![path.to_string_lossy()], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                    })
                    .optional()
                    .ok()
                    .flatten();
                kept != Some((*mtime, i64::try_from(*size).unwrap_or(i64::MAX)))
            })
            .map(|(path, _, _)| path)
            .collect()
    }

    /// Find or create the catalogue row for this document.
    ///
    /// Two rules, in order. The path comes first because that is what an atomic
    /// save looks like: same name, brand-new inode. Identity comes second, and
    /// only when the old name is gone — otherwise two hard links to one inode
    /// would take turns renaming each other's history. Identity includes the
    /// birth time where the filesystem records one: XFS hands a deleted
    /// document's inode number to the next new file, which must not inherit
    /// that document's versions. A row without one (written before it was
    /// recorded, or on a filesystem without it) matches on `(dev, ino)` alone.
    fn upsert_file(&self, path: &Path, metadata: &std::fs::Metadata) -> Result<i64> {
        let conn = self.conn.as_ref().expect("checked by the caller");
        let key = path.to_string_lossy().into_owned();
        let born = metadata
            .created()
            .ok()
            .and_then(|created| created.duration_since(UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_nanos()).ok());
        let (dev, ino) = (metadata.dev() as i64, metadata.ino() as i64);
        if let Some(id) = file_id_for_path(conn, path)? {
            // Only on a change: an UPDATE that rewrites the same values still
            // writes the page to the WAL, and every save passes through here.
            conn.execute(
                "UPDATE files SET dev = ?2, ino = ?3, born = ?4, missing_since = NULL
                  WHERE id = ?1 AND (dev IS NOT ?2 OR ino IS NOT ?3 OR born IS NOT ?4
                                     OR missing_since IS NOT NULL)",
                params![id, dev, ino, born],
            )?;
            return Ok(id);
        }
        let renamed: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, current_path FROM files
                  WHERE dev = ?1 AND ino = ?2 AND (born IS NULL OR ?3 IS NULL OR born = ?3)
                  ORDER BY created_at DESC LIMIT 1",
                params![dev, ino, born],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((id, previous)) = renamed
            && !Path::new(&previous).exists()
        {
            conn.execute(
                "UPDATE files SET current_path = ?2, born = ?3, missing_since = NULL WHERE id = ?1",
                params![id, key, born],
            )?;
            return Ok(id);
        }
        conn.execute(
            "INSERT INTO files(current_path, dev, ino, born, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key, dev, ino, born, now_secs()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn record_version(
        &self,
        file_id: i64,
        path: &Path,
        metadata: &std::fs::Metadata,
        object: &str,
        reason: Reason,
    ) -> Result<()> {
        let conn = self.conn.as_ref().expect("checked by the caller");
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        conn.execute(
            "INSERT INTO versions(file_id, saved_at, mtime_s, mtime_ns, size, object, reason, was_named)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                file_id,
                now_secs(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.len() as i64,
                object,
                reason.as_str(),
                name,
            ],
        )?;
        Ok(())
    }

    /// A refusal by the filesystem, filed against whatever it is really about.
    fn note_refusal(&mut self, metadata: &std::fs::Metadata, refusal: Refusal) {
        match refusal {
            // The whole device cannot do this. One answer for every file on it.
            Refusal::Device(reason) => {
                self.hopeless_devices.push(metadata.dev());
                if self.support == Support::Untested {
                    self.support = Support::Unavailable(reason);
                    log::info!("file history is not available here: {}", reason.as_str());
                }
            }
            // This one file cannot be cloned — `nodatacow`, an immutable flag,
            // a file being executed. Everything else on the disk still can.
            Refusal::File => {}
            Refusal::OutOfSpace => self.paused_for_space = true,
        }
    }

    /// Thin one document's versions by time, then by count.
    ///
    /// Ten presses of Ctrl+S in five minutes are ten nearly identical versions,
    /// and keeping "the newest ten" would throw away yesterday's — the one that
    /// mattered. So: the last hour keeps one, today keeps one per hour, this
    /// week one per day, then one per week. The count is a ceiling that is
    /// rarely reached.
    fn prune_file(&self, file_id: i64) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let mut kept: Vec<(i64, String)> = Vec::new();
        let mut dropped: Vec<(i64, String)> = Vec::new();
        let mut seen_buckets: Vec<i64> = Vec::new();
        let now = now_secs();
        let oldest_allowed = now - i64::from(self.keep_days) * 24 * 60 * 60;

        let mut statement = conn.prepare(
            "SELECT id, saved_at, object, reason FROM versions
              WHERE file_id = ?1 ORDER BY saved_at DESC, id DESC",
        )?;
        let rows: Vec<(i64, i64, String, String)> = statement
            .query_map(params![file_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .flatten()
            .collect();
        let oldest_index = rows.len().saturating_sub(1);

        for (index, (id, saved_at, object, reason)) in rows.into_iter().enumerate() {
            // The newest is never dropped, whatever its age: a document nobody
            // has touched in a year still deserves the one version it has.
            let newest = index == 0;
            // Nor is the state the document was in before this feature ever saw
            // it. Thinning by time would drop it the moment somebody saves, and
            // it is the only copy of what they started from — "como estava antes
            // de eu mexer" is the whole point. Age still retires it.
            //
            // **The oldest row, not the one marked `baseline`.** A document
            // created while the service is watching earns its first version from
            // the watcher, which calls that a save; keying the protection on the
            // reason lost the original of every file born after the feature was
            // installed, which is most of them.
            let first_ever = index == oldest_index;
            // And neither is the state a document was in just before somebody
            // asked to go back to an older one. The dialog that asks promises
            // "o jeito que ele está agora também fica guardado, para você poder
            // desfazer", and hourly thinning would quietly break that promise:
            // going back and undoing it happen minutes apart, inside one slot.
            let is_the_undo = Reason::parse(&reason) == Reason::BeforeRestore;
            let bucket = time_bucket(now, saved_at);
            let too_old = saved_at < oldest_allowed;
            let too_many = kept.len() >= self.keep_versions as usize;
            if newest
                || (!too_old
                    && (first_ever
                        || is_the_undo
                        || (!too_many && !seen_buckets.contains(&bucket))))
            {
                seen_buckets.push(bucket);
                kept.push((id, object));
            } else {
                dropped.push((id, object));
            }
        }
        self.drop_versions(&dropped)
    }

    /// Versions to drop because one document holds more than its share of the
    /// budget: all but its three newest. The first pass of keeping the store
    /// inside its budget; [`Self::over_budget`] is the second.
    fn budget_hogs(&self) -> Result<Vec<(i64, String)>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        if self.budget_bytes == 0 {
            return Ok(Vec::new());
        }
        let share = self.budget_bytes / 100 * MAX_BUDGET_SHARE_PERCENT;
        // Same accounting as `total_bytes`: what a document costs is what it
        // holds beyond its newest version.
        let hogs: Vec<i64> = conn
            .prepare(&format!(
                "SELECT v.file_id FROM versions v
                  WHERE {NOT_THE_NEWEST}
                  GROUP BY v.file_id HAVING SUM(v.size) > ?1"
            ))?
            .query_map(params![share as i64], |row| row.get::<_, i64>(0))?
            .flatten()
            .collect();
        let mut dropped = Vec::new();
        for file_id in hogs {
            dropped.extend(
                conn.prepare(
                    "SELECT id, object FROM versions WHERE file_id = ?1
                      ORDER BY saved_at DESC, id DESC LIMIT -1 OFFSET 3",
                )?
                .query_map(params![file_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .flatten(),
            );
        }
        Ok(dropped)
    }

    /// The oldest versions to drop so the store fits its budget again.
    fn over_budget(&self) -> Result<Vec<(i64, String)>> {
        if self.conn.is_none() || self.budget_bytes == 0 {
            return Ok(Vec::new());
        }
        let total = self.total_bytes()?;
        if total <= self.budget_bytes {
            return Ok(Vec::new());
        }
        self.oldest_versions_totalling(total - self.budget_bytes)
    }

    /// Throw away objects nobody claims and rows whose object is gone.
    ///
    /// A crash between the clone and the row leaves a file holding disk space
    /// that nothing will ever free. Twenty lines here are what make the store
    /// honest about being durable data.
    pub fn sweep_orphans(&mut self) -> Result<()> {
        if self.conn.is_none() {
            return Ok(());
        }
        // A set, not a list: the walk below asks "is this object known" once per
        // file on disk, and a linear scan of ten thousand names per file is the
        // difference between a tidy-up and a stall at every start.
        let known: std::collections::HashSet<String> =
            self.all_object_names()?.into_iter().collect();
        let missing_rows: Vec<&String> = known
            .iter()
            .filter(|name| !self.objects.join(name).exists())
            .collect();
        if let Some(conn) = &self.conn {
            for name in &missing_rows {
                conn.execute("DELETE FROM versions WHERE object = ?1", params![name])?;
            }
        }
        let Ok(entries) = std::fs::read_dir(&self.objects) else {
            return Ok(());
        };
        for shard in entries.flatten() {
            let Ok(files) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            for object in files.flatten() {
                let name = format!(
                    "{}/{}",
                    shard.file_name().to_string_lossy(),
                    object.file_name().to_string_lossy()
                );
                if !known.contains(name.as_str()) {
                    let _ = std::fs::remove_file(object.path());
                }
            }
        }
        Ok(())
    }

    fn all_object_names(&self) -> Result<Vec<String>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        Ok(conn
            .prepare("SELECT object FROM versions")?
            .query_map([], |row| row.get::<_, String>(0))?
            .flatten()
            .collect())
    }

    /// What the store actually costs, in bytes.
    ///
    /// **The newest version of each document is not counted.** A clone shares
    /// the live file's blocks, so the newest version costs nothing until the
    /// document is saved again; what costs is every version that has been
    /// superseded. Counting the whole sum would have the budget evicting
    /// versions that are free — on this machine, four thousand versions whose
    /// real cost is zero.
    fn total_bytes(&self) -> Result<u64> {
        let Some(conn) = &self.conn else {
            return Ok(0);
        };
        let total: i64 = conn.query_row(
            &format!("SELECT COALESCE(SUM(size), 0) FROM versions v WHERE {NOT_THE_NEWEST}"),
            [],
            |row| row.get(0),
        )?;
        Ok(total.max(0) as u64)
    }

    /// The oldest superseded versions, enough of them to cover `bytes`.
    ///
    /// One query and one walk instead of "take the oldest, measure everything
    /// again, repeat": the second shape ran a full sum of the table per version
    /// evicted.
    fn oldest_versions_totalling(&self, bytes: u64) -> Result<Vec<(i64, String)>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(&format!(
            "SELECT v.id, v.object, v.size FROM versions v
              WHERE {NOT_THE_NEWEST} ORDER BY v.saved_at ASC"
        ))?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut chosen = Vec::new();
        let mut covered = 0u64;
        for row in rows.flatten() {
            let (id, object, size) = row;
            chosen.push((id, object));
            covered = covered.saturating_add(size.max(0) as u64);
            if covered >= bytes {
                break;
            }
        }
        Ok(chosen)
    }

    /// Remove versions: the objects first, then the rows.
    fn drop_versions(&self, versions: &[(i64, String)]) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        for (id, object) in versions {
            let _ = std::fs::remove_file(self.objects.join(object));
            conn.execute("DELETE FROM versions WHERE id = ?1", params![id])?;
        }
        Ok(())
    }
}

/// Which coarse slot in time a version falls in, so only one is kept per slot.
///
/// Hours today, days this week, weeks after that. Older versions are worth
/// keeping further apart: nobody wants the eleven o'clock and the noon copies of
/// something they wrote three weeks ago.
fn time_bucket(now: i64, saved_at: i64) -> i64 {
    const HOUR: i64 = 3600;
    const DAY: i64 = 24 * HOUR;
    let age = (now - saved_at).max(0);
    if age < HOUR {
        0
    } else if age < DAY {
        1 + saved_at / HOUR
    } else if age < 7 * DAY {
        1_000_000 + saved_at / DAY
    } else {
        2_000_000 + saved_at / (7 * DAY)
    }
}

/// Why a clone was refused, and what the refusal is about.
pub(crate) enum Refusal {
    /// Nothing on this device can be cloned.
    Device(Unavailable),
    /// This one file cannot.
    File,
    /// No space, or over quota.
    OutOfSpace,
}

/// Space on the filesystem holding `path`, from `statvfs(2)`.
struct DiskCapacity {
    /// What an unprivileged process may still write (`f_bavail`).
    available_bytes: u64,
    total_bytes: u64,
}

fn disk_capacity(path: &Path) -> io::Result<DiskCapacity> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `stats` is a correctly sized output buffer and `c_path` a valid
    // NUL-terminated string for the duration of the call.
    let stats = unsafe {
        let mut stats = std::mem::zeroed::<libc::statvfs>();
        if libc::statvfs(c_path.as_ptr(), &raw mut stats) != 0 {
            return Err(io::Error::last_os_error());
        }
        stats
    };
    // POSIX counts blocks in fragments; a filesystem reporting none uses f_bsize.
    let fragment = if stats.f_frsize > 0 {
        stats.f_frsize
    } else {
        stats.f_bsize
    };
    Ok(DiskCapacity {
        available_bytes: stats.f_bavail.saturating_mul(fragment),
        total_bytes: stats.f_blocks.saturating_mul(fragment),
    })
}

/// Clone `source` into `destination`, sharing its blocks.
///
/// The destination is created exclusively and read-only: a version somebody can
/// save over is not a version. Nothing here ever copies bytes — a refusal is a
/// refusal.
pub(crate) fn clone_file(source: &Path, destination: &Path) -> std::result::Result<(), Refusal> {
    let before = std::fs::symlink_metadata(source).map_err(|_| Refusal::File)?;
    // `O_NOFOLLOW`: a link is not a document, and following one would keep a
    // version of whatever it points at, filters and all.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)
        .map_err(|_| Refusal::File)?;
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| classify(&error))?;
    }
    let clone = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(destination)
        .map_err(|error| classify(&error))?;

    // SAFETY: FICLONE takes the source descriptor as its argument and writes
    // nothing into user memory. Both descriptors are open and owned here.
    let outcome = unsafe { libc::ioctl(clone.as_raw_fd(), libc::FICLONE, file.as_raw_fd()) };
    if outcome != 0 {
        let error = io::Error::last_os_error();
        drop(clone);
        let _ = std::fs::remove_file(destination);
        return Err(classify(&error));
    }
    drop(clone);

    // The file may have been saved again while this ran. `FICLONE` itself is
    // atomic against concurrent writes, but a version captured in the middle of
    // a sequence of saves is a version of a moment nobody meant to keep.
    let after = std::fs::symlink_metadata(source).map_err(|_| Refusal::File)?;
    if after.dev() != before.dev()
        || after.ino() != before.ino()
        || after.len() != before.len()
        || after.mtime_nsec() != before.mtime_nsec()
        || after.mtime() != before.mtime()
    {
        let _ = std::fs::remove_file(destination);
        return Err(Refusal::File);
    }
    Ok(())
}

/// What an error from the kernel is really saying.
fn classify(error: &io::Error) -> Refusal {
    match error.raw_os_error() {
        // The filesystem cannot share blocks at all.
        Some(libc::EOPNOTSUPP) | Some(libc::ENOTTY) => Refusal::Device(Unavailable::NoReflink),
        // Sharing across mount points, which is what this service always does,
        // is only allowed from Linux 5.18.
        Some(libc::EXDEV) => Refusal::Device(Unavailable::OldKernel),
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => Refusal::OutOfSpace,
        // `EINVAL` is what a file with copy-on-write turned off answers. The
        // disk is fine; this file is not.
        _ => Refusal::File,
    }
}

/// Whether this document earns a version, and its metadata if so.
///
/// Cheapest test first. The allow list is deliberately an allow list: a block
/// list is how somebody discovers the problem with a 100 GB file.
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
/// disk — so it can also answer about a path that no longer exists.
fn eligible_name(path: &Path) -> Option<()> {
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

/// The nearest folder above `path` that holds a `.git` — a directory in an
/// ordinary clone, a file in a linked worktree or a submodule.
///
/// The walk stops at the catalogued folder that owns `path`: a `.git` above it
/// belongs to something the person did not ask to have searched.
fn git_work_tree(path: &Path) -> Option<&Path> {
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
fn gitignored(work_tree: &Path, path: &Path) -> bool {
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

/// Whether this is a live session, where nothing survives the reboot.
fn is_live_session() -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    mounts.lines().any(|line| {
        let mount_point = line.split(' ').nth(4);
        mount_point == Some("/") && line.contains(" overlay ")
    })
}

/// The store's own directory, with the marker that keeps it out of backups.
///
/// `CACHEDIR.TAG` is what `rsync --exclude-caches`, borg and Deja Dup read.
/// Without it every backup would carry ten versions of every document.
fn prepare_store(objects: &Path) -> Result<()> {
    std::fs::create_dir_all(objects).with_context(|| format!("create {}", objects.display()))?;
    let tag = crate::config::history_dir().join("CACHEDIR.TAG");
    if !tag.exists() {
        std::fs::write(
            &tag,
            "Signature: 8a477f597d28d172789f06886806bc55\n\
             # Earlier versions of this computer's documents, kept by big-search.\n\
             # They are not a backup: leave them out of one.\n",
        )
        .with_context(|| format!("write {}", tag.display()))?;
    }
    Ok(())
}

fn open_store(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(1))?;
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            current_path TEXT NOT NULL UNIQUE,
            dev INTEGER,
            ino INTEGER,
            created_at INTEGER NOT NULL,
            missing_since INTEGER,
            born INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_files_dev_ino ON files(dev, ino);
        CREATE TABLE IF NOT EXISTS versions (
            id INTEGER PRIMARY KEY,
            file_id INTEGER NOT NULL REFERENCES files(id),
            saved_at INTEGER NOT NULL,
            mtime_s INTEGER NOT NULL,
            mtime_ns INTEGER NOT NULL,
            size INTEGER NOT NULL,
            object TEXT NOT NULL,
            reason TEXT NOT NULL,
            was_named TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_versions_file ON versions(file_id, saved_at DESC);
        CREATE INDEX IF NOT EXISTS idx_versions_age ON versions(saved_at);
        ",
    )?;
    // Stores created before the birth time was recorded.
    let has_born: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('files') WHERE name = 'born')",
        [],
        |row| row.get(0),
    )?;
    if !has_born {
        conn.execute_batch("ALTER TABLE files ADD COLUMN born INTEGER")?;
    }
    Ok(conn)
}

/// Forget documents whose file has been gone long enough.
///
/// The store is locked to read the catalogue and to write the outcome, not
/// while the filesystem is asked about every document: the worker runs at idle
/// priority, and a file manager's request for a version waits on the same lock.
/// A row renamed or saved in between no longer matches what was checked and is
/// left for the next sweep.
fn expire_missing(history: &SharedHistory) -> Result<()> {
    let files: Vec<(i64, String, Option<i64>)> = {
        let store = history.lock().expect("versions store lock");
        let Some(conn) = &store.conn else {
            return Ok(());
        };
        conn.prepare("SELECT id, current_path, missing_since FROM files")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let checked: Vec<_> = files
        .into_iter()
        .map(|(id, path, missing_since)| {
            let exists = Path::new(&path).exists();
            (id, path, missing_since, exists)
        })
        .collect();

    let now = now_secs();
    let mut store = history.lock().expect("versions store lock");
    let Some(conn) = &store.conn else {
        return Ok(());
    };
    let mut expired: Vec<String> = Vec::new();
    for (id, path, missing_since, exists) in checked {
        match (exists, missing_since) {
            (true, Some(_)) => {
                conn.execute(
                    "UPDATE files SET missing_since = NULL WHERE id = ?1 AND current_path = ?2",
                    params![id, path],
                )?;
            }
            (false, None) => {
                conn.execute(
                    "UPDATE files SET missing_since = ?3
                      WHERE id = ?1 AND current_path = ?2 AND missing_since IS NULL",
                    params![id, path, now],
                )?;
            }
            (false, Some(since)) if now - since > MISSING_GRACE_SECONDS => {
                let unchanged: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM files
                          WHERE id = ?1 AND current_path = ?2 AND missing_since = ?3",
                        params![id, path, since],
                        |row| row.get(0),
                    )
                    .optional()?;
                if unchanged.is_some() {
                    expired.push(path);
                }
            }
            _ => {}
        }
    }
    for path in expired {
        store.forget(Path::new(&path))?;
    }
    Ok(())
}

/// Move `from`'s catalogue row to `to`, in one transaction.
///
/// When `to` has a row of its own (`mv a.md b.md` over a document with
/// history), its versions join `from`'s: they hold what the rename overwrote.
/// Left alone, the unique path would refuse the move, and `from`'s row would
/// expire with every version in it.
fn follow_rename(conn: &Connection, from: &Path, to: &Path) -> Result<()> {
    let Some(from_id) = file_id_for_path(conn, from)? else {
        return Ok(());
    };
    let transaction = conn.unchecked_transaction()?;
    if let Some(to_id) = file_id_for_path(conn, to)?
        && to_id != from_id
    {
        conn.execute(
            "UPDATE versions SET file_id = ?1 WHERE file_id = ?2",
            params![from_id, to_id],
        )?;
        conn.execute("DELETE FROM files WHERE id = ?1", params![to_id])?;
    }
    conn.execute(
        "UPDATE files SET current_path = ?2, missing_since = NULL WHERE id = ?1",
        params![from_id, to.to_string_lossy()],
    )?;
    transaction.commit()?;
    Ok(())
}

/// The row for `path`. Rows are keyed by UTF-8 text, so a path that is not
/// UTF-8 has none: a lossy key would name a different file.
fn file_id_for_path(conn: &Connection, path: &Path) -> Result<Option<i64>> {
    let Some(key) = path.to_str() else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT id FROM files WHERE current_path = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?)
}

/// A name for one object: two hex characters of directory, then the rest.
///
/// Named after nothing in the document, because the document will be renamed and
/// the object must not have to follow.
fn object_name() -> String {
    let mut bytes = [0u8; 16];
    // SAFETY: `getrandom` writes at most `len` bytes into the buffer given.
    let filled =
        unsafe { libc::getrandom(bytes.as_mut_ptr().cast::<libc::c_void>(), bytes.len(), 0) };
    if filled != bytes.len() as isize {
        let now = now_secs().to_le_bytes();
        bytes[..8].copy_from_slice(&now);
    }
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}/{}", &hex[..2], &hex[2..])
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// What the worker is waiting to save, and until when.
pub struct Pending {
    deadlines: HashMap<PathBuf, std::time::Instant>,
    limit: usize,
}

impl Pending {
    /// Most files waiting at once. A bound, because a machine with little memory
    /// must not grow a map the size of an unpacked archive.
    const LIMIT: usize = 4096;

    pub fn new() -> Self {
        Self {
            deadlines: HashMap::new(),
            limit: Self::LIMIT,
        }
    }

    /// Note a change, pushing this file's deadline forward.
    ///
    /// Pushing forward rather than keeping the first is what stops an editor's
    /// autosave from becoming a hundred versions: the version is written once
    /// the file has been quiet.
    pub fn note(&mut self, path: PathBuf, wait: std::time::Duration) {
        if self.deadlines.len() >= self.limit && !self.deadlines.contains_key(&path) {
            return;
        }
        self.deadlines
            .insert(path, std::time::Instant::now() + wait);
    }

    /// Ready paths, oldest first, removed only when consumed by the worker.
    ///
    /// A time-limited batch can stop anywhere: dropping its iterator must leave
    /// every unvisited path and its original deadline queued for the next turn.
    pub fn due(&mut self) -> impl Iterator<Item = PathBuf> + '_ {
        let now = std::time::Instant::now();
        let mut due: Vec<_> = self
            .deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(path, deadline)| (path.clone(), *deadline))
            .collect();
        due.sort_unstable_by(|(left_path, left_deadline), (right_path, right_deadline)| {
            left_deadline
                .cmp(right_deadline)
                .then_with(|| left_path.cmp(right_path))
        });
        due.into_iter().map(|(path, _)| {
            self.deadlines.remove(&path);
            path
        })
    }

    pub fn is_empty(&self) -> bool {
        self.deadlines.is_empty()
    }
}

impl Default for Pending {
    fn default() -> Self {
        Self::new()
    }
}

/// What the watcher tells the versions worker.
pub enum HistoryEvent {
    /// This document may have just been saved.
    Changed(PathBuf),
    /// The kernel reported both names of a rename in one event.
    Renamed(PathBuf, PathBuf),
    /// A page of the catalogue's documents with the `(mtime, size)` it holds,
    /// for the pass that covers what no event did.
    Catalogued(Vec<(PathBuf, i64, u64)>),
}

/// How long a document has to be quiet before its version is written.
///
/// Long enough that an editor's autosave, or a program that writes a file three
/// times in a row, produces one version rather than three; short enough that
/// somebody who saves and immediately looks finds it there.
const QUIET_PERIOD: std::time::Duration = std::time::Duration::from_secs(3);
/// How often, at most, the worker looks at the disk and at documents that
/// disappeared. Only after something happened: with nothing saved or changed
/// there is nothing new to find, and the worker sleeps until an event.
const HOUSEKEEPING: std::time::Duration = std::time::Duration::from_secs(60);
/// How often the store checks whether documents it knows are still there.
///
/// Once an hour, not once a minute: it asks the filesystem about every document
/// it keeps versions of, and on a machine with twenty thousand of them that is
/// twenty thousand questions. What it is looking for — a file gone for a week —
/// does not move in a minute.
const MISSING_SWEEP: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// How long the worker rests between bursts while it still has files waiting.
///
/// Twice the burst, so about a third of the time is spent working. The service
/// already runs at the lowest priority the kernel offers; this is what keeps it
/// from feeling busy on a slow machine even so.
const BACKLOG_PAUSE: std::time::Duration = std::time::Duration::from_millis(400);

/// How long the worker spends saving versions before yielding.
///
/// A budget in time, not in files: cloning charges by extent, so one fragmented
/// document can cost more than ten small ones, and on a slow disk a count would
/// be a promise the machine cannot keep.
const WORK_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

/// Run the versions worker until the sender goes away.
///
/// Its own thread on purpose: the search must never wait for a version, and a
/// failure here must never be able to stop the watcher.
/// Versions dropped per hold of the store's lock during housekeeping. The
/// worker runs at idle priority and a file manager's request may be waiting on
/// the lock, so it waits for one batch at most.
const DROP_BATCH: usize = 16;

/// Drop `versions`, objects first, a batch per hold of the lock.
fn drop_in_batches(history: &SharedHistory, versions: &[(i64, String)]) -> Result<()> {
    for batch in versions.chunks(DROP_BATCH) {
        history
            .lock()
            .expect("versions store lock")
            .drop_versions(batch)?;
    }
    Ok(())
}

/// Keep the whole store inside its budget.
///
/// Two passes, because size and age answer different questions: first no
/// document may hold more than a tenth of the budget, then the oldest go.
fn enforce_budget(history: &SharedHistory) -> Result<()> {
    let hogs = history.lock().expect("versions store lock").budget_hogs()?;
    drop_in_batches(history, &hogs)?;
    let oldest = history.lock().expect("versions store lock").over_budget()?;
    drop_in_batches(history, &oldest)
}

/// Stop when the disk is nearly full, and give space back until it is not.
///
/// Stopping alone would not be enough. When the disk fills for some other
/// reason, the person deletes files and the space does not come back,
/// because these versions are still holding the old blocks, and they have
/// no idea this store exists.
fn watch_free_space(history: &SharedHistory) -> Result<()> {
    if history.lock().expect("versions store lock").conn.is_none() {
        return Ok(());
    }
    let Ok(capacity) = disk_capacity(&crate::config::data_dir()) else {
        return Ok(());
    };
    let (paused, cost) = {
        let store = history.lock().expect("versions store lock");
        (store.paused_for_space, store.total_bytes()?)
    };
    // Once paused, the store waits for the recovered target, not the threshold.
    let resume_at = if paused {
        recovered_disk_target(cost)
    } else {
        low_disk_threshold(cost)
    };
    if capacity.available_bytes >= resume_at {
        history
            .lock()
            .expect("versions store lock")
            .paused_for_space = false;
        return Ok(());
    }
    history
        .lock()
        .expect("versions store lock")
        .paused_for_space = true;
    log::info!("little disk space left: giving kept versions back");
    let mut available = capacity.available_bytes;
    let mut target = recovered_disk_target(cost);
    while available < target {
        // Asked for the shortfall, not for one version: but the disk is
        // measured again after each batch, because a version only frees the
        // blocks it does *not* share with the live file. Its size is a
        // ceiling on what comes back, never the amount.
        let dropped = history
            .lock()
            .expect("versions store lock")
            .oldest_versions_totalling(target - available)?;
        if dropped.is_empty() {
            break;
        }
        drop_in_batches(history, &dropped)?;
        let Ok(now) = disk_capacity(&crate::config::data_dir()) else {
            break;
        };
        if now.available_bytes <= available {
            break; // nothing came back; stop rather than spin
        }
        available = now.available_bytes;
        // Fewer versions lower what they need left free.
        target = recovered_disk_target(history.lock().expect("versions store lock").total_bytes()?);
    }
    if available >= target {
        history
            .lock()
            .expect("versions store lock")
            .paused_for_space = false;
    }
    Ok(())
}

pub fn run_worker(history: &SharedHistory, events: std::sync::mpsc::Receiver<HistoryEvent>) {
    crate::throttle::lower_priority();
    {
        let mut store = history.lock().expect("versions store lock");
        if let Support::Unavailable(reason) = store.support()
            && reason != Unavailable::Disabled
        {
            log::info!("earlier versions are not kept here: {}", reason.as_str());
        }
        if let Err(error) = store.sweep_orphans() {
            log::warn!("could not tidy the versions store: {error:#}");
        }
    }
    let mut pending = Pending::new();
    let mut next_housekeeping = std::time::Instant::now() + HOUSEKEEPING;
    let mut next_missing_sweep = std::time::Instant::now() + MISSING_SWEEP;
    // The disk and documents gone missing are looked at once after a start,
    // then after each event or saved version.
    let mut housekeeping_owed = true;

    loop {
        // With a backlog, come back quickly; with none, sleep until there is
        // something to do. The first version of every document is written this
        // way, and a worker that waited out the whole quiet period between
        // batches would take hours over what should take minutes.
        let received = if !pending.is_empty() {
            events.recv_timeout(BACKLOG_PAUSE)
        } else if housekeeping_owed {
            events.recv_timeout(
                next_housekeeping.saturating_duration_since(std::time::Instant::now()),
            )
        } else {
            events
                .recv()
                .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)
        };
        if received.is_ok() {
            housekeeping_owed = true;
        }
        match received {
            Ok(HistoryEvent::Changed(path)) => pending.note(path, QUIET_PERIOD),
            Ok(HistoryEvent::Renamed(from, to)) => {
                if let Err(error) = history
                    .lock()
                    .expect("versions store lock")
                    .note_rename(&from, &to)
                {
                    log::debug!("could not follow a rename: {error:#}");
                }
                pending.note(to, QUIET_PERIOD);
            }
            // The pass that covers what no event could: the queue overflowed, or
            // the service was not running when somebody saved. The catalogue
            // was just reconciled with the disk, so a document whose newest
            // version it matches is skipped without touching the disk.
            Ok(HistoryEvent::Catalogued(entries)) => {
                let unkept = history.lock().expect("versions store lock").unkept(entries);
                for path in unkept {
                    pending.note(path, std::time::Duration::ZERO);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }

        let until = std::time::Instant::now() + WORK_BUDGET;
        let mut due = pending.due();
        while std::time::Instant::now() < until {
            let Some(path) = due.next() else { break };
            // Check the deadline before consuming the next path. Unvisited
            // entries remain queued even when this batch runs out of time.
            // The copy runs without the lock: at idle priority it can wait a
            // long time for the disk, and the file manager's request before a
            // restore must not wait with it.
            let prepared = history
                .lock()
                .expect("versions store lock")
                .prepare_version(&path, Reason::Saved);
            let kept = match prepared {
                Ok(Some(version)) => {
                    let copied = clone_file(&path, &version.object);
                    history
                        .lock()
                        .expect("versions store lock")
                        .finish_version(version, copied)
                }
                Ok(None) => Ok(false),
                Err(error) => Err(error),
            };
            match kept {
                Ok(true) => {
                    log::debug!("kept a version of {}", path.display());
                    housekeeping_owed = true;
                }
                Ok(false) => {}
                Err(error) => log::debug!("no version of {}: {error:#}", path.display()),
            }
        }

        if housekeeping_owed && std::time::Instant::now() >= next_housekeeping {
            housekeeping_owed = false;
            next_housekeeping = std::time::Instant::now() + HOUSEKEEPING;
            // The lock is taken per small batch, so a request from the file
            // manager never waits for a whole step.
            if let Err(error) = watch_free_space(history) {
                log::warn!("could not check the free space: {error:#}");
            }
            let unchecked = std::mem::take(
                &mut history
                    .lock()
                    .expect("versions store lock")
                    .budget_unchecked,
            );
            if unchecked && let Err(error) = enforce_budget(history) {
                history
                    .lock()
                    .expect("versions store lock")
                    .budget_unchecked = true;
                log::warn!("could not keep versions inside the budget: {error:#}");
            }
            if std::time::Instant::now() >= next_missing_sweep {
                next_missing_sweep = std::time::Instant::now() + MISSING_SWEEP;
                if let Err(error) = expire_missing(history) {
                    log::warn!("could not retire missing documents: {error:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(conn: Connection, dir: &Path) -> History {
        History {
            conn: Some(conn),
            objects: dir.join("objects"),
            support: Support::Ready,
            hopeless_devices: Vec::new(),
            budget_bytes: 0,
            keep_versions: 1,
            keep_days: 1,
            paused_for_space: false,
            budget_unchecked: false,
            skip_git_repositories: false,
            respect_gitignore: false,
        }
    }

    fn version_owners(conn: &Connection) -> Vec<(String, String)> {
        conn.prepare(
            "SELECT f.current_path, v.object FROM versions v JOIN files f ON f.id = v.file_id
              ORDER BY v.object",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// `mv a.md b.md` over a document with its own history keeps every version
    /// of both under the new name.
    #[test]
    fn rename_over_a_kept_document() {
        let dir = crate::test_env::scratch_dir("bs-history-rename");
        let conn = open_store(&dir.join("history.sqlite")).unwrap();
        for (id, path) in [(1, "/d/a.md"), (2, "/d/b.md")] {
            conn.execute(
                "INSERT INTO files(id, current_path, created_at) VALUES (?1, ?2, 0)",
                params![id, path],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO versions(file_id, saved_at, mtime_s, mtime_ns, size, object, reason)
                 VALUES (?1, 0, 0, 0, 1, ?2, 'saved')",
                params![id, format!("object-{id}")],
            )
            .unwrap();
        }
        follow_rename(&conn, Path::new("/d/a.md"), Path::new("/d/b.md")).unwrap();
        assert_eq!(
            version_owners(&conn),
            [
                ("/d/b.md".to_string(), "object-1".to_string()),
                ("/d/b.md".to_string(), "object-2".to_string()),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A new file given a deleted document's inode number starts its own
    /// history; a renamed document keeps its own.
    #[test]
    fn reused_inode_starts_a_new_history() {
        let dir = crate::test_env::scratch_dir("bs-history-inode");
        std::fs::create_dir_all(&dir).unwrap();
        let store = store_with(open_store(&dir.join("history.sqlite")).unwrap(), &dir);
        let conn = store.conn.as_ref().unwrap();
        let notes = dir.join("notes.odt");
        std::fs::write(&notes, b"new").unwrap();
        let metadata = std::fs::metadata(&notes).unwrap();
        let born = metadata
            .created()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        let deleted = |born: i64| {
            conn.execute("DELETE FROM files", []).unwrap();
            conn.execute(
                "INSERT INTO files(id, current_path, dev, ino, born, created_at)
                 VALUES (7, '/gone/report.odt', ?1, ?2, ?3, 0)",
                params![metadata.dev() as i64, metadata.ino() as i64, born],
            )
            .unwrap();
        };

        deleted(born - 1);
        assert_ne!(store.upsert_file(&notes, &metadata).unwrap(), 7);
        deleted(born);
        assert_eq!(store.upsert_file(&notes, &metadata).unwrap(), 7);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A document gone past the grace period is forgotten; one that came back
    /// is no longer counted missing; one just gone starts its grace period.
    #[test]
    fn missing_documents_expire_after_the_grace() {
        let dir = crate::test_env::scratch_dir("bs-history-missing");
        std::fs::create_dir_all(&dir).unwrap();
        let present = dir.join("present.md");
        std::fs::write(&present, b"x").unwrap();
        let conn = open_store(&dir.join("history.sqlite")).unwrap();
        let long_ago = now_secs() - MISSING_GRACE_SECONDS - 1;
        for (id, path, missing_since) in [
            (1, present.to_string_lossy().into_owned(), Some(long_ago)),
            (2, "/gone/old.md".to_string(), Some(long_ago)),
            (3, "/gone/new.md".to_string(), None),
        ] {
            conn.execute(
                "INSERT INTO files(id, current_path, created_at, missing_since)
                 VALUES (?1, ?2, 0, ?3)",
                params![id, path, missing_since],
            )
            .unwrap();
        }
        let history: SharedHistory =
            std::sync::Arc::new(std::sync::Mutex::new(store_with(conn, &dir)));
        expire_missing(&history).unwrap();

        let store = history.lock().unwrap();
        let rows: Vec<(i64, Option<i64>)> = store
            .conn
            .as_ref()
            .unwrap()
            .prepare("SELECT id, missing_since FROM files ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0], (1, None));
        assert!(rows[1].0 == 3 && rows[1].1.is_some(), "{rows:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A path that is not UTF-8 never finds the row of the file whose name its
    /// lossy form spells.
    #[test]
    fn non_utf8_path_has_no_row() {
        use std::os::unix::ffi::OsStrExt;
        let dir = crate::test_env::scratch_dir("bs-history-utf8");
        let conn = open_store(&dir.join("history.sqlite")).unwrap();
        conn.execute(
            "INSERT INTO files(id, current_path, created_at) VALUES (1, '/d/Relat\u{FFFD}rios/a.md', 0)",
            [],
        )
        .unwrap();
        let latin1 = Path::new(std::ffi::OsStr::from_bytes(b"/d/Relat\xf3rios/a.md"));
        assert_eq!(file_id_for_path(&conn, latin1).unwrap(), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_work_trees_and_their_ignore_rules_are_found() {
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

    #[test]
    fn unvisited_history_paths_survive_a_partial_batch() {
        let mut pending = Pending::new();
        for name in ["a", "b", "c"] {
            pending.note(name.into(), std::time::Duration::ZERO);
        }
        let first = pending.due().next().unwrap();
        let rest: Vec<_> = pending.due().collect();
        assert_eq!(rest.len(), 2);
        assert!(!rest.contains(&first));
        assert!(pending.is_empty());
    }

    #[test]
    fn a_zero_budget_does_not_remove_ready_history_paths() {
        let mut pending = Pending::new();
        pending.note("a".into(), std::time::Duration::ZERO);
        pending.note("b".into(), std::time::Duration::ZERO);
        drop(pending.due());
        assert_eq!(pending.due().count(), 2);
    }

    #[test]
    fn oldest_history_deadlines_run_first_and_future_paths_wait() {
        let mut pending = Pending::new();
        let now = std::time::Instant::now();
        pending.deadlines.insert("newest".into(), now);
        let oldest = now.checked_sub(std::time::Duration::from_secs(2)).unwrap();
        pending.deadlines.insert("oldest".into(), oldest);
        pending
            .deadlines
            .insert("future".into(), now + std::time::Duration::from_secs(60));
        assert_eq!(pending.due().next().unwrap(), PathBuf::from("oldest"));
        assert_eq!(
            pending.due().collect::<Vec<_>>(),
            vec![PathBuf::from("newest")]
        );
        assert!(pending.deadlines.contains_key(Path::new("future")));
    }

    #[test]
    fn a_later_save_keeps_its_debounce_after_a_short_history_batch() {
        let mut pending = Pending::new();
        pending.note("a".into(), std::time::Duration::ZERO);
        pending.note("b".into(), std::time::Duration::ZERO);
        assert_eq!(pending.due().next().unwrap(), PathBuf::from("a"));
        pending.note("b".into(), std::time::Duration::from_secs(60));
        assert_eq!(pending.due().count(), 0);
        assert_eq!(pending.deadlines.len(), 1);
    }

    #[test]
    fn partial_batches_do_not_release_queue_capacity_for_unvisited_paths() {
        let mut pending = Pending::new();
        pending.limit = 2;
        pending.note("a".into(), std::time::Duration::ZERO);
        pending.note("b".into(), std::time::Duration::ZERO);
        drop(pending.due());
        pending.note("c".into(), std::time::Duration::ZERO);
        assert_eq!(
            pending.due().collect::<Vec<_>>(),
            vec![PathBuf::from("a"), PathBuf::from("b")]
        );
        assert!(pending.is_empty());
    }

    /// Ten saves in five minutes must not push out yesterday's version.
    ///
    /// The bucket is what does it: everything inside the last hour shares one
    /// slot, so only the newest of the burst is kept, and yesterday sits in a
    /// slot of its own.
    #[test]
    fn a_burst_of_saves_does_not_evict_yesterday() {
        let now = 1_800_000_000;
        let burst: Vec<i64> = (0..10).map(|minute| now - minute * 30).collect();
        let buckets: Vec<i64> = burst.iter().map(|at| time_bucket(now, *at)).collect();
        assert!(
            buckets.windows(2).all(|pair| pair[0] == pair[1]),
            "the whole burst has to share one slot: {buckets:?}"
        );
        let yesterday = time_bucket(now, now - 30 * 3600);
        assert_ne!(buckets[0], yesterday);
    }

    /// Older versions are kept further apart: hours today, days this week,
    /// weeks after that.
    #[test]
    fn versions_are_kept_further_apart_as_they_age() {
        let now = 1_800_000_000;
        const HOUR: i64 = 3600;
        const DAY: i64 = 24 * HOUR;
        // Two saves three hours ago and two hours ago are different slots.
        assert_ne!(
            time_bucket(now, now - 3 * HOUR),
            time_bucket(now, now - 2 * HOUR)
        );
        // Two saves an hour apart, three days ago, are the same slot.
        assert_eq!(
            time_bucket(now, now - 3 * DAY),
            time_bucket(now, now - 3 * DAY - HOUR)
        );
        // Two saves a day apart, three weeks ago, are the same slot.
        assert_eq!(
            time_bucket(now, now - 21 * DAY),
            time_bucket(now, now - 21 * DAY - DAY)
        );
    }

    /// A refusal is filed against the right thing: the disk, the file, or the
    /// space left.
    ///
    /// Getting this wrong in either direction is expensive: blaming the disk for
    /// one file switches the feature off for everything, and blaming the file
    /// for the disk means retrying on every document forever.
    #[test]
    fn each_refusal_is_about_what_it_is_about() {
        let of = |code| classify(&io::Error::from_raw_os_error(code));
        assert!(matches!(
            of(libc::EOPNOTSUPP),
            Refusal::Device(Unavailable::NoReflink)
        ));
        assert!(matches!(
            of(libc::EXDEV),
            Refusal::Device(Unavailable::OldKernel)
        ));
        assert!(matches!(of(libc::ENOSPC), Refusal::OutOfSpace));
        assert!(matches!(of(libc::EDQUOT), Refusal::OutOfSpace));
        assert!(matches!(of(libc::EINVAL), Refusal::File));
        assert!(matches!(of(libc::EPERM), Refusal::File));
    }

    /// After a restart the pass over the catalogue checks only what the newest
    /// version does not already hold, without touching the disk.
    #[test]
    fn the_catalogue_pass_skips_documents_already_kept() {
        let dir = std::env::temp_dir().join(format!("bs-history-unkept-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let conn = open_store(&dir.join("history.sqlite")).unwrap();
        for (id, path) in [(1, "/d/kept.md"), (2, "/d/edited.md")] {
            conn.execute(
                "INSERT INTO files(id, current_path, created_at) VALUES (?1, ?2, 0)",
                params![id, path],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO versions(file_id, saved_at, mtime_s, mtime_ns, size, object, reason)
                 VALUES (?1, 0, 10, 0, 5, 'x', 'saved')",
                params![id],
            )
            .unwrap();
        }
        let store = store_with(conn, &dir);

        let unkept = store.unkept(vec![
            (PathBuf::from("/d/kept.md"), 10, 5),
            (PathBuf::from("/d/edited.md"), 11, 5),
            (PathBuf::from("/d/new.md"), 10, 5),
        ]);
        assert_eq!(
            unkept,
            [PathBuf::from("/d/edited.md"), PathBuf::from("/d/new.md")]
        );
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn disabled_store_erases_its_files() {
        let _env = crate::test_env::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let data = crate::test_env::scratch_dir("bs-history-erase");
        let _data_home = crate::test_env::EnvVarGuard::set("XDG_DATA_HOME", data.to_str().unwrap());
        let objects = crate::config::history_dir().join("objects/ab");
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::write(objects.join("cdef"), "kept").unwrap();
        drop(open_store(&crate::config::history_db_path()).unwrap());

        let mut store = History::open(&big_search_config::FileHistory {
            enabled: false,
            ..Default::default()
        });
        store.forget_everything().unwrap();
        assert!(!crate::config::history_dir().join("objects").exists());
        assert!(!crate::config::history_db_path().exists());
        std::fs::remove_dir_all(&data).ok();
    }

    #[test]
    fn free_space_follows_the_store() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // A small store only needs the floor, however big the disk.
        assert_eq!(low_disk_threshold(800 * 1024 * 1024), 8000 * 1024 * 1024);
        assert_eq!(low_disk_threshold(0), 2 * GIB);
        assert_eq!(low_disk_threshold(10 * GIB), 100 * GIB);
        assert_eq!(recovered_disk_target(0), 2 * GIB + GIB / 2);
    }

    /// Two objects never collide, and each lands in a shard.
    #[test]
    fn every_object_gets_its_own_name() {
        let first = object_name();
        let second = object_name();
        assert_ne!(first, second);
        assert_eq!(first.len(), 33, "{first}");
        assert_eq!(&first[2..3], "/");
    }
}
