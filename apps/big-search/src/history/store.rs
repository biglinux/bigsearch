//! The store on disk: its directory, the SQLite schema, catalogue rows looked
//! up by path, and the names of the objects versions are copied to.
//!
//! `files` holds one row per document, keyed by its UTF-8 path; `versions`
//! holds one row per kept copy, pointing at an object under `objects/`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Whether this is a live session, where nothing survives the reboot.
pub(super) fn is_live_session() -> bool {
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

pub(super) fn open_store(path: &Path) -> Result<Connection> {
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

/// Move `from`'s catalogue row to `to`, in one transaction.
///
/// When `to` has a row of its own (`mv a.md b.md` over a document with
/// history), its versions join `from`'s: they hold what the rename overwrote.
/// Left alone, the unique path would refuse the move, and `from`'s row would
/// expire with every version in it.
pub(super) fn follow_rename(conn: &Connection, from: &Path, to: &Path) -> Result<()> {
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
