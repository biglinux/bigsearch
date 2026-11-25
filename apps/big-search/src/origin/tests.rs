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

/// Statistics taken while the store held one record would have every
/// lookup read the whole table once it grew; opening the store drops them,
/// also for a connection already open.
#[test]
fn open_drops_statistics() {
    let db = temporary_origin_path("statistics");
    let insert = "INSERT INTO file_origin(path, fs, ino, kind, confidence, label,
                                          first_seen_at, updated_at)
                  VALUES (?1, 1, ?2, 'downloaded', 1, 'x', 0, 0)";
    {
        let conn = open_writer(&db).unwrap();
        conn.execute(insert, params!["/0", 0]).unwrap();
        conn.execute_batch("ANALYZE").unwrap();
        for ino in 1..2000 {
            conn.execute(insert, params![format!("/{ino}"), ino])
                .unwrap();
        }
    }
    let plan = |conn: &Connection| -> String {
        conn.query_row(
            "EXPLAIN QUERY PLAN SELECT path FROM file_origin
              WHERE fs = 1 AND ino = 7 ORDER BY updated_at DESC LIMIT 1",
            [],
            |row| row.get(3),
        )
        .unwrap()
    };
    let watcher = Connection::open(&db).unwrap();
    assert!(!plan(&watcher).contains("idx_file_origin_fs_ino"));
    let conn = open_writer(&db).unwrap();
    // A statement that runs, unlike EXPLAIN, notices the schema changed.
    watcher
        .query_row("SELECT count(*) FROM file_origin", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    for conn in [&conn, &watcher] {
        let plan = plan(conn);
        assert!(plan.contains("idx_file_origin_fs_ino"), "{plan}");
    }
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
        fs: None,
        ino: None,
        born: None,
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
        fs: None,
        ino: None,
        born: None,
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

/// A renamed file keeps its origin, found by its identity under the new
/// name; a file given a deleted one's inode number, born later, does not
/// take it.
#[test]
fn renamed_file_found_by_identity() {
    let db = temporary_origin_path("identity");
    let folder = db.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&folder).unwrap();
    let (before, after) = (folder.join("report.pdf"), folder.join("renamed.pdf"));
    std::fs::write(&before, b"pdf").unwrap();
    let conn = open_writer(&db).unwrap();
    let arrived =
        OriginRecord::arrived(&before, OriginKind::WebDownload, OriginConfidence::High, 1);
    upsert_record(&conn, &arrived).unwrap();
    std::fs::rename(&before, &after).unwrap();
    let key = path_key(&after);
    let by_name = |conn: &Connection| select_record(conn, &key).unwrap();
    let in_listing = |conn: &Connection| select_records(conn, std::slice::from_ref(&key)).unwrap();
    assert_eq!(
        by_name(&conn).map(|record| record.kind),
        Some(OriginKind::WebDownload)
    );
    assert!(in_listing(&conn).contains_key(&key));

    conn.execute("UPDATE file_origin SET born = born + 1", [])
        .unwrap();
    assert!(by_name(&conn).is_none());
    assert!(in_listing(&conn).is_empty());
    let _ = std::fs::remove_dir_all(&folder);
}

/// The store as it was while rows recorded `st_dev`.
const STORE_RECORDING_DEVICES: &str = "
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    INSERT INTO meta(key, value) VALUES ('schema_version', '1');
    CREATE TABLE file_origin (
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
    CREATE INDEX idx_file_origin_dev_ino ON file_origin(dev, ino);
";

/// An old store at `db` with a row per `(path, ino)`, each on device `dev`.
fn write_store_recording_devices(db: &Path, dev: i64, rows: &[(String, i64)]) {
    let old = Connection::open(db).unwrap();
    old.execute_batch(STORE_RECORDING_DEVICES).unwrap();
    for (path, ino) in rows {
        old.execute(
            "INSERT INTO file_origin(path, dev, ino, kind, confidence, label,
                                     first_seen_at, updated_at)
             VALUES (?1, ?2, ?3, 'web_download', 3, '', 1, 1)",
            params![path, dev, ino],
        )
        .unwrap();
    }
}

fn origin_index_names(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT name FROM pragma_index_list('file_origin') ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Guards: persisted format: a store whose rows recorded `st_dev` opens with
/// its rows kept, each given the identity of the file at its path when that
/// is still the file it recorded, and none otherwise.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "writes the st_dev an older build recorded"
)]
fn store_recording_devices_migrated() {
    let db = temporary_origin_path("migrate");
    let folder = db.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("kept.pdf");
    std::fs::write(&file, b"pdf").unwrap();
    let metadata = std::fs::symlink_metadata(&file).unwrap();
    let (dev, ino) = (metadata.dev().cast_signed(), metadata.ino().cast_signed());
    write_store_recording_devices(
        &db,
        dev,
        &[
            (path_key(&file), ino),
            (path_key(&folder.join("replaced.pdf")), ino + 1),
            ("/gone/x.pdf".to_string(), ino),
        ],
    );
    std::fs::write(folder.join("replaced.pdf"), b"another").unwrap();

    let conn = open_writer(&db).unwrap();
    let identities: Vec<(Option<i64>, Option<i64>)> = conn
        .prepare("SELECT fs, born FROM file_origin ORDER BY rowid")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let (fs, _, born) = crate::history::identity(&file, &metadata).unwrap();
    assert_eq!(identities, [(Some(fs), born), (None, None), (None, None)]);
    let indexes = origin_index_names(&conn);
    assert!(
        indexes.contains(&"idx_file_origin_fs_ino".to_string()),
        "{indexes:?}"
    );
    assert!(
        !indexes.contains(&"idx_file_origin_dev_ino".to_string()),
        "{indexes:?}"
    );
    drop(conn);
    // Opened again, nothing changes.
    let conn = open_writer(&db).unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM file_origin WHERE fs IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
    let _ = std::fs::remove_dir_all(&folder);
}
