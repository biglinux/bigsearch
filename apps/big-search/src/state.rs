//! Persisted `path → (mtime, size)` cache. Drives incremental dedup (unchanged
//! files are never re-extracted) and deletion detection on a full reconcile.
//! Stored in SQLite so each persist writes only changed rows instead of a full
//! JSON catalog rewrite.
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const STATE_PATH_BATCH: usize = 2048;

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

#[derive(Clone, Copy)]
enum PendingChange {
    Set(i64, u64),
    Remove,
}

impl State {
    /// Load from disk, or start empty if absent/corrupt.
    pub fn load(legacy_json_path: PathBuf) -> Self {
        let db_path = sqlite_path(&legacy_json_path);
        let namespace = state_namespace(&legacy_json_path);
        match open_state_db(&db_path) {
            Ok(conn) => {
                migrate_legacy_json_if_needed(&conn, &namespace, &legacy_json_path).ok();
                State {
                    db_path,
                    namespace,
                    conn,
                    pending: HashMap::new(),
                    clear_before_persist: false,
                    dirty: false,
                }
            }
            Err(_) => State::in_memory(db_path),
        }
    }

    /// Start empty (used for a forced full rebuild).
    pub fn empty(legacy_json_path: PathBuf) -> Self {
        let db_path = sqlite_path(&legacy_json_path);
        let namespace = state_namespace(&legacy_json_path);
        let conn = open_state_db(&db_path).unwrap_or_else(|_| {
            let conn = Connection::open_in_memory().expect("in-memory sqlite state opens");
            tune_connection_for_low_memory(&conn);
            init_schema(&conn).expect("in-memory sqlite state schema initializes");
            conn
        });
        State {
            db_path,
            namespace,
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

    /// Start a low-RAM reconciliation pass. Seen paths are recorded in a SQLite
    /// temporary B-tree instead of a process-wide `HashSet<String>`.
    pub fn begin_scan(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                "
                DROP TABLE IF EXISTS temp.current_scan_seen;
                CREATE TEMP TABLE current_scan_seen (
                    path TEXT PRIMARY KEY
                ) WITHOUT ROWID;
                ",
            )
            .context("initialize scan seen table")
    }

    /// Mark `path` as observed in the current reconciliation pass.
    pub fn mark_seen(&mut self, path: &Path) -> Result<()> {
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO temp.current_scan_seen(path) VALUES (?1)")
            .and_then(|mut stmt| stmt.execute(params![key(path)]))
            .map(|_| ())
            .context("mark path seen")
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
                    INSERT OR IGNORE INTO temp.current_scan_seen(path)
                    SELECT path FROM file_state
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

    /// Persisted paths after `after`, sorted, plus pending changes. This is used by
    /// bounded catalogue backfills instead of materialising the full path list.
    pub fn paths_after(&self, after: Option<&str>, limit: usize) -> Result<Vec<PathBuf>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let after_text = after.unwrap_or_default();
        let mut paths: Vec<String> = if self.clear_before_persist {
            Vec::new()
        } else {
            let mut stmt = self
                .conn
                .prepare(
                    "
                    SELECT path FROM file_state
                    WHERE namespace = ?1 AND path > ?2
                    ORDER BY path
                    LIMIT ?3
                    ",
                )
                .context("prepare path batch query")?;
            let rows = stmt
                .query_map(params![self.namespace, after_text, limit as i64], |row| {
                    row.get::<_, String>(0)
                })
                .context("query path batch")?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for (path, change) in &self.pending {
            if !after_text.is_empty() && path.as_str() <= after_text {
                continue;
            }
            match change {
                PendingChange::Set(_, _) if !paths.iter().any(|known| known == path) => {
                    paths.push(path.clone());
                }
                PendingChange::Set(_, _) => {}
                PendingChange::Remove => paths.retain(|known| known != path),
            }
        }
        paths.sort();
        paths.dedup();
        paths.truncate(limit);
        Ok(paths.into_iter().map(PathBuf::from).collect())
    }

    /// All recorded paths (snapshot). Prefer [`Self::paths_after`] for large catalogues.
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let Ok(batch) = self.paths_after(after.as_deref(), STATE_PATH_BATCH) else {
                break;
            };
            if batch.is_empty() {
                break;
            }
            after = batch.last().map(|path| path.to_string_lossy().into_owned());
            out.extend(batch);
        }
        out
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
        let tx = self.conn.transaction().context("begin state transaction")?;
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

    fn in_memory(db_path: PathBuf) -> Self {
        let conn = Connection::open_in_memory().expect("in-memory sqlite state opens");
        init_schema(&conn).expect("in-memory sqlite state schema initializes");
        State {
            db_path,
            namespace: "memory".to_string(),
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

fn sqlite_path(legacy_json_path: &Path) -> PathBuf {
    legacy_json_path
        .parent()
        .map(|parent| parent.join("state.sqlite"))
        .unwrap_or_else(|| legacy_json_path.with_extension("sqlite"))
}

fn state_namespace(legacy_json_path: &Path) -> String {
    match legacy_json_path
        .file_stem()
        .and_then(|value| value.to_str())
    {
        Some("content-state") => CONTENT_NAMESPACE.to_string(),
        _ => NAME_NAMESPACE.to_string(),
    }
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

fn migrate_legacy_json_if_needed(
    conn: &Connection,
    namespace: &str,
    legacy_json_path: &Path,
) -> Result<()> {
    let migration_key = format!("legacy_migrated:{namespace}");
    let has_migrated: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM meta WHERE key = ?1",
            params![migration_key],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if has_migrated > 0 {
        return Ok(());
    }
    if !legacy_json_path.exists() {
        return Ok(());
    }
    let existing_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM file_state WHERE namespace = ?1",
            params![namespace],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if existing_rows > 0 {
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, 1)",
            params![migration_key],
        )
        .context("mark legacy state migrated")?;
        return Ok(());
    }
    let bytes = std::fs::read(legacy_json_path).context("read legacy state json")?;
    let map: HashMap<String, (i64, u64)> =
        serde_json::from_slice(&bytes).context("parse legacy state json")?;
    let tx = conn
        .unchecked_transaction()
        .context("begin legacy state migration")?;
    for (path, (mtime, size)) in map {
        tx.execute(
            "INSERT OR REPLACE INTO file_state(namespace, path, mtime, size)
             VALUES (?1, ?2, ?3, ?4)",
            params![namespace, path, mtime, u64_to_db_size(size)],
        )?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, 1)",
        params![migration_key],
    )?;
    tx.commit().context("commit legacy state migration")
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
        let file = dir.join("state.json");
        let p = Path::new("/x/y.txt");

        let mut s = State::empty(file.clone());
        assert!(!s.unchanged(p, 100, 5));
        s.set(p, 100, 5);
        assert!(s.unchanged(p, 100, 5));
        assert!(!s.unchanged(p, 101, 5)); // mtime changed
        assert!(!s.unchanged(p, 100, 6)); // size changed
        s.persist().unwrap();

        // Survives a reload.
        assert!(State::load(file.clone()).unchanged(p, 100, 5));

        // Removal forgets it.
        let mut s2 = State::load(file);
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
        let file = dir.join("state.json");
        let path = Path::new("/x/persisted.md");

        let mut seeded = State::empty(file.clone());
        seeded.set(path, 100, 5);
        seeded.persist().unwrap();

        // contains() must see persisted rows (else sync would re-add duplicate docs)…
        let reloaded = State::load(file.clone());
        assert!(reloaded.contains(path));
        assert!(reloaded.unchanged(path, 100, 5));

        // …but a cleared (not yet persisted) state must deny both, and report
        // nothing pending for extraction.
        let mut cleared = State::load(file.clone());
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
        let empty = State::empty(file);
        assert!(!empty.contains(path));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paths_after_merges_pending_changes_with_persisted_rows() {
        let dir = std::env::temp_dir().join(format!("bs-state-paths-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = State::empty(dir.join("state.json"));
        state.set(Path::new("/a/kept.md"), 1, 1);
        state.set(Path::new("/b/removed.md"), 1, 1);
        state.persist().unwrap();

        state.remove(Path::new("/b/removed.md")); // pending removal
        state.set(Path::new("/c/added.md"), 2, 2); // pending addition

        let all = state.paths_after(None, 10).unwrap();
        assert_eq!(
            all,
            vec![PathBuf::from("/a/kept.md"), PathBuf::from("/c/added.md")]
        );
        // Keyset cursor applies to pending entries too.
        let after = state.paths_after(Some("/a/kept.md"), 10).unwrap();
        assert_eq!(after, vec![PathBuf::from("/c/added.md")]);
        assert_eq!(state.len(), 2);
        assert!(!state.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_against_content_namespace_streams_pending_entries() {
        let dir = std::env::temp_dir().join(format!("bs-state-stale-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut names = State::empty(dir.join("state.json"));
        let mut content = State::empty(dir.join("content-state.json"));

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
    fn migrates_legacy_json_once() {
        let dir = std::env::temp_dir().join(format!("bs-state-migrate-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let name_legacy_file = dir.join("state.json");
        let legacy_file = dir.join("content-state.json");
        let name_path = Path::new("/legacy/name-only.md");
        let path = Path::new("/legacy/file.md");
        let mut name_legacy = HashMap::new();
        name_legacy.insert(key(name_path), (100, 6));
        std::fs::write(&name_legacy_file, serde_json::to_vec(&name_legacy).unwrap()).unwrap();
        let mut legacy = HashMap::new();
        legacy.insert(key(path), (200, 12));
        std::fs::write(&legacy_file, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let name_state = State::load(name_legacy_file);
        let state = State::load(legacy_file.clone());
        assert!(name_state.unchanged(name_path, 100, 6));
        assert!(!name_state.unchanged(path, 200, 12));
        assert!(state.unchanged(path, 200, 12));
        assert!(dir.join("state.sqlite").exists());

        std::fs::write(&legacy_file, b"not json").unwrap();
        assert!(State::load(legacy_file).unchanged(path, 200, 12));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_does_not_reimport_legacy_json() {
        let dir = std::env::temp_dir().join(format!("bs-state-clear-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let legacy_file = dir.join("content-state.json");
        let path = Path::new("/legacy/file.md");
        let mut legacy = HashMap::new();
        legacy.insert(key(path), (200, 12));
        std::fs::write(&legacy_file, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let mut state = State::load(legacy_file.clone());
        assert!(state.unchanged(path, 200, 12));
        state.clear();
        state.persist().unwrap();

        assert!(!State::load(legacy_file).unchanged(path, 200, 12));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn len_tracks_pending_overlaps_and_removals() {
        let dir = std::env::temp_dir().join(format!("bs-state-len-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = State::empty(dir.join("state.json"));
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
            state.paths(),
            vec![
                PathBuf::from("/p/a.md"),
                PathBuf::from("/p/c.md"),
                PathBuf::from("/p/d.md"),
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
    fn existing_sqlite_rows_win_over_legacy_json() {
        let dir = std::env::temp_dir().join(format!("bs-state-sqlwins-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let legacy_file = dir.join("state.json");
        let kept = Path::new("/p/kept.md");
        let stale = Path::new("/p/stale.md");

        let mut seeded = State::empty(legacy_file.clone());
        seeded.set(kept, 1, 1);
        seeded.persist().unwrap();
        drop(seeded);

        // A legacy JSON appearing after SQLite already has rows must be ignored,
        // not merged over the newer catalogue.
        let mut legacy = HashMap::new();
        legacy.insert(key(stale), (2i64, 2u64));
        std::fs::write(&legacy_file, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let state = State::load(legacy_file);
        assert!(state.unchanged(kept, 1, 1));
        assert!(!state.contains(stale), "legacy import must be skipped");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn on_disk_state_uses_wal_journal() {
        // tune_connection_for_low_memory switches to WAL; the default rollback
        // journal would double write amplification on every persist.
        let dir = std::env::temp_dir().join(format!("bs-state-wal-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let state = State::empty(dir.join("state.json"));
        assert_eq!(state.journal_mode(), "wal");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_legacy_json_starts_empty() {
        let dir = std::env::temp_dir().join(format!("bs-state-corrupt-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let legacy_file = dir.join("state.json");
        std::fs::write(&legacy_file, b"{").unwrap();

        let state = State::load(legacy_file);
        assert!(state.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
