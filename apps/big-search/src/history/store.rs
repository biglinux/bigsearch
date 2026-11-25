//! The store on disk: its directory, the SQLite schema, catalogue rows looked
//! up by path, and the names of the objects versions are copied to.
//!
//! `files` holds one row per document, keyed by its UTF-8 path, or by none
//! while its document is not known to be anywhere; `versions` holds one row
//! per kept copy, pointing at an object under `objects/`.

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

/// The columns of `files`.
const FILES_COLUMNS: &str = "(
    id INTEGER PRIMARY KEY,
    current_path TEXT UNIQUE,
    dev INTEGER,
    ino INTEGER,
    created_at INTEGER NOT NULL,
    missing_since INTEGER,
    born INTEGER
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
    // Stores created before a row could be parked without a key. SQLite
    // lifts a column's not-null constraint only by rebuilding the table, with
    // foreign keys off, which it ignores inside a transaction.
    let key_required: bool = conn.query_row(
        "SELECT \"notnull\" FROM pragma_table_info('files') WHERE name = 'current_path'",
        [],
        |row| row.get(0),
    )?;
    if key_required {
        conn.execute_batch("PRAGMA foreign_keys = OFF")?;
        let transaction = conn.unchecked_transaction()?;
        let rebuilt = transaction
            .execute_batch(&format!(
                "
                CREATE TABLE files_rebuilt {FILES_COLUMNS};
                INSERT INTO files_rebuilt(id, current_path, dev, ino, created_at, missing_since, born)
                    SELECT id, current_path, dev, ino, created_at, missing_since, born FROM files;
                DROP TABLE files;
                ALTER TABLE files_rebuilt RENAME TO files;
                CREATE INDEX idx_files_dev_ino ON files(dev, ino);
                "
            ))
            .and_then(|()| transaction.commit());
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
        rebuilt.context("rebuild the versions store's table of documents")?;
    }
    Ok(conn)
}

/// Take `file_id`'s key away and count its document missing from now, if it
/// was not already: it is not known to be anywhere until a file with its
/// identity claims it ([`rows_of_file`]) or the missing sweep expires it.
pub(super) fn park(conn: &Connection, file_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE files SET current_path = NULL, missing_since = COALESCE(missing_since, ?2)
          WHERE id = ?1",
        params![file_id, now_secs()],
    )?;
    Ok(())
}

/// Move each `from`'s catalogue row to its `to`, in one transaction.
///
/// When `to` has a row of its own (`mv a.md b.md` over a document with
/// history), its versions join `from`'s: they hold what the rename overwrote.
/// Left alone, the unique path would refuse the move, and `from`'s row would
/// expire with every version in it.
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
        let Some(from_id) = file_id_for_path(conn, from)? else {
            continue;
        };
        let Some(key) = to.to_str() else {
            anyhow::bail!(
                "{} is not UTF-8, which versions are kept under",
                to.display()
            );
        };
        if let Some(to_id) = file_id_for_path(conn, to)? {
            join_rows(conn, from_id, &[to_id])?;
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

/// The rows recorded for the file with this `(dev, ino, born)`, with their
/// keys (none for a parked row), newest first. A row or a file without a
/// birth time matches on `(dev, ino)` alone.
pub(super) fn rows_of_file(
    conn: &Connection,
    (dev, ino, born): (i64, i64, Option<i64>),
) -> Result<Vec<(i64, Option<String>)>> {
    conn.prepare(
        "SELECT id, current_path FROM files
          WHERE dev = ?1 AND ino = ?2 AND (born IS NULL OR ?3 IS NULL OR born = ?3)
          ORDER BY created_at DESC",
    )?
    .query_map(params![dev, ino, born], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?
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
