//! Persisted `path → (mtime, size)` cache. Drives incremental dedup (unchanged
//! files are never re-extracted) and deletion detection on a full reconcile.
//! Stored in SQLite so each persist writes only changed rows instead of a full
//! JSON catalog rewrite.
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Namespace of the content-extraction state inside the shared state database.
pub const CONTENT_NAMESPACE: &str = "content";
/// Namespace of the name-catalogue state inside the shared state database.
pub const NAME_NAMESPACE: &str = "name";

pub struct State {
    db_path: PathBuf,
    namespace: String,
    conn: Connection,
    pending: HashMap<String, PendingChange>,
    clear_before_persist: bool,
    dirty: bool,
}

/// A difference a reconciliation pass found (see [`State::scan_changes_after`]).
#[derive(Debug, PartialEq, Eq)]
pub enum ScanChange {
    /// On disk with this `(mtime, size)`, which the catalogue records otherwise
    /// (`recorded`) or not at all.
    Seen {
        path: PathBuf,
        mtime: i64,
        size: u64,
        recorded: Option<(i64, u64)>,
    },
    /// In the catalogue, not seen by the pass.
    Gone(PathBuf),
}

#[derive(Clone, Copy)]
enum PendingChange {
    Set(i64, u64),
    Remove,
}

impl State {
    /// The `namespace` rows of the state database at `db_path`, or an empty
    /// in-memory state if the database cannot be opened.
    pub fn load(db_path: &Path, namespace: &str) -> Self {
        match open_state_db(db_path) {
            Ok(conn) => State {
                db_path: db_path.to_path_buf(),
                namespace: namespace.to_owned(),
                conn,
                pending: HashMap::new(),
                clear_before_persist: false,
                dirty: false,
            },
            Err(_) => State::in_memory(db_path.to_path_buf(), namespace),
        }
    }

    /// Start empty (used for a forced full rebuild): the first persist replaces
    /// every row of `namespace`.
    pub fn empty(db_path: &Path, namespace: &str) -> Self {
        let conn = open_state_db(db_path).unwrap_or_else(|_| {
            let conn = Connection::open_in_memory().expect("in-memory sqlite state opens");
            tune_connection_for_low_memory(&conn);
            init_schema(&conn).expect("in-memory sqlite state schema initializes");
            conn
        });
        State {
            db_path: db_path.to_path_buf(),
            namespace: namespace.to_owned(),
            conn,
            pending: HashMap::new(),
            clear_before_persist: true,
            dirty: true,
        }
    }

    /// Whether `path` is recorded with this exact mtime+size (→ unchanged).
    pub fn unchanged(&self, path: &Path, mtime: i64, size: u64) -> bool {
        self.recorded(path) == Some((mtime, size))
    }

    /// The mtime and size `path` is catalogued with, or `None` when it is not
    /// catalogued. One lookup answers both of the scanner's questions — is it
    /// unchanged, is it known at all — which it used to ask SQLite separately
    /// for every new or changed file.
    pub fn recorded(&self, path: &Path) -> Option<(i64, u64)> {
        let path_key = key(path);
        match self.pending.get(&path_key).copied() {
            Some(PendingChange::Set(mtime, size)) => Some((mtime, size)),
            Some(PendingChange::Remove) => None,
            None if self.clear_before_persist => None,
            None => self
                .conn
                .prepare_cached(
                    "SELECT mtime, size FROM file_state WHERE namespace = ?1 AND path = ?2",
                )
                .and_then(|mut stmt| {
                    stmt.query_row(params![self.namespace, path_key], |row| {
                        Ok((row.get::<_, i64>(0)?, db_size_to_u64(row.get::<_, i64>(1)?)))
                    })
                })
                .ok(),
        }
    }

    /// Whether `path` is catalogued at all (any mtime/size). A catalogued path
    /// already has its name document indexed, so a metadata-only change must
    /// not rewrite the name index.
    pub fn contains(&self, path: &Path) -> bool {
        let path_key = key(path);
        match self.pending.get(&path_key) {
            Some(PendingChange::Set(_, _)) => true,
            Some(PendingChange::Remove) => false,
            None if self.clear_before_persist => false,
            None => self.contains_persisted_key(&path_key),
        }
    }

    pub fn set(&mut self, path: &Path, mtime: i64, size: u64) {
        self.pending
            .insert(key(path), PendingChange::Set(mtime, size));
        self.dirty = true;
    }

    pub fn remove(&mut self, path: &Path) {
        self.pending.insert(key(path), PendingChange::Remove);
        self.dirty = true;
    }

    /// Forget every entry (a `Rebuild` clears the index, so the cache must agree —
    /// otherwise unchanged-checks would skip re-indexing the wiped documents).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.clear_before_persist = true;
        self.dirty = true;
    }

    /// Number of not-yet-flushed row changes. The scanner uses this to keep the
    /// pending map bounded on machines with little memory.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Run a reconciliation pass in one transaction. Each lookup and seen-mark
    /// would otherwise take and release the database's locks on its own: two
    /// `fcntl` and an `fstat` apiece, 324 000 system calls for a walk over
    /// 109 000 paths. Batches persisted meanwhile commit with the pass, failed
    /// or not, as each committed on its own before.
    pub fn scan_pass<T>(&mut self, pass: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.conn
            .execute_batch("BEGIN")
            .context("begin scan transaction")?;
        let result = pass(self);
        if let Err(error) = self.conn.execute_batch("COMMIT") {
            // Never leave it open: every later flush would nest in it unseen.
            let _ = self.conn.execute_batch("ROLLBACK");
            // The pass's own failure, if any, says more than the commit's.
            result?;
            return Err(error).context("commit scan transaction");
        }
        result
    }

    /// Start a low-RAM reconciliation pass. Seen paths are recorded in a SQLite
    /// temporary B-tree instead of a process-wide `HashSet<String>`.
    pub fn begin_scan(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                "
                DROP TABLE IF EXISTS temp.current_scan_seen;
                CREATE TEMP TABLE current_scan_seen (
                    path TEXT PRIMARY KEY,
                    mtime INTEGER,
                    size INTEGER
                ) WITHOUT ROWID;
                ",
            )
            .context("initialize scan seen table")
    }

    /// Mark `path` as observed in the current reconciliation pass, with the
    /// `(mtime, size)` found on disk when the pass compares them (see
    /// [`Self::scan_changes_after`]).
    pub fn mark_seen(&mut self, path: &Path, found: Option<(i64, u64)>) -> Result<()> {
        let (mtime, size) = found.map_or((None, None), |(mtime, size)| {
            (Some(mtime), Some(u64_to_db_size(size)))
        });
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO temp.current_scan_seen(path, mtime, size)
                 VALUES (?1, ?2, ?3)",
            )
            .and_then(|mut stmt| stmt.execute(params![key(path), mtime, size]))
            .map(|_| ())
            .context("mark path seen")
    }

    /// What this pass saw that the catalogue does not hold, in path order and
    /// strictly after `after`, with the cursor to continue from (`None` once
    /// both sides are read).
    ///
    /// Both lists are read a page at a time in path order and merged here. A
    /// lookup per path cost a B-tree descent of string comparisons for every
    /// path, three per path with the seen-mark and the deletion check: 38 % of
    /// a restart walk over 109 000 paths. The catalogue must be persisted when
    /// the pass starts; rows written meanwhile all sort at or before `after`.
    pub fn scan_changes_after(
        &self,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<ScanChange>, Option<String>)> {
        let page = |sql: &str,
                    namespace: Option<&str>|
         -> Result<Vec<(String, Option<i64>, Option<i64>)>> {
            let mut stmt = self.conn.prepare_cached(sql).context("prepare scan page")?;
            let row = |row: &rusqlite::Row<'_>| Ok((row.get(0)?, row.get(1)?, row.get(2)?));
            let rows = match namespace {
                Some(namespace) => stmt.query_map(params![namespace, after, limit as i64], row),
                None => stmt.query_map(params![after, limit as i64], row),
            }
            .context("query scan page")?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        };
        let seen = page(
            "SELECT path, mtime, size FROM temp.current_scan_seen
              WHERE path > ?1 ORDER BY path LIMIT ?2",
            None,
        )?;
        let known = if self.clear_before_persist {
            Vec::new()
        } else {
            page(
                "SELECT path, mtime, size FROM file_state
                  WHERE namespace = ?1 AND path > ?2 ORDER BY path LIMIT ?3",
                Some(&self.namespace),
            )?
        };
        // Past the last row of a full page the other side may still hold
        // smaller paths: stop there and continue from it next time.
        let full_end = |rows: &[(String, Option<i64>, Option<i64>)]| {
            (rows.len() == limit)
                .then(|| rows.last().map(|row| row.0.clone()))
                .flatten()
        };
        let end = match (full_end(&seen), full_end(&known)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let within = |path: &String| end.as_ref().is_none_or(|end| path <= end);

        let mut changes = Vec::new();
        let mut seen = seen.into_iter().filter(|row| within(&row.0)).peekable();
        let mut known = known.into_iter().filter(|row| within(&row.0)).peekable();
        loop {
            let order = match (seen.peek(), known.peek()) {
                (None, None) => break,
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(s), Some(k)) => s.0.cmp(&k.0),
            };
            match order {
                std::cmp::Ordering::Greater => {
                    let (path, _, _) = known.next().expect("peeked");
                    changes.push(ScanChange::Gone(PathBuf::from(path)));
                }
                order => {
                    let (path, mtime, size) = seen.next().expect("peeked");
                    let recorded = if order.is_eq() {
                        let (_, mtime, size) = known.next().expect("peeked");
                        Some((mtime.unwrap_or(0), db_size_to_u64(size.unwrap_or(0))))
                    } else {
                        None
                    };
                    // Marked without metadata: seen, nothing to compare.
                    let (Some(mtime), Some(size)) = (mtime, size) else {
                        continue;
                    };
                    let size = db_size_to_u64(size);
                    if recorded != Some((mtime, size)) {
                        changes.push(ScanChange::Seen {
                            path: PathBuf::from(path),
                            mtime,
                            size,
                            recorded,
                        });
                    }
                }
            }
        }
        Ok((changes, end))
    }

    /// Mark every cached path under the retained prefixes as seen. Used for
    /// offline removable/network sources whose catalogues must not be deleted just
    /// because the mount is absent during this scan.
    pub fn mark_seen_prefixes(&mut self, prefixes: &[PathBuf]) -> Result<()> {
        for prefix in prefixes {
            let prefix_text = key(prefix);
            let prefix_len = i64::try_from(prefix_text.len()).unwrap_or(i64::MAX);
            let child_sep_pos = prefix_len.saturating_add(1);
            self.conn
                .execute(
                    "
                    INSERT OR IGNORE INTO temp.current_scan_seen(path, mtime, size)
                    SELECT path, mtime, size FROM file_state
                    WHERE namespace = ?1
                      AND (path = ?2
                           OR (length(path) > ?3
                               AND substr(path, 1, ?3) = ?2
                               AND substr(path, ?4, 1) = '/'))
                    ",
                    params![self.namespace, prefix_text, prefix_len, child_sep_pos],
                )
                .with_context(|| format!("mark retained prefix {}", prefix.display()))?;
        }
        Ok(())
    }

    /// Batch of cached paths not seen in the current scan, lexicographically after
    /// `after`. Batching avoids allocating the whole deletion set.
    pub fn unseen_paths_after(&self, after: Option<&str>, limit: usize) -> Result<Vec<PathBuf>> {
        if limit == 0 || self.clear_before_persist {
            return Ok(Vec::new());
        }
        // The empty cursor sorts before every (absolute) path, so a bare
        // `path > ?` also starts a pass. Every keyset query here relies on that:
        // an `?2 = '' OR` guard stops SQLite seeking the primary key, and each
        // page becomes a scan from the namespace's first row.
        let after = after.unwrap_or_default();
        let mut stmt = self
            .conn
            .prepare(
                "
                SELECT path FROM file_state
                WHERE namespace = ?1
                  AND path > ?2
                  AND NOT EXISTS (
                      SELECT 1 FROM temp.current_scan_seen AS seen
                      WHERE seen.path = file_state.path
                  )
                ORDER BY path
                LIMIT ?3
                ",
            )
            .context("prepare unseen path query")?;
        let rows = stmt
            .query_map(params![self.namespace, after, limit as i64], |row| {
                row.get::<_, String>(0)
            })
            .context("query unseen paths")?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(PathBuf::from(row?));
        }
        Ok(paths)
    }

    /// Paths of this namespace (ordered, strictly after `after`) that no longer
    /// exist in `other_namespace` — the mirror of [`Self::entries_stale_against`].
    ///
    /// Deleting a file drops its catalogue row but nothing ever dropped the
    /// matching content row or content document, so the content index kept
    /// answering with paths that are gone: 55.9 % of the hits in a measured
    /// search. This is the query that finds them.
    pub fn paths_missing_from(
        &self,
        other_namespace: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PathBuf>> {
        if limit == 0 || self.clear_before_persist {
            return Ok(Vec::new());
        }
        let after = after.unwrap_or_default();
        let mut stmt = self
            .conn
            .prepare_cached(
                "
                SELECT mine.path
                FROM file_state AS mine
                LEFT JOIN file_state AS catalog
                  ON catalog.namespace = ?2 AND catalog.path = mine.path
                WHERE mine.namespace = ?1
                  AND mine.path > ?3
                  AND catalog.path IS NULL
                ORDER BY mine.path
                LIMIT ?4
                ",
            )
            .context("prepare orphan-namespace query")?;
        let rows = stmt
            .query_map(
                params![self.namespace, other_namespace, after, limit as i64],
                |row| row.get::<_, String>(0),
            )
            .context("query orphan namespace paths")?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(PathBuf::from(row?));
        }
        Ok(paths)
    }

    /// Rows of this namespace (path-ordered, strictly after `after`) whose
    /// `(mtime, size)` is absent or different in `other_namespace` of the same
    /// database. Streams the pending-work set in one indexed SQL join instead of
    /// re-walking the catalogue with a `stat` + a SELECT per file.
    pub fn entries_stale_against(
        &self,
        other_namespace: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(PathBuf, i64, u64)>> {
        if limit == 0 || self.clear_before_persist {
            return Ok(Vec::new());
        }
        let after = after.unwrap_or_default();
        let mut stmt = self
            .conn
            .prepare_cached(
                "
                SELECT catalog.path, catalog.mtime, catalog.size
                FROM file_state AS catalog
                LEFT JOIN file_state AS done
                  ON done.namespace = ?2 AND done.path = catalog.path
                WHERE catalog.namespace = ?1
                  AND catalog.path > ?3
                  AND (done.path IS NULL
                       OR done.mtime != catalog.mtime
                       OR done.size != catalog.size)
                ORDER BY catalog.path
                LIMIT ?4
                ",
            )
            .context("prepare stale-namespace query")?;
        let rows = stmt
            .query_map(
                params![self.namespace, other_namespace, after, limit as i64],
                |row| {
                    Ok((
                        PathBuf::from(row.get::<_, String>(0)?),
                        row.get::<_, i64>(1)?,
                        db_size_to_u64(row.get::<_, i64>(2)?),
                    ))
                },
            )
            .context("query stale namespace entries")?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    /// Recorded `(path, mtime, size)` after `after`, sorted, pending changes
    /// included. Used by bounded catalogue passes instead of materialising the
    /// full catalogue.
    pub fn entries_after(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(PathBuf, i64, u64)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let after_text = after.unwrap_or_default();
        let mut entries: Vec<(String, i64, u64)> = if self.clear_before_persist {
            Vec::new()
        } else {
            let mut stmt = self
                .conn
                .prepare_cached(
                    "
                    SELECT path, mtime, size FROM file_state
                    WHERE namespace = ?1 AND path > ?2
                    ORDER BY path
                    LIMIT ?3
                    ",
                )
                .context("prepare path batch query")?;
            let rows = stmt
                .query_map(params![self.namespace, after_text, limit as i64], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        db_size_to_u64(row.get::<_, i64>(2)?),
                    ))
                })
                .context("query path batch")?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for (path, change) in &self.pending {
            if !after_text.is_empty() && path.as_str() <= after_text {
                continue;
            }
            entries.retain(|(known, _, _)| known != path);
            if let PendingChange::Set(mtime, size) = *change {
                entries.push((path.clone(), mtime, size));
            }
        }
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        entries.truncate(limit);
        Ok(entries
            .into_iter()
            .map(|(path, mtime, size)| (PathBuf::from(path), mtime, size))
            .collect())
    }

    pub fn len(&self) -> usize {
        let mut count = if self.clear_before_persist {
            0usize
        } else {
            self.conn
                .query_row(
                    "SELECT COUNT(*) FROM file_state WHERE namespace = ?1",
                    params![self.namespace],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                .max(0) as usize
        };
        for (path, change) in &self.pending {
            match change {
                PendingChange::Set(_, _) => {
                    if self.clear_before_persist || !self.contains_persisted_key(path) {
                        count = count.saturating_add(1);
                    }
                }
                PendingChange::Remove => {
                    if !self.clear_before_persist && self.contains_persisted_key(path) {
                        count = count.saturating_sub(1);
                    }
                }
            }
        }
        count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn contains_persisted_key(&self, path: &str) -> bool {
        self.conn
            .prepare_cached("SELECT 1 FROM file_state WHERE namespace = ?1 AND path = ?2")
            .and_then(|mut stmt| stmt.query_row(params![self.namespace, path], |_| Ok(())))
            .is_ok()
    }

    /// Persist changed rows in one SQLite transaction.
    pub fn persist(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A savepoint, so a flush inside a scan pass nests in its transaction
        // (see [`Self::scan_pass`]); on its own it is a transaction like any.
        let tx = self.conn.savepoint().context("begin state transaction")?;
        if self.clear_before_persist {
            tx.execute(
                "DELETE FROM file_state WHERE namespace = ?1",
                params![self.namespace],
            )
            .context("clear state table")?;
        }
        {
            let mut upsert = tx
                .prepare_cached(
                    "INSERT INTO file_state(namespace, path, mtime, size)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(namespace, path) DO UPDATE
                     SET mtime = excluded.mtime, size = excluded.size",
                )
                .context("prepare state upsert")?;
            let mut delete = tx
                .prepare_cached("DELETE FROM file_state WHERE namespace = ?1 AND path = ?2")
                .context("prepare state delete")?;
            for (path, change) in &self.pending {
                match *change {
                    PendingChange::Set(mtime, size) => {
                        upsert
                            .execute(params![self.namespace, path, mtime, u64_to_db_size(size)])
                            .with_context(|| format!("upsert state {}", path))?;
                    }
                    PendingChange::Remove => {
                        delete
                            .execute(params![self.namespace, path])
                            .with_context(|| format!("delete state {}", path))?;
                    }
                }
            }
        }
        tx.commit().context("commit state transaction")?;
        self.pending.clear();
        self.clear_before_persist = false;
        self.dirty = false;
        Ok(())
    }

    /// Journal mode of the backing connection (tests assert the low-memory tuning).
    #[cfg(test)]
    fn journal_mode(&self) -> String {
        self.conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap_or_default()
    }

    fn in_memory(db_path: PathBuf, namespace: &str) -> Self {
        let conn = Connection::open_in_memory().expect("in-memory sqlite state opens");
        init_schema(&conn).expect("in-memory sqlite state schema initializes");
        State {
            db_path,
            namespace: namespace.to_owned(),
            conn,
            pending: HashMap::new(),
            clear_before_persist: false,
            dirty: false,
        }
    }
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn open_state_db(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db_path).with_context(|| format!("open {}", db_path.display()))?;
    tune_connection_for_low_memory(&conn);
    init_schema(&conn)?;
    Ok(conn)
}

fn tune_connection_for_low_memory(conn: &Connection) {
    // Keep SQLite's page cache deliberately small and let large scans spill temp
    // B-trees to disk instead of anonymous process memory. mmap is capped: it uses
    // virtual address space and the kernel page cache, not a preallocated heap.
    let _ = conn.execute_batch(
        "
        PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA temp_store = FILE;
        PRAGMA cache_size = -2048;
        PRAGMA mmap_size = 67108864;
        PRAGMA journal_size_limit = 1048576;
        ",
    );
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value INTEGER NOT NULL
        );
        INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', 1);
        CREATE TABLE IF NOT EXISTS file_state (
            namespace TEXT NOT NULL,
            path TEXT NOT NULL,
            mtime INTEGER NOT NULL,
            size INTEGER NOT NULL,
            PRIMARY KEY(namespace, path)
        ) WITHOUT ROWID;
        ",
    )
    .context("initialize state schema")
}

fn u64_to_db_size(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

fn db_size_to_u64(size: i64) -> u64 {
    u64::try_from(size).unwrap_or(0)
}

/// `(mtime_secs, size)` from already-fetched metadata (no extra stat).
pub fn meta_pair(m: &std::fs::Metadata) -> (i64, u64) {
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (mtime, m.len())
}

/// `(mtime_secs, size)` for `path`, or `None` if it cannot be stat'd.
pub fn meta(path: &Path) -> Option<(i64, u64)> {
    Some(meta_pair(&std::fs::symlink_metadata(path).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bs-state-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("state.sqlite");
        let p = Path::new("/x/y.txt");

        let mut s = State::empty(&file, NAME_NAMESPACE);
        assert!(!s.unchanged(p, 100, 5));
        s.set(p, 100, 5);
        assert!(s.unchanged(p, 100, 5));
        assert!(!s.unchanged(p, 101, 5)); // mtime changed
        assert!(!s.unchanged(p, 100, 6)); // size changed
        s.persist().unwrap();

        // Survives a reload.
        assert!(State::load(&file, NAME_NAMESPACE).unchanged(p, 100, 5));

        // Removal forgets it.
        let mut s2 = State::load(&file, NAME_NAMESPACE);
        s2.remove(p);
        assert!(!s2.unchanged(p, 100, 5));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_pending_state_forgets_persisted_rows_before_persist() {
        // A rebuild clears the cache; until the clear is persisted, lookups must
        // NOT fall through to the still-populated SQLite rows — otherwise the
        // rescan would skip re-adding every wiped document.
        let dir = std::env::temp_dir().join(format!("bs-state-clearmem-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("state.sqlite");
        let path = Path::new("/x/persisted.md");

        let mut seeded = State::empty(&file, NAME_NAMESPACE);
        seeded.set(path, 100, 5);
        seeded.persist().unwrap();

        // contains() must see persisted rows (else sync would re-add duplicate docs)…
        let reloaded = State::load(&file, NAME_NAMESPACE);
        assert!(reloaded.contains(path));
        assert!(reloaded.unchanged(path, 100, 5));

        // …but a cleared (not yet persisted) state must deny both, and report
        // nothing pending for extraction.
        let mut cleared = State::load(&file, NAME_NAMESPACE);
        cleared.clear();
        assert!(!cleared.contains(path), "clear() must hide persisted rows");
        assert!(!cleared.unchanged(path, 100, 5));
        assert!(
            cleared
                .entries_stale_against(CONTENT_NAMESPACE, None, 10)
                .unwrap()
                .is_empty(),
            "cleared state must not stream stale entries"
        );

        // A fresh empty state (clear_before_persist) also denies persisted rows.
        let empty = State::empty(&file, NAME_NAMESPACE);
        assert!(!empty.contains(path));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The pages of the two sides end at different paths; whatever the page
    /// size, the merge must report each difference once and nothing else.
    #[test]
    fn scan_changes_are_the_same_whatever_the_page_size() {
        let dir = std::env::temp_dir().join(format!("bs-state-scan-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = State::empty(&dir.join("state.sqlite"), NAME_NAMESPACE);
        for (path, mtime) in [("/a", 1), ("/b", 1), ("/d", 1), ("/e", 1), ("/g", 1)] {
            state.set(Path::new(path), mtime, 1);
        }
        state.persist().unwrap();
        state.begin_scan().unwrap();
        for (path, mtime) in [("/a", 1), ("/b", 2), ("/c", 1), ("/e", 1), ("/f", 1)] {
            state.mark_seen(Path::new(path), Some((mtime, 1))).unwrap();
        }
        state.mark_seen(Path::new("/g"), None).unwrap();

        let seen = |path: &str, mtime, recorded| ScanChange::Seen {
            path: PathBuf::from(path),
            mtime,
            size: 1,
            recorded,
        };
        let expected = vec![
            seen("/b", 2, Some((1, 1))),
            seen("/c", 1, None),
            ScanChange::Gone(PathBuf::from("/d")),
            seen("/f", 1, None),
        ];
        for limit in 1..=6 {
            let mut changes = Vec::new();
            let mut after = Some(String::new());
            while let Some(cursor) = after {
                let (page, next) = state.scan_changes_after(&cursor, limit).unwrap();
                changes.extend(page);
                after = next;
            }
            assert_eq!(changes, expected, "page size {limit}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn entries_after_merges_pending_changes_with_persisted_rows() {
        let dir = std::env::temp_dir().join(format!("bs-state-paths-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = State::empty(&dir.join("state.sqlite"), crate::state::NAME_NAMESPACE);
        state.set(Path::new("/a/kept.md"), 1, 1);
        state.set(Path::new("/b/removed.md"), 1, 1);
        state.persist().unwrap();

        state.remove(Path::new("/b/removed.md")); // pending removal
        state.set(Path::new("/c/added.md"), 2, 2); // pending addition
        state.set(Path::new("/a/kept.md"), 3, 3); // pending change wins

        let all = state.entries_after(None, 10).unwrap();
        assert_eq!(
            all,
            vec![
                (PathBuf::from("/a/kept.md"), 3, 3),
                (PathBuf::from("/c/added.md"), 2, 2)
            ]
        );
        // Keyset cursor applies to pending entries too.
        let after = state.entries_after(Some("/a/kept.md"), 10).unwrap();
        assert_eq!(after, vec![(PathBuf::from("/c/added.md"), 2, 2)]);
        assert_eq!(state.len(), 2);
        assert!(!state.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_against_content_namespace_streams_pending_entries() {
        let dir = std::env::temp_dir().join(format!("bs-state-stale-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut names = State::empty(&dir.join("state.sqlite"), crate::state::NAME_NAMESPACE);
        let mut content = State::empty(&dir.join("state.sqlite"), crate::state::CONTENT_NAMESPACE);

        names.set(Path::new("/a/extracted.md"), 100, 5);
        names.set(Path::new("/b/changed.md"), 200, 9);
        names.set(Path::new("/c/never-extracted.md"), 300, 7);
        content.set(Path::new("/a/extracted.md"), 100, 5); // up to date
        content.set(Path::new("/b/changed.md"), 150, 9); // stale mtime
        names.persist().unwrap();
        content.persist().unwrap();

        let pending = names
            .entries_stale_against(CONTENT_NAMESPACE, None, 10)
            .unwrap();
        assert_eq!(
            pending,
            vec![
                (PathBuf::from("/b/changed.md"), 200, 9),
                (PathBuf::from("/c/never-extracted.md"), 300, 7),
            ]
        );

        // Keyset pagination resumes strictly after the cursor.
        let resumed = names
            .entries_stale_against(CONTENT_NAMESPACE, Some("/b/changed.md"), 10)
            .unwrap();
        assert_eq!(
            resumed,
            vec![(PathBuf::from("/c/never-extracted.md"), 300, 7)]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn len_tracks_pending_overlaps_and_removals() {
        let dir = std::env::temp_dir().join(format!("bs-state-len-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = State::empty(&dir.join("state.sqlite"), crate::state::NAME_NAMESPACE);
        state.set(Path::new("/p/a.md"), 1, 1);
        state.set(Path::new("/p/b.md"), 1, 1);
        state.persist().unwrap();
        assert_eq!(state.len(), 2);
        assert_eq!(state.pending_len(), 0);

        state.set(Path::new("/p/a.md"), 2, 2); // update of a persisted row: no double count
        state.set(Path::new("/p/c.md"), 1, 1); // new pending rows: +2
        state.set(Path::new("/p/d.md"), 1, 1);
        state.remove(Path::new("/p/b.md")); // persisted removal: -1
        state.remove(Path::new("/p/x.md")); // never persisted: no-op
        assert_eq!(state.pending_len(), 5);
        assert_eq!(state.len(), 3, "a updated, c+d added, b removed, x ignored");
        assert!(!state.is_empty());
        assert_eq!(
            state.entries_after(None, 10).unwrap(),
            vec![
                (PathBuf::from("/p/a.md"), 2, 2),
                (PathBuf::from("/p/c.md"), 1, 1),
                (PathBuf::from("/p/d.md"), 1, 1),
            ]
        );

        state.clear();
        state.set(Path::new("/p/e.md"), 1, 1);
        assert_eq!(state.len(), 1, "clear hides persisted rows, keeps pending");
        assert!(
            state.unseen_paths_after(None, 5).unwrap().is_empty(),
            "a cleared state has no deletions to stream"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn on_disk_state_uses_wal_journal() {
        // tune_connection_for_low_memory switches to WAL; the default rollback
        // journal would double write amplification on every persist.
        let dir = std::env::temp_dir().join(format!("bs-state-wal-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let state = State::empty(&dir.join("state.sqlite"), crate::state::NAME_NAMESPACE);
        assert_eq!(state.journal_mode(), "wal");
        std::fs::remove_dir_all(&dir).ok();
    }
}
