//! Lightweight file origin/provenance store. Search remains owned by Tantivy;
//! this SQLite sidecar is consulted only for explicit origin requests or when a
//! query asks for origin summaries.
use anyhow::{Context, Result};
use big_indexd_client::{Hit, OriginProvenance, OriginSummary};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

mod downloads;

pub(crate) use downloads::domain_from_url;
use downloads::{read_xdg_origin_domain, should_probe_xdg_origin};

const SCHEMA_VERSION: &str = "1";
const ORIGIN_CAPABILITY: &str = "file_origin";
const ORIGIN_REGISTER_CAPABILITY: &str = "file_origin_register";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginKind {
    WebDownload,
    RemovableCopy,
    NetworkCopy,
    LocalCopy,
    AppCreated,
    DerivedFile,
    MovedOrRenamed,
    Unknown,
}

impl OriginKind {
    fn as_str(self) -> &'static str {
        match self {
            OriginKind::WebDownload => "web_download",
            OriginKind::RemovableCopy => "removable_copy",
            OriginKind::NetworkCopy => "network_copy",
            OriginKind::LocalCopy => "local_copy",
            OriginKind::AppCreated => "app_created",
            OriginKind::DerivedFile => "derived_file",
            OriginKind::MovedOrRenamed => "moved_or_renamed",
            OriginKind::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginConfidence {
    Low = 1,
    Medium = 2,
    High = 3,
}

#[derive(Clone, Debug)]
struct OriginRecord {
    path: String,
    dev: Option<i64>,
    ino: Option<i64>,
    kind: OriginKind,
    confidence: OriginConfidence,
    source_app: Option<String>,
    source_path: Option<String>,
    source_device_id: Option<String>,
    source_device_label: Option<String>,
    source_domain: Option<String>,
    first_seen_at: i64,
    updated_at: i64,
}

impl OriginRecord {
    /// A file at `path`, seen arriving at `at`, with nothing known yet of
    /// where it came from.
    fn arrived(path: &Path, kind: OriginKind, confidence: OriginConfidence, at: i64) -> Self {
        let (dev, ino) = dev_ino(path);
        Self {
            path: path_key(path),
            dev,
            ino,
            kind,
            confidence,
            source_app: None,
            source_path: None,
            source_device_id: None,
            source_device_label: None,
            source_domain: None,
            first_seen_at: at,
            updated_at: at,
        }
    }

    /// The sentence the command line prints, built when the record is read so
    /// that nothing in one language is stored. The desktop builds its own,
    /// translated, from `kind` and the fields.
    fn label(&self) -> String {
        let device = self.source_device_label.as_deref();
        match self.kind {
            OriginKind::WebDownload => match &self.source_domain {
                Some(domain) => format!("Downloaded from {domain}"),
                None => "Downloaded from the internet".to_owned(),
            },
            OriginKind::RemovableCopy => match device {
                Some(name) => format!("Copied from the removable drive {name}"),
                None => "Copied from a removable drive".to_owned(),
            },
            OriginKind::NetworkCopy => match device {
                Some(name) => format!("Copied from the network share {name}"),
                None => "Copied from a network share".to_owned(),
            },
            OriginKind::LocalCopy => "Copied on this computer".to_owned(),
            OriginKind::AppCreated => {
                format!("Created by {}", app_label(self.source_app.as_deref()))
            }
            OriginKind::DerivedFile => {
                let source = self.source_path.as_deref().unwrap_or_default();
                let name = Path::new(source)
                    .file_name()
                    .map_or_else(|| source.to_owned(), |n| n.to_string_lossy().into_owned());
                format!("Made from {name}")
            }
            OriginKind::MovedOrRenamed => "Moved or renamed on this computer".to_owned(),
            OriginKind::Unknown => "Origin unknown".to_owned(),
        }
    }

    fn into_provenance(self) -> OriginProvenance {
        OriginProvenance {
            kind: self.kind.as_str().to_string(),
            confidence: self.confidence as u8,
            label: self.label(),
            first_seen_at: self.first_seen_at,
            source_app: self.source_app,
            source_path: self.source_path,
            source_device_id: self.source_device_id,
            source_device_label: self.source_device_label,
            source_domain: self.source_domain,
        }
    }
}

#[derive(Debug)]
pub struct OriginSettings {
    pub enabled: bool,
    pub xattr_enabled: bool,
    pub watch_enabled: bool,
    pub query_enabled: bool,
    /// Read browsers' download lists to learn which site a file came from.
    /// Opt-in: that list belongs to the browser.
    pub browser_history: bool,
    pub max_batch: usize,
}

impl OriginSettings {
    /// The configuration file decides; the environment only overrides it.
    ///
    /// The desktop's switch for "remember where files came from" writes the
    /// file, so a value pinned in the service unit would leave that switch wired
    /// to nothing. The variables are for a developer running the service by hand.
    fn load() -> Self {
        let configured = big_search_config::read().origin;
        Self {
            enabled: env_bool("BIG_SEARCH_ORIGIN", configured.enabled),
            xattr_enabled: env_bool("BIG_SEARCH_ORIGIN_XATTR", true),
            watch_enabled: env_bool("BIG_SEARCH_ORIGIN_WATCH", configured.enabled),
            query_enabled: env_bool("BIG_SEARCH_ORIGIN_QUERY", true),
            browser_history: env_bool(
                "BIG_SEARCH_ORIGIN_BROWSER_HISTORY",
                configured.browser_history,
            ),
            max_batch: std::env::var("BIG_SEARCH_ORIGIN_MAX_BATCH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(256),
        }
    }
}

pub fn settings() -> &'static OriginSettings {
    static SETTINGS: OnceLock<OriginSettings> = OnceLock::new();
    SETTINGS.get_or_init(OriginSettings::load)
}

pub fn capabilities() -> Vec<String> {
    if settings().enabled {
        vec![
            ORIGIN_CAPABILITY.to_string(),
            ORIGIN_REGISTER_CAPABILITY.to_string(),
        ]
    } else {
        Vec::new()
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    crate::settings::env_bool(name).unwrap_or(default)
}

pub struct OriginWriter {
    conn: Option<Connection>,
}

impl OriginWriter {
    /// The writer for the origin database at `path`; one that records nothing
    /// when file origin is turned off.
    ///
    /// # Errors
    ///
    /// Returns an error when the origin database cannot be opened or its schema created.
    pub fn open(path: PathBuf) -> Result<Self> {
        if !settings().enabled {
            return Ok(Self { conn: None });
        }
        let conn =
            open_writer(&path).with_context(|| format!("open origin db {}", path.display()))?;
        Ok(Self { conn: Some(conn) })
    }

    pub fn disabled() -> Self {
        Self { conn: None }
    }

    /// Records that the program `app_id` created `path`.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` is relative, holds `..` or leads outside the configured
    /// sources, or the database cannot be written.
    pub fn register_created(&self, path: &Path, app_id: Option<&str>) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let path = key_under_source(path)?;
        let record = OriginRecord {
            source_app: app_id.map(str::to_string),
            ..OriginRecord::arrived(
                &path,
                OriginKind::AppCreated,
                OriginConfidence::High,
                now_secs(),
            )
        };
        upsert_record(conn, &record)
    }

    /// Records that `dst` is a copy of `src`.
    ///
    /// # Errors
    ///
    /// Returns an error when `dst` is relative, holds `..` or leads outside the configured
    /// sources, `src` is relative or holds `..`, or the database cannot be written.
    pub fn register_copy(&self, src: &Path, dst: &Path, app_id: Option<&str>) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let dst = key_under_source(dst)?;
        let src = spelled_source(src)?;
        let source = crate::settings::sources().owner(&src);
        let (kind, source_device_id, source_device_label) = match source {
            Some(source) if source.removable() => (
                OriginKind::RemovableCopy,
                Some(source.id.clone()),
                Some(source.name.clone()),
            ),
            Some(source) if source.network() => (
                OriginKind::NetworkCopy,
                Some(source.id.clone()),
                Some(source.name.clone()),
            ),
            _ => (OriginKind::LocalCopy, None, None),
        };
        let record = OriginRecord {
            source_app: app_id.map(str::to_string),
            source_path: Some(path_key(&src)),
            source_device_id,
            source_device_label,
            ..OriginRecord::arrived(&dst, kind, OriginConfidence::High, now_secs())
        };
        upsert_record(conn, &record)
    }

    /// Records that `src` was moved or renamed to `dst`.
    ///
    /// # Errors
    ///
    /// Returns an error when `dst` is relative, holds `..` or leads outside the configured
    /// sources, `src` is relative or holds `..`, or the database cannot be written.
    pub fn register_move(&self, src: &Path, dst: &Path, app_id: Option<&str>) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let dst = key_under_source(dst)?;
        let now = now_secs();
        let src_key = path_key(&spelled_source(src)?);
        let dst_key = path_key(&dst);
        let (dev, ino) = dev_ino(&dst);

        if let Some(mut record) = select_record(conn, &src_key)? {
            record.path = dst_key;
            record.dev = dev;
            record.ino = ino;
            record.updated_at = now;
            upsert_record(conn, &record)?;
            if src_key != record.path {
                conn.execute("DELETE FROM file_origin WHERE path = ?1", params![src_key])?;
            }
            return Ok(());
        }

        let record = OriginRecord {
            source_app: app_id.map(str::to_string),
            source_path: Some(src_key),
            ..OriginRecord::arrived(
                &dst,
                OriginKind::MovedOrRenamed,
                OriginConfidence::Medium,
                now,
            )
        };
        upsert_record(conn, &record)
    }

    /// Records that `dst` was made from `src`.
    ///
    /// # Errors
    ///
    /// Returns an error when `dst` is relative, holds `..` or leads outside the configured
    /// sources, `src` is relative or holds `..`, or the database cannot be written.
    pub fn register_derived(&self, src: &Path, dst: &Path, app_id: Option<&str>) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let dst = key_under_source(dst)?;
        let record = OriginRecord {
            source_app: app_id.map(str::to_string),
            source_path: Some(path_key(&spelled_source(src)?)),
            ..OriginRecord::arrived(
                &dst,
                OriginKind::DerivedFile,
                OriginConfidence::High,
                now_secs(),
            )
        };
        upsert_record(conn, &record)
    }

    pub(crate) fn register_web_download(&self, path: &Path, domain: &str) -> Result<()> {
        self.register_web_download_at(path, domain, now_secs())
    }

    /// The same, for a download whose date is known: from the list a browser
    /// keeps, where the file may have arrived months ago.
    pub(crate) fn register_web_download_at(
        &self,
        path: &Path,
        domain: &str,
        downloaded_at: i64,
    ) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let path = key_under_source(path)?;
        let record = OriginRecord {
            source_domain: Some(domain.to_string()),
            first_seen_at: downloaded_at,
            ..OriginRecord::arrived(
                &path,
                OriginKind::WebDownload,
                OriginConfidence::High,
                now_secs(),
            )
        };
        upsert_record(conn, &record)
    }

    /// The same for a whole list, in one transaction.
    ///
    /// A browser's download list is thousands of rows, read in one go when the
    /// service starts. One at a time is one commit each against a database that
    /// syncs, thousands of them before the first file is even indexed.
    ///
    /// Returns how many were recorded.
    pub(crate) fn register_web_downloads_at(
        &self,
        downloads: &[(PathBuf, String, i64)],
    ) -> Result<usize> {
        let Some(conn) = &self.conn else {
            return Ok(0);
        };
        // `unchecked_transaction` because the connection is behind `&self`; the
        // store is single-threaded, which is what makes that safe here.
        let transaction = conn.unchecked_transaction()?;
        let mut recorded = 0;
        for (path, domain, downloaded_at) in downloads {
            if self
                .register_web_download_at(path, domain, *downloaded_at)
                .is_ok()
                && conn.changes() > 0
            {
                recorded += 1;
            }
        }
        transaction.commit()?;
        Ok(recorded)
    }

    pub fn observe_recent_path(&self, path: &Path) {
        if !settings().enabled || !settings().watch_enabled || !settings().xattr_enabled {
            return;
        }
        if !should_probe_xdg_origin(path) {
            return;
        }
        let Some(domain) = read_xdg_origin_domain(path) else {
            return;
        };
        if let Err(e) = self.register_web_download(path, &domain) {
            log::debug!(
                "origin xattr registration failed for {}: {e:#}",
                path.display()
            );
        }
    }
}

pub struct OriginReader {
    path: PathBuf,
    conn: RefCell<Option<Connection>>,
}

impl OriginReader {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            conn: RefCell::new(None),
        }
    }

    pub fn origin(&self, path: &Path) -> Option<OriginProvenance> {
        if !settings().enabled || !settings().query_enabled {
            return None;
        }
        self.with_connection(|conn| select_record(conn, &path_key(path)).ok().flatten())
            .flatten()
            .map(OriginRecord::into_provenance)
    }

    pub fn attach_summaries(&self, hits: &mut [Hit]) {
        if hits.is_empty() || !settings().enabled || !settings().query_enabled {
            return;
        }
        let paths: Vec<String> = hits.iter().map(|hit| hit.path.clone()).collect();
        let Some(records) = self.with_connection(|conn| select_records(conn, &paths).ok()) else {
            return;
        };
        let Some(records) = records else {
            return;
        };
        for hit in hits {
            if let Some(origin_provenance) = records.get(&hit.path).cloned() {
                hit.origin = Some(OriginSummary::from(origin_provenance));
            }
        }
    }

    fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> T) -> Option<T> {
        if !self.path.exists() {
            return None;
        }
        if self.conn.borrow().is_none() {
            let conn = open_reader(&self.path).ok()?;
            *self.conn.borrow_mut() = Some(conn);
        }
        let conn = self.conn.borrow();
        Some(f(conn.as_ref()?))
    }
}

/// Forget every origin ever recorded in the database at `path`, also when
/// recording is turned off: what was recorded before is still on disk.
///
/// Rows are deleted rather than the file: the watcher may hold its own
/// connection and would keep writing into an unlinked inode. `VACUUM`
/// afterwards because forgetting has to mean the rows are gone from the file,
/// not merely unreferenced inside it.
pub(crate) fn forget_all(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let conn = open_writer(path).with_context(|| format!("open origin db {}", path.display()))?;
    conn.execute("DELETE FROM file_origin", [])?;
    conn.execute_batch("VACUUM")?;
    log::info!("every recorded file origin was forgotten");
    Ok(())
}

fn open_writer(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create origin dir {}", parent.display()))?;
    }
    let conn = Connection::open(path)?;
    configure_connection(&conn)?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', '1');
        CREATE TABLE IF NOT EXISTS file_origin (
            path TEXT PRIMARY KEY,
            dev INTEGER,
            ino INTEGER,
            kind TEXT NOT NULL,
            confidence INTEGER NOT NULL,
            label TEXT NOT NULL,
            source_app TEXT,
            source_path TEXT,
            source_device_id TEXT,
            source_device_label TEXT,
            source_domain TEXT,
            first_seen_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_file_origin_dev_ino
            ON file_origin(dev, ino);
        CREATE INDEX IF NOT EXISTS idx_file_origin_kind_time
            ON file_origin(kind, first_seen_at DESC);
        ",
    )?;
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
        params![SCHEMA_VERSION],
    )?;
    // No `PRAGMA optimize`: each registration opens this database, and the
    // ANALYZE it ran failed some with "database is locked", without the busy
    // wait, beside another connection.
    Ok(conn)
}

fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    configure_reader_connection(&conn)?;
    Ok(conn)
}

/// How long a writer waits for another writer's transaction. Two write here:
/// the watcher, at idle priority, and the connection threads that register
/// what desktop apps report. On a busy machine the scheduler can leave an
/// idle-priority thread off the CPU for a second in the middle of its
/// transaction, and a 25 ms wait then failed the app's registration
/// ("database is locked"), losing the origin. Each connection has its own
/// thread, so waiting holds up only the app that asked.
const WRITER_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.busy_timeout(WRITER_BUSY_TIMEOUT)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA cache_size = -1024;
        PRAGMA wal_autocheckpoint = 256;
        PRAGMA journal_size_limit = 1048576;
        PRAGMA foreign_keys = ON;
        ",
    )?;
    Ok(())
}

fn configure_reader_connection(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(25))?;
    conn.execute_batch(
        "
        PRAGMA query_only = ON;
        PRAGMA cache_size = -1024;
        PRAGMA foreign_keys = ON;
        ",
    )?;
    Ok(())
}

/// Insert or replace the record for `record.path`.
///
/// A web download never replaces a record of another kind: the browser's list
/// and the xattr only name a path, and a file that took a deleted download's
/// name keeps the origin it was given when it arrived.
fn upsert_record(conn: &Connection, record: &OriginRecord) -> Result<()> {
    conn.execute(
        "
        INSERT INTO file_origin (
            path, dev, ino, kind, confidence, label, source_app, source_path,
            source_device_id, source_device_label, source_domain, first_seen_at,
            updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        ON CONFLICT(path) DO UPDATE SET
            dev = excluded.dev,
            ino = excluded.ino,
            kind = excluded.kind,
            confidence = excluded.confidence,
            label = excluded.label,
            source_app = excluded.source_app,
            source_path = excluded.source_path,
            source_device_id = excluded.source_device_id,
            source_device_label = excluded.source_device_label,
            source_domain = excluded.source_domain,
            first_seen_at = excluded.first_seen_at,
            updated_at = excluded.updated_at
        WHERE excluded.kind <> 'web_download' OR file_origin.kind = 'web_download'
        ",
        params![
            record.path,
            record.dev,
            record.ino,
            record.kind.as_str(),
            record.confidence as u8,
            // `label()` builds the sentence on read; the column stays, empty,
            // for the older builds that still read it.
            "",
            record.source_app,
            record.source_path,
            record.source_device_id,
            record.source_device_label,
            record.source_domain,
            record.first_seen_at,
            record.updated_at,
        ],
    )?;
    Ok(())
}

/// The columns `row_to_record` reads, in its positional order: a query listing
/// them differently would silently swap fields.
const RECORD_COLUMNS: &str = "path, dev, ino, kind, confidence, source_app, \
     source_path, source_device_id, source_device_label, source_domain, \
     first_seen_at, updated_at";

fn select_record(conn: &Connection, path: &str) -> Result<Option<OriginRecord>> {
    let by_path = conn
        .query_row(
            &format!("SELECT {RECORD_COLUMNS} FROM file_origin WHERE path = ?1"),
            params![path],
            row_to_record,
        )
        .optional()?;
    if by_path.is_some() {
        return Ok(by_path);
    }
    // Renamed, or moved by something that did not tell us. The file is the same
    // file (same device, same inode), and where it came from did not change
    // because somebody gave it a different name.
    select_by_identity(conn, Path::new(path))
}

/// The record for whatever file now sits at `path`, found by its identity on
/// disk rather than by its name.
fn select_by_identity(conn: &Connection, path: &Path) -> Result<Option<OriginRecord>> {
    let (Some(dev), Some(ino)) = dev_ino(path) else {
        return Ok(None);
    };
    conn.query_row(
        &format!(
            "SELECT {RECORD_COLUMNS} FROM file_origin
              WHERE dev = ?1 AND ino = ?2 ORDER BY updated_at DESC LIMIT 1"
        ),
        params![dev, ino],
        row_to_record,
    )
    .optional()
    .map_err(Into::into)
}

/// How many unmatched paths are looked up by identity (one `stat` each) per
/// request; `-a --origin` can ask about a hundred thousand at once.
const IDENTITY_FALLBACK_LIMIT: usize = 256;

fn select_records(
    conn: &Connection,
    paths: &[String],
) -> Result<HashMap<String, OriginProvenance>> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = (0..paths.len()).map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT {RECORD_COLUMNS} FROM file_origin WHERE path IN ({placeholders})");
    // ast-grep-ignore: rust-sql-format (only the column constant and "?" placeholders are formatted; every value is bound)
    let mut stmt = conn.prepare(&sql)?;
    let records = stmt.query_map(params_from_iter(paths.iter()), row_to_record)?;
    let mut by_path = HashMap::with_capacity(paths.len());
    for record in records {
        let record = record?;
        by_path.insert(record.path.clone(), record.into_provenance());
    }
    attach_by_identity(conn, paths, &mut by_path)?;
    Ok(by_path)
}

/// Fill in the paths the name lookup missed by device and inode, filed under
/// the name asked for, so a renamed file keeps its origin.
///
/// A downloaded document renamed in another program keeps its origin: the row
/// still holds the earlier name, and the file is the same file (same device,
/// same inode). The record found this way is filed under the name that was
/// asked for, not the stale one it is stored under.
///
/// The price, the same the properties dialog pays: on a filesystem that reuses
/// inode numbers, a deleted download's origin can land on an unrelated new file.
fn attach_by_identity(
    conn: &Connection,
    paths: &[String],
    found: &mut HashMap<String, OriginProvenance>,
) -> Result<()> {
    let missing: Vec<&String> = paths
        .iter()
        .filter(|path| !found.contains_key(*path))
        .take(IDENTITY_FALLBACK_LIMIT)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let identities: Vec<(&String, i64, i64)> = missing
        .into_iter()
        .filter_map(|path| match dev_ino(Path::new(path)) {
            (Some(dev), Some(ino)) => Some((path, dev, ino)),
            _ => None,
        })
        .collect();
    if identities.is_empty() {
        return Ok(());
    }
    let placeholders = (0..identities.len())
        .map(|_| "(?, ?)")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT {RECORD_COLUMNS} FROM file_origin
          WHERE (dev, ino) IN ({placeholders})"
    );
    // ast-grep-ignore: rust-sql-format (only the column constant and "?" placeholders are formatted; every value is bound)
    let mut stmt = conn.prepare(&sql)?;
    let mut values: Vec<i64> = Vec::with_capacity(identities.len() * 2);
    for (_, dev, ino) in &identities {
        values.push(*dev);
        values.push(*ino);
    }
    let records = stmt.query_map(params_from_iter(values.iter()), row_to_record)?;
    let mut by_identity: HashMap<(i64, i64), OriginRecord> = HashMap::new();
    for record in records {
        let record = record?;
        if let (Some(dev), Some(ino)) = (record.dev, record.ino) {
            by_identity.insert((dev, ino), record);
        }
    }
    for (path, dev, ino) in identities {
        if let Some(record) = by_identity.get(&(dev, ino)) {
            found.insert(path.clone(), record.clone().into_provenance());
        }
    }
    Ok(())
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<OriginRecord> {
    let kind_text: String = row.get(3)?;
    let confidence: u8 = row.get(4)?;
    Ok(OriginRecord {
        path: row.get(0)?,
        dev: row.get(1)?,
        ino: row.get(2)?,
        kind: parse_kind(&kind_text),
        confidence: parse_confidence(confidence),
        source_app: row.get(5)?,
        source_path: row.get(6)?,
        source_device_id: row.get(7)?,
        source_device_label: row.get(8)?,
        source_domain: row.get(9)?,
        first_seen_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn parse_kind(kind: &str) -> OriginKind {
    match kind {
        "web_download" => OriginKind::WebDownload,
        "removable_copy" => OriginKind::RemovableCopy,
        "network_copy" => OriginKind::NetworkCopy,
        "local_copy" => OriginKind::LocalCopy,
        "app_created" => OriginKind::AppCreated,
        "derived_file" => OriginKind::DerivedFile,
        "moved_or_renamed" => OriginKind::MovedOrRenamed,
        _ => OriginKind::Unknown,
    }
}

fn parse_confidence(confidence: u8) -> OriginConfidence {
    match confidence {
        3 => OriginConfidence::High,
        2 => OriginConfidence::Medium,
        _ => OriginConfidence::Low,
    }
}

/// The key of a file whose origin is recorded: under a source, symlinks
/// resolved as the catalogue keys it.
fn key_under_source(path: &Path) -> Result<PathBuf> {
    crate::settings::sources()
        .resolved_client_path(path)
        .with_context(|| {
            format!(
                "origin path is relative, holds .. or is outside the configured sources: {}",
                path.display()
            )
        })
}

/// The file another one came from: anywhere, a device included, keyed as its
/// row would be so a move finds it.
fn spelled_source(path: &Path) -> Result<PathBuf> {
    crate::settings::sources()
        .client_row_key(path)
        .with_context(|| format!("origin source is relative or holds ..: {}", path.display()))
}

fn app_label(app_id: Option<&str>) -> String {
    match app_id {
        Some("big-text-editor") => "Text Editor".to_string(),
        Some("big-filemanager") => "File Manager".to_string(),
        Some("big-video-converter") => "Video Converter".to_string(),
        Some(id) => id.to_string(),
        None => "an application".to_string(),
    }
}

fn dev_ino(path: &Path) -> (Option<i64>, Option<i64>) {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|metadata| (Some(metadata.dev() as i64), Some(metadata.ino() as i64)))
        .unwrap_or((None, None))
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_origin_path(label: &str) -> PathBuf {
        let dir = crate::test_env::scratch_dir(&format!(
            "bs-origin-{label}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        dir.join("origin.sqlite")
    }

    /// Another writer holding the lock for a while (the watcher set aside by
    /// the scheduler mid-transaction) delays a writer instead of failing it.
    /// Opening writes the schema, so it waits too.
    #[test]
    fn writer_waits_for_open_transaction() {
        let db = temporary_origin_path("busy");
        drop(OriginWriter::open(db.clone()).unwrap());
        let holder = Connection::open(&db).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            holder.execute_batch("COMMIT").unwrap();
        });
        let _writer = OriginWriter::open(db.clone()).unwrap();
        release.join().unwrap();
        assert!(
            OriginReader::new(db.clone())
                .origin(Path::new("/missing"))
                .is_none()
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn forget_all_empties_the_database() {
        let db = temporary_origin_path("forget");
        let conn = open_writer(&db).unwrap();
        conn.execute(
            "INSERT INTO file_origin(path, kind, confidence, label, first_seen_at, updated_at)
             VALUES ('/a', 'downloaded', 1, 'x', 0, 0)",
            [],
        )
        .unwrap();
        forget_all(&db).unwrap();
        let left: i64 = conn
            .query_row("SELECT count(*) FROM file_origin", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 0);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn fresh_store_has_no_origin() {
        let db = temporary_origin_path("schema");
        let _writer = OriginWriter::open(db.clone()).unwrap();
        assert!(
            OriginReader::new(db.clone())
                .origin(Path::new("/missing"))
                .is_none()
        );
        assert!(db.exists());
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    /// The browser's list names a path, not a file: it must not overwrite what
    /// is known about a file that took a deleted download's name.
    #[test]
    fn a_download_never_replaces_another_origin() {
        let db = temporary_origin_path("kinds");
        let conn = open_writer(&db).unwrap();
        let record = |kind, domain: Option<&str>| OriginRecord {
            path: "/d/report.pdf".to_string(),
            dev: None,
            ino: None,
            kind,
            confidence: OriginConfidence::High,
            source_app: None,
            source_path: None,
            source_device_id: None,
            source_device_label: None,
            source_domain: domain.map(str::to_string),
            first_seen_at: 1,
            updated_at: 1,
        };
        let stored = || select_record(&conn, "/d/report.pdf").unwrap().unwrap();

        upsert_record(&conn, &record(OriginKind::WebDownload, Some("a.example"))).unwrap();
        upsert_record(&conn, &record(OriginKind::WebDownload, Some("b.example"))).unwrap();
        assert_eq!(stored().source_domain.as_deref(), Some("b.example"));
        upsert_record(&conn, &record(OriginKind::AppCreated, None)).unwrap();
        upsert_record(&conn, &record(OriginKind::WebDownload, Some("c.example"))).unwrap();
        assert_eq!(stored().kind, OriginKind::AppCreated);
        assert_eq!(stored().source_domain, None);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    /// The label is built from the fields when read; the database keeps no
    /// sentence in any language.
    #[test]
    fn label_built_on_read() {
        let db = temporary_origin_path("label");
        let conn = open_writer(&db).unwrap();
        let record = OriginRecord {
            path: "/d/copy.pdf".to_string(),
            dev: None,
            ino: None,
            kind: OriginKind::RemovableCopy,
            confidence: OriginConfidence::High,
            source_app: None,
            source_path: Some("/media/stick/copy.pdf".to_string()),
            source_device_id: Some("stick".to_string()),
            source_device_label: Some("KINGSTON".to_string()),
            source_domain: None,
            first_seen_at: 1,
            updated_at: 1,
        };
        upsert_record(&conn, &record).unwrap();
        let stored: String = conn
            .query_row("SELECT label FROM file_origin", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, "");
        let read = select_record(&conn, "/d/copy.pdf").unwrap().unwrap();
        assert_eq!(read.label(), "Copied from the removable drive KINGSTON");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }
}
