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
    /// Folders removed with everything under them since the last persist. Their
    /// rows are dropped as one key range when it runs, never read into memory.
    removed_trees: Vec<String>,
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
                removed_trees: Vec::new(),
                clear_before_persist: false,
                dirty: false,
            },
            Err(error) => {
                log::warn!(
                    "state database {} unavailable, nothing will be saved: {error:#}",
                    db_path.display()
                );
                State::in_memory(db_path.to_path_buf(), namespace)
            }
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
            removed_trees: Vec::new(),
            clear_before_persist: true,
            dirty: true,
        }
    }

    /// Whether `path` is recorded with this exact mtime+size (→ unchanged).
    pub fn unchanged(&self, path: &Path, mtime: i64, size: u64) -> bool {
        self.recorded(path) == Some((mtime, size))
    }

    /// The mtime and size `path` is catalogued with, or `None` when it is not
    /// catalogued. One lookup answers both of the scanner's questions: is it
    /// unchanged, and is it known at all.
    pub fn recorded(&self, path: &Path) -> Option<(i64, u64)> {
        let path_key = key(path);
        match self.pending.get(&path_key).copied() {
            Some(PendingChange::Set(mtime, size)) => Some((mtime, size)),
            Some(PendingChange::Remove) => None,
            None if self.clear_before_persist || self.in_removed_tree(&path_key) => None,
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
            None if self.clear_before_persist || self.in_removed_tree(&path_key) => false,
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

    /// The persisted paths directly inside `dir`.
    pub fn children(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let (low, high) = subtree_range(&key(dir));
        // substr() and length() both count characters, so they agree with
        // each other whatever the path holds.
        let mut statement = self.conn.prepare_cached(
            "SELECT path FROM file_state
              WHERE namespace = ?1 AND path >= ?2 AND path < ?3
                AND instr(substr(path, length(?2) + 1), '/') = 0",
        )?;
        let rows = statement.query_map(params![self.namespace, low, high], |row| {
            row.get::<_, String>(0)
        })?;
        Ok(rows.flatten().map(PathBuf::from).collect())
    }

    /// Forget `path` and every path under it. A folder moved away, to the trash
    /// included, is one event naming the folder and none for what it held.
    pub fn remove_tree(&mut self, path: &Path) {
        let path_key = key(path);
        let (low, high) = subtree_range(&path_key);
        self.pending
            .retain(|key, _| !(key.as_str() >= low.as_str() && key.as_str() < high.as_str()));
        self.pending.insert(path_key.clone(), PendingChange::Remove);
        self.removed_trees.push(path_key);
        self.dirty = true;
    }

    /// Forget every entry. A `Rebuild` clears the index, so the cache must agree
    /// or unchanged-checks would skip re-indexing the wiped documents.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.removed_trees.clear();
        self.clear_before_persist = true;
        self.dirty = true;
    }

    /// Number of not-yet-flushed row changes. The scanner uses this to keep the
    /// pending map bounded on machines with little memory.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Run a reconciliation pass in one transaction. Each lookup and seen-mark
    /// would otherwise take and release the database's locks on its own, two
    /// `fcntl` and an `fstat` apiece. Batches persisted meanwhile commit with
    /// the pass, and a failed pass rolls all of them back.
    pub fn scan_pass<T>(&mut self, pass: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.conn
            .execute_batch("BEGIN")
            .context("begin scan transaction")?;
        let result = pass(self);
        if result.is_err() {
            // The caller has not committed the index either: keeping the
            // catalogue's half would record work the index never got.
            let _ = self.conn.execute_batch("ROLLBACK");
            return result;
        }
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
    /// Both lists are read a page at a time in path order and merged here,
    /// where a lookup per path would cost three B-tree descents with the
    /// seen-mark and the deletion check. The catalogue must be persisted when
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
            let (low, high) = subtree_range(&prefix_text);
            // A key range: length() and substr() count characters and would
            // miss the descendants of a non-ASCII prefix.
            self.conn
                .execute(
                    "
                    INSERT OR IGNORE INTO temp.current_scan_seen(path, mtime, size)
                    SELECT path, mtime, size FROM file_state
                    WHERE namespace = ?1
                      AND (path = ?2 OR (path >= ?3 AND path < ?4))
                    ",
                    params![self.namespace, prefix_text, low, high],
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

    /// Paths of this namespace (ordered, strictly after `after`) absent from
    /// `other_namespace`: the mirror of [`Self::entries_stale_against`].
    ///
    /// Deleting a file drops only its catalogue row; this finds the content
    /// rows and documents that would keep answering for paths that are gone.
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
        // Ranges first: a path set again after its folder left must survive.
        for dir in &self.removed_trees {
            let (low, high) = subtree_range(dir);
            tx.execute(
                "DELETE FROM file_state WHERE namespace = ?1 AND path >= ?2 AND path < ?3",
                params![self.namespace, low, high],
            )
            .with_context(|| format!("delete state tree {dir}"))?;
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
        self.removed_trees.clear();
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
            removed_trees: Vec::new(),
            clear_before_persist: false,
            dirty: false,
        }
    }

    /// Whether `path_key` lies under a folder removed since the last persist,
    /// so its persisted row does not count.
    fn in_removed_tree(&self, path_key: &str) -> bool {
        self.removed_trees.iter().any(|dir| {
            path_key
                .strip_prefix(dir.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        })
    }
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The keys strictly under `dir`: text compares bytewise and '0' is the byte
/// after '/'.
fn subtree_range(dir: &str) -> (String, String) {
    (format!("{dir}/"), format!("{dir}0"))
}

fn open_state_db(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db_path).with_context(|| format!("open {}", db_path.display()))?;
    // The daemon's scan pass holds the write lock for its whole walk; a CLI
    // command opening the same database waits for it rather than failing.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    tune_connection_for_low_memory(&conn);
    init_schema(&conn)?;
    Ok(conn)
}

fn tune_connection_for_low_memory(conn: &Connection) {
    // Keep SQLite's page cache small and let large scans spill temp
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
mod tests;
