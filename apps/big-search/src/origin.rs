//! Lightweight file origin/provenance store. Search remains owned by Tantivy;
//! this SQLite sidecar is consulted only for explicit origin requests or when a
//! query asks for origin summaries.
use anyhow::{Context, Result, bail};
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
    label: String,
    source_app: Option<String>,
    source_path: Option<String>,
    source_device_id: Option<String>,
    source_device_label: Option<String>,
    source_domain: Option<String>,
    first_seen_at: i64,
    updated_at: i64,
}

impl OriginRecord {
    fn into_provenance(self) -> OriginProvenance {
        OriginProvenance {
            kind: self.kind.as_str().to_string(),
            confidence: self.confidence as u8,
            label: self.label,
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
    /// Read the download lists the web browsers keep, to say which site a file
    /// came from. Off unless the person turned it on: that list belongs to the
    /// browser, and nobody expects a search service to have read it.
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
    match std::env::var(name).ok().as_deref() {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        Some(_) | None => default,
    }
}

pub struct OriginWriter {
    conn: Option<Connection>,
}

impl OriginWriter {
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

    pub fn register_created(&self, path: &Path, app_id: Option<&str>) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        ensure_path_under_source(path)?;
        let now = now_secs();
        let (dev, ino) = dev_ino(path);
        let app = app_id.map(str::to_string);
        let record = OriginRecord {
            path: path_key(path),
            dev,
            ino,
            kind: OriginKind::AppCreated,
            confidence: OriginConfidence::High,
            label: format!("Criado por {}", app_label(app_id)),
            source_app: app,
            source_path: None,
            source_device_id: None,
            source_device_label: None,
            source_domain: None,
            first_seen_at: now,
            updated_at: now,
        };
        upsert_record(conn, &record)
    }

    pub fn register_copy(&self, src: &Path, dst: &Path, app_id: Option<&str>) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        ensure_path_under_source(dst)?;
        let now = now_secs();
        let (dev, ino) = dev_ino(dst);
        let source = crate::settings::sources().owner(src);
        let (kind, label, source_device_id, source_device_label) = match source {
            Some(source) if source.removable() => (
                OriginKind::RemovableCopy,
                format!("Copiado do pendrive {}", source.name),
                Some(source.id.clone()),
                Some(source.name.clone()),
            ),
            Some(source) if source.network() => (
                OriginKind::NetworkCopy,
                format!("Copiado da rede {}", source.name),
                Some(source.id.clone()),
                Some(source.name.clone()),
            ),
            _ => (
                OriginKind::LocalCopy,
                "Copiado neste computador".to_string(),
                None,
                None,
            ),
        };
        let record = OriginRecord {
            path: path_key(dst),
            dev,
            ino,
            kind,
            confidence: OriginConfidence::High,
            label,
            source_app: app_id.map(str::to_string),
            source_path: Some(path_key(src)),
            source_device_id,
            source_device_label,
            source_domain: None,
            first_seen_at: now,
            updated_at: now,
        };
        upsert_record(conn, &record)
    }

    pub fn register_move(&self, src: &Path, dst: &Path, app_id: Option<&str>) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        ensure_path_under_source(dst)?;
        let now = now_secs();
        let src_key = path_key(src);
        let dst_key = path_key(dst);
        let (dev, ino) = dev_ino(dst);

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
            path: dst_key,
            dev,
            ino,
            kind: OriginKind::MovedOrRenamed,
            confidence: OriginConfidence::Medium,
            label: "Movido ou renomeado neste computador".to_string(),
            source_app: app_id.map(str::to_string),
            source_path: Some(path_key(src)),
            source_device_id: None,
            source_device_label: None,
            source_domain: None,
            first_seen_at: now,
            updated_at: now,
        };
        upsert_record(conn, &record)
    }

    pub fn register_derived(
        &self,
        src: &Path,
        dst: &Path,
        app_id: Option<&str>,
        action: Option<&str>,
    ) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        ensure_path_under_source(dst)?;
        let now = now_secs();
        let (dev, ino) = dev_ino(dst);
        let source_name = src
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_key(src));
        let label = match action {
            Some("video-conversion") => format!("Gerado a partir de {source_name}"),
            Some(_) | None => format!("Gerado a partir de {source_name}"),
        };
        let record = OriginRecord {
            path: path_key(dst),
            dev,
            ino,
            kind: OriginKind::DerivedFile,
            confidence: OriginConfidence::High,
            label,
            source_app: app_id.map(str::to_string),
            source_path: Some(path_key(src)),
            source_device_id: None,
            source_device_label: None,
            source_domain: None,
            first_seen_at: now,
            updated_at: now,
        };
        upsert_record(conn, &record)
    }

    pub fn register_web_download(&self, path: &Path, domain: &str) -> Result<()> {
        self.register_web_download_at(path, domain, now_secs())
    }

    /// The same, for a download whose date is known: from the list a browser
    /// keeps, where the file may have arrived months ago.
    ///
    /// The stored `label` stays what the rest of this file writes, for the
    /// command line that prints it. The desktop ignores it and builds its own
    /// sentence from `kind` and the fields, because only the desktop has a
    /// translation catalogue.
    pub fn register_web_download_at(
        &self,
        path: &Path,
        domain: &str,
        downloaded_at: i64,
    ) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        ensure_path_under_source(path)?;
        let (dev, ino) = dev_ino(path);
        let record = OriginRecord {
            path: path_key(path),
            dev,
            ino,
            kind: OriginKind::WebDownload,
            confidence: OriginConfidence::High,
            label: "Baixado da internet".to_string(),
            source_app: None,
            source_path: None,
            source_device_id: None,
            source_device_label: None,
            source_domain: Some(domain.to_string()),
            first_seen_at: downloaded_at,
            updated_at: now_secs(),
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
    pub fn register_web_downloads_at(&self, downloads: &[(PathBuf, String, i64)]) -> Result<usize> {
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

    pub fn origin(&self, path: &Path) -> Result<Option<OriginProvenance>> {
        let Some(conn) = &self.conn else {
            return Ok(None);
        };
        Ok(select_record(conn, &path_key(path))?.map(OriginRecord::into_provenance))
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
pub fn forget_all(path: &Path) -> Result<()> {
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
    conn.execute_batch("PRAGMA optimize;")?;
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
            record.label,
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

/// The columns `row_to_record` reads, in the order it reads them.
///
/// One list, because it is read by position: a query that lists them in another
/// order does not fail, it returns a record with the fields swapped.
const RECORD_COLUMNS: &str = "path, dev, ino, kind, confidence, label, source_app, \
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

/// How many unmatched paths are looked up by identity in one go.
///
/// `big-search -a --origin` can ask about a hundred thousand paths at once, and
/// each one not already known costs a `stat`. A page of results is far smaller
/// than this; a dump of the whole catalogue stops at the first few hundred.
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

/// Fill in the paths the name lookup missed, by what the file *is*.
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
        label: row.get(5)?,
        source_app: row.get(6)?,
        source_path: row.get(7)?,
        source_device_id: row.get(8)?,
        source_device_label: row.get(9)?,
        source_domain: row.get(10)?,
        first_seen_at: row.get(11)?,
        updated_at: row.get(12)?,
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

fn ensure_path_under_source(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("origin path must be absolute: {}", path.display());
    }
    if crate::settings::sources().owner(path).is_none() {
        bail!(
            "origin path is outside configured sources: {}",
            path.display()
        );
    }
    Ok(())
}

fn app_label(app_id: Option<&str>) -> String {
    match app_id {
        Some("big-text-editor") => "Editor de Textos".to_string(),
        Some("big-filemanager") => "Gerenciador de Arquivos".to_string(),
        Some("big-video-converter") => "Conversor de Vídeo".to_string(),
        Some(id) => id.to_string(),
        None => "aplicativo".to_string(),
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
        let dir = std::env::temp_dir().join(format!(
            "bs-origin-{label}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
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
        let writer = OriginWriter::open(db.clone()).unwrap();
        release.join().unwrap();
        assert!(writer.origin(Path::new("/missing")).unwrap().is_none());
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
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
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    #[test]
    fn fresh_store_has_no_origin() {
        let db = temporary_origin_path("schema");
        let writer = OriginWriter::open(db.clone()).unwrap();
        assert!(writer.origin(Path::new("/missing")).unwrap().is_none());
        assert!(db.exists());
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
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
            label: String::new(),
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
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }
}
