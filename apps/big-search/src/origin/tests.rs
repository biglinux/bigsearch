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
    let insert = "INSERT INTO file_origin(path, dev, ino, kind, confidence, label,
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
              WHERE dev = 1 AND ino = 7 ORDER BY updated_at DESC LIMIT 1",
            [],
            |row| row.get(3),
        )
        .unwrap()
    };
    let watcher = Connection::open(&db).unwrap();
    assert!(!plan(&watcher).contains("idx_file_origin_dev_ino"));
    let conn = open_writer(&db).unwrap();
    // A statement that runs, unlike EXPLAIN, notices the schema changed.
    watcher
        .query_row("SELECT count(*) FROM file_origin", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    for conn in [&conn, &watcher] {
        let plan = plan(conn);
        assert!(plan.contains("idx_file_origin_dev_ino"), "{plan}");
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
