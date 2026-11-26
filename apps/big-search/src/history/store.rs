//! The store on disk: its directory, the SQLite schema, catalogue rows looked
//! up by path, and the names of the objects versions are copied to.
//!
//! `files` holds one row per document, keyed by its UTF-8 path, or by none
//! once parked; `versions` holds one row per kept copy, pointing at an object
//! under `objects/`.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Whether this is a live session, where nothing survives the reboot: the root
/// is an overlay.
pub(super) fn is_live_session() -> bool {
    crate::mounts::mount_for(Path::new("/")).is_some_and(|mount| mount.fstype == "overlay")
}

/// The store's own directory, with the marker that keeps it out of backups.
///
/// `CACHEDIR.TAG` is what `rsync --exclude-caches`, borg and Deja Dup read.
/// Without it every backup would carry ten versions of every document.
pub(super) fn prepare_store(objects: &Path) -> Result<()> {
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

/// The columns of `files`. `fs`, `ino` and `born` are the document's identity
/// (`super::identity`); `parked_from` is the key a parked row was taken from.
const FILES_COLUMNS: &str = "(
    id INTEGER PRIMARY KEY,
    current_path TEXT UNIQUE,
    fs INTEGER,
    ino INTEGER,
    created_at INTEGER NOT NULL,
    missing_since INTEGER,
    born INTEGER,
    parked_from TEXT
)";

pub(super) fn open_store(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(1))?;
    conn.execute_batch(&format!(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA journal_size_limit = 1048576;
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS files {FILES_COLUMNS};
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
        "
    ))?;
    // Stores created before the birth time was recorded.
    let has_born: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('files') WHERE name = 'born')",
        [],
        |row| row.get(0),
    )?;
    if !has_born {
        conn.execute_batch("ALTER TABLE files ADD COLUMN born INTEGER")?;
    }
    // Stores whose rows recorded `st_dev`, which btrfs draws anew at each
    // mount, and needed a key. SQLite lifts a column's not-null constraint
    // only by rebuilding the table, with foreign keys off, which it ignores
    // inside a transaction.
    let records_dev: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('files') WHERE name = 'dev')",
        [],
        |row| row.get(0),
    )?;
    if records_dev {
        let filesystems = filesystems_of_keys(&conn)?;
        conn.execute_batch("PRAGMA foreign_keys = OFF")?;
        let rebuilt = rebuild_files(&conn, &filesystems);
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
        rebuilt.context("rebuild the versions store's table of documents")?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_files_fs_ino ON files(fs, ino)")?;
    Ok(conn)
}

/// The filesystem of each keyed row's folder, by row id: where its document
/// was kept. A folder that is gone or cannot be read leaves its rows none,
/// to be found by key alone.
fn filesystems_of_keys(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let keys: Vec<(i64, String)> = conn
        .prepare("SELECT id, current_path FROM files WHERE current_path IS NOT NULL")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut by_folder: HashMap<PathBuf, Option<i64>> = HashMap::new();
    let mut filesystems = Vec::new();
    for (id, key) in keys {
        let Some(folder) = Path::new(&key).parent() else {
            continue;
        };
        let fs = *by_folder.entry(folder.to_path_buf()).or_insert_with(|| {
            std::fs::metadata(folder)
                .and_then(|metadata| super::disk::filesystem_id(folder, &metadata))
                .ok()
        });
        if let Some(fs) = fs {
            filesystems.push((id, fs));
        }
    }
    Ok(filesystems)
}

/// Rebuild `files` in the current columns, each row's filesystem from
/// `filesystems`, in one transaction.
fn rebuild_files(conn: &Connection, filesystems: &[(i64, i64)]) -> Result<()> {
    let transaction = conn.unchecked_transaction()?;
    transaction.execute_batch(&format!(
        "
        CREATE TABLE files_rebuilt {FILES_COLUMNS};
        INSERT INTO files_rebuilt(id, current_path, ino, created_at, missing_since, born)
            SELECT id, current_path, ino, created_at, missing_since, born FROM files;
        "
    ))?;
    {
        let mut identify = transaction.prepare("UPDATE files_rebuilt SET fs = ?2 WHERE id = ?1")?;
        for (id, fs) in filesystems {
            identify.execute(params![id, fs])?;
        }
    }
    transaction.execute_batch("DROP TABLE files; ALTER TABLE files_rebuilt RENAME TO files;")?;
    transaction.commit()?;
    Ok(())
}

/// Take `file_id`'s key away for good, keeping it as the place the row was
/// parked from, and count its document missing from now, if it was not
/// already: a forget through that place erases it, or the missing sweep
/// expires it.
pub(super) fn park(conn: &Connection, file_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE files SET parked_from = current_path, current_path = NULL,
                          missing_since = COALESCE(missing_since, ?2)
          WHERE id = ?1",
        params![file_id, now_secs()],
    )?;
    Ok(())
}

/// The parked rows taken from any of `keys`.
pub(super) fn parked_from(conn: &Connection, keys: &[&Path]) -> Result<Vec<i64>> {
    let mut statement = conn.prepare("SELECT id FROM files WHERE parked_from = ?1")?;
    let mut parked = Vec::new();
    for key in keys.iter().filter_map(|key| key.to_str()) {
        for id in statement.query_map(params![key], |row| row.get(0))? {
            parked.push(id?);
        }
    }
    Ok(parked)
}

/// Move each `from`'s catalogue row to its `to`, in one transaction.
///
/// When `to` has a row of its own (`mv a.md b.md` over a document with
/// history), its versions join `from`'s: they hold what the rename overwrote.
/// Left alone, the unique path would refuse the move, and `from`'s row would
/// expire with every version in it. A row whose document was found gone is
/// parked instead: at `to` it held nothing the rename overwrote, and at
/// `from` it moves only with its own file, not another one that arrived there.
///
/// # Errors
///
/// Returns an error, moving nothing, when a `to` with a row to receive is not
/// UTF-8: its key would name no file. Also when the database cannot be written.
pub(super) fn follow_renames<'a>(
    conn: &Connection,
    moves: impl IntoIterator<Item = (&'a Path, &'a Path)>,
) -> Result<()> {
    let transaction = conn.unchecked_transaction()?;
    for (from, to) in moves {
        let Some(moved) = kept_at(conn, from)? else {
            continue;
        };
        if moved.missing_since.is_some()
            && !std::fs::symlink_metadata(to).is_ok_and(|metadata| moved.records(&metadata))
        {
            park(conn, moved.id)?;
            continue;
        }
        let from_id = moved.id;
        let Some(key) = to.to_str() else {
            anyhow::bail!(
                "{} is not UTF-8, which versions are kept under",
                to.display()
            );
        };
        match kept_at(conn, to)? {
            Some(overwritten) if overwritten.missing_since.is_some() => {
                park(conn, overwritten.id)?;
            }
            Some(overwritten) => join_rows(conn, from_id, &[overwritten.id])?,
            None => {}
        }
        conn.execute(
            "UPDATE files SET current_path = ?2, missing_since = NULL WHERE id = ?1",
            params![from_id, key],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// Give `rows`' versions to the row `into` and delete those rows.
pub(super) fn join_rows(conn: &Connection, into: i64, rows: &[i64]) -> Result<()> {
    for &row in rows.iter().filter(|&&row| row != into) {
        conn.execute(
            "UPDATE versions SET file_id = ?1 WHERE file_id = ?2",
            params![into, row],
        )?;
        conn.execute("DELETE FROM files WHERE id = ?1", params![row])?;
    }
    Ok(())
}

/// The rows below the folder `folder`, with their keys.
pub(super) fn rows_under(conn: &Connection, folder: &Path) -> Result<Vec<(i64, PathBuf)>> {
    let Some(key) = folder.to_str() else {
        return Ok(Vec::new());
    };
    // `0` follows `/` in byte order, so the range is every key starting
    // `folder/`, read from the unique index on the key.
    conn.prepare(
        "SELECT id, current_path FROM files WHERE current_path > ?1 AND current_path < ?2",
    )?
    .query_map(params![format!("{key}/"), format!("{key}0")], |row| {
        Ok((row.get(0)?, PathBuf::from(row.get::<_, String>(1)?)))
    })?
    .collect::<rusqlite::Result<_>>()
    .map_err(Into::into)
}

/// The rows recorded for the file with this `(fs, ino, born)`, parked ones
/// included. A row or a file without a birth time matches on `(fs, ino)` alone.
pub(super) fn rows_of_file(
    conn: &Connection,
    (fs, ino, born): (i64, i64, Option<i64>),
) -> Result<Vec<i64>> {
    conn.prepare(
        "SELECT id FROM files
          WHERE fs = ?1 AND ino = ?2 AND (born IS NULL OR ?3 IS NULL OR born = ?3)",
    )?
    .query_map(params![fs, ino, born], |row| row.get(0))?
    .collect::<rusqlite::Result<_>>()
    .map_err(Into::into)
}

/// A row as the decision about the file at its key needs it.
#[derive(PartialEq, Eq)]
pub(super) struct Kept {
    pub(super) id: i64,
    ino: Option<i64>,
    born: Option<i64>,
    /// When the missing sweep found no file at the key, cleared by a save.
    pub(super) missing_since: Option<i64>,
}

impl Kept {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            ino: row.get("ino")?,
            born: row.get("born")?,
            missing_since: row.get("missing_since")?,
        })
    }

    /// Whether the row records this file. At one path the inode number and
    /// birth time tell files apart without the filesystem, whose `f_fsid` is
    /// a device number on XFS and may change across boots. A row or a file
    /// without a birth time matches on the inode number alone.
    pub(super) fn records(&self, metadata: &std::fs::Metadata) -> bool {
        let born = super::born(metadata);
        self.ino == Some(metadata.ino().cast_signed())
            && (self.born.is_none() || born.is_none() || self.born == born)
    }
}

/// The row kept under `path`, if any; none for a path that is not UTF-8,
/// as [`file_id_for_path`] explains.
pub(super) fn kept_at(conn: &Connection, path: &Path) -> Result<Option<Kept>> {
    let Some(key) = path.to_str() else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT id, ino, born, missing_since FROM files WHERE current_path = ?1",
            params![key],
            Kept::read,
        )
        .optional()?)
}

/// Every row with its key, none for a parked one.
pub(super) fn all_rows(conn: &Connection) -> Result<Vec<(Option<String>, Kept)>> {
    conn.prepare("SELECT id, current_path, ino, born, missing_since FROM files")?
        .query_map([], |row| Ok((row.get("current_path")?, Kept::read(row)?)))?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
}

/// The row for `path`. Rows are keyed by UTF-8 text, so a path that is not
/// UTF-8 has none: a lossy key would name a different file.
pub(super) fn file_id_for_path(conn: &Connection, path: &Path) -> Result<Option<i64>> {
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

/// The row of the first of `spellings` that has one, with that spelling.
pub(super) fn first_stored<'a>(
    conn: &Connection,
    spellings: &'a [impl AsRef<Path>],
) -> Result<Option<(i64, &'a Path)>> {
    for path in spellings {
        let path = path.as_ref();
        if let Some(file_id) = file_id_for_path(conn, path)? {
            return Ok(Some((file_id, path)));
        }
    }
    Ok(None)
}

/// A name for one object: two hex characters of directory, then the rest.
///
/// Named after nothing in the document, because the document will be renamed and
/// the object must not have to follow.
pub(super) fn object_name() -> String {
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

pub(super) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
