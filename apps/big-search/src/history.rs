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
//! blocks; nothing is copied and nothing is charged.
//!
//! That is the price of the first version only. Every document editor saves
//! by writing a temporary file and renaming it over the original, so the file
//! that comes out shares no block with the version before it, and the earlier
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

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

mod disk;
mod eligibility;
mod retention;
mod store;
#[cfg(test)]
mod tests;
mod worker;

pub(crate) use disk::{Refusal, clone_file};
pub use eligibility::{eligible_metadata, may_be_a_document};
pub use worker::{HistoryEvent, Pending, run_worker};

use disk::disk_capacity;
use eligibility::{eligible_name, git_work_tree, gitignored};
use store::{
    file_id_for_path, follow_rename, is_live_session, now_secs, object_name, open_store,
    prepare_store,
};

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

    /// Follow a renamed document: keep its history under the destination name.
    ///
    /// The watcher hears the rename as a pair (the kernel gives both names in
    /// one event), and that pair is the only reliable way to follow a document
    /// that is renamed and then saved: the save replaces the inode, so by the
    /// time anything looks, neither the source name nor its identity is there
    /// to match on.
    pub fn note_rename(&self, from: &Path, to: &Path) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        // Only to a place the versions could be asked for again. A document
        // renamed into a hidden name is on its way out: a file manager deletes
        // for good with one rename to `.something-delete-...` and then an
        // unlink, and following it there would file the history
        // under a name nobody can type, out of reach of the request to forget it.
        if eligible_name(to).is_none() {
            return Ok(());
        }
        follow_rename(conn, from, to)
    }

    /// The earlier versions of one document, newest first.
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
            // The state is already kept, so there is nothing to clone. But when
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
    /// only when the row's path is gone; otherwise two hard links to one inode
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
            // This one file cannot be cloned (`nodatacow`, an immutable flag,
            // a file being executed). Everything else on the disk still can.
            Refusal::File => {}
            Refusal::OutOfSpace => self.paused_for_space = true,
        }
    }
}
