use super::*;

pub(super) fn store_with(conn: Connection, dir: &Path) -> History {
    History {
        conn: Some(conn),
        objects: dir.join("objects"),
        support: Support::Ready,
        hopeless_devices: Vec::new(),
        budget_bytes: 0,
        keep_versions: 1,
        keep_days: 1,
        paused_for_space: false,
        budget_unchecked: false,
        skip_git_repositories: false,
        respect_gitignore: false,
    }
}

fn version_owners(conn: &Connection) -> Vec<(String, String)> {
    conn.prepare(
        "SELECT f.current_path, v.object FROM versions v JOIN files f ON f.id = v.file_id
          ORDER BY v.object",
    )
    .unwrap()
    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// `mv a.md b.md` over a document with its own history keeps every version
/// of both under the new name.
#[test]
fn rename_over_a_kept_document() {
    let dir = crate::test_env::scratch_dir("bs-history-rename");
    let conn = open_store(&dir.join("history.sqlite")).unwrap();
    for (id, path) in [(1, "/d/a.md"), (2, "/d/b.md")] {
        conn.execute(
            "INSERT INTO files(id, current_path, created_at) VALUES (?1, ?2, 0)",
            params![id, path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO versions(file_id, saved_at, mtime_s, mtime_ns, size, object, reason)
             VALUES (?1, 0, 0, 0, 1, ?2, 'saved')",
            params![id, format!("object-{id}")],
        )
        .unwrap();
    }
    follow_rename(&conn, Path::new("/d/a.md"), Path::new("/d/b.md")).unwrap();
    assert_eq!(
        version_owners(&conn),
        [
            ("/d/b.md".to_string(), "object-1".to_string()),
            ("/d/b.md".to_string(), "object-2".to_string()),
        ]
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A new file given a deleted document's inode number starts its own
/// history; a renamed document keeps its own.
#[test]
fn reused_inode_starts_a_new_history() {
    let dir = crate::test_env::scratch_dir("bs-history-inode");
    std::fs::create_dir_all(&dir).unwrap();
    let store = store_with(open_store(&dir.join("history.sqlite")).unwrap(), &dir);
    let conn = store.conn.as_ref().unwrap();
    let notes = dir.join("notes.odt");
    std::fs::write(&notes, b"new").unwrap();
    let metadata = std::fs::metadata(&notes).unwrap();
    let born = metadata
        .created()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    let deleted = |born: i64| {
        conn.execute("DELETE FROM files", []).unwrap();
        conn.execute(
            "INSERT INTO files(id, current_path, dev, ino, born, created_at)
             VALUES (7, '/gone/report.odt', ?1, ?2, ?3, 0)",
            params![metadata.dev() as i64, metadata.ino() as i64, born],
        )
        .unwrap();
    };

    deleted(born - 1);
    assert_ne!(store.upsert_file(&notes, &metadata).unwrap(), 7);
    deleted(born);
    assert_eq!(store.upsert_file(&notes, &metadata).unwrap(), 7);
    std::fs::remove_dir_all(&dir).ok();
}

/// A path that is not UTF-8 never finds the row of the file whose name its
/// lossy form spells.
#[test]
fn non_utf8_path_has_no_row() {
    use std::os::unix::ffi::OsStrExt;
    let dir = crate::test_env::scratch_dir("bs-history-utf8");
    let conn = open_store(&dir.join("history.sqlite")).unwrap();
    conn.execute(
        "INSERT INTO files(id, current_path, created_at) VALUES (1, '/d/Relat\u{FFFD}rios/a.md', 0)",
        [],
    )
    .unwrap();
    let latin1 = Path::new(std::ffi::OsStr::from_bytes(b"/d/Relat\xf3rios/a.md"));
    assert_eq!(file_id_for_path(&conn, latin1).unwrap(), None);
    std::fs::remove_dir_all(&dir).ok();
}

/// After a restart the pass over the catalogue checks only what the newest
/// version does not already hold, without touching the disk.
#[test]
fn catalogue_pass_skips_kept_documents() {
    let dir = std::env::temp_dir().join(format!("bs-history-unkept-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let conn = open_store(&dir.join("history.sqlite")).unwrap();
    for (id, path) in [(1, "/d/kept.md"), (2, "/d/edited.md")] {
        conn.execute(
            "INSERT INTO files(id, current_path, created_at) VALUES (?1, ?2, 0)",
            params![id, path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO versions(file_id, saved_at, mtime_s, mtime_ns, size, object, reason)
             VALUES (?1, 0, 10, 0, 5, 'x', 'saved')",
            params![id],
        )
        .unwrap();
    }
    let store = store_with(conn, &dir);

    let unkept = store.unkept(vec![
        (PathBuf::from("/d/kept.md"), 10, 5),
        (PathBuf::from("/d/edited.md"), 11, 5),
        (PathBuf::from("/d/new.md"), 10, 5),
    ]);
    assert_eq!(
        unkept,
        [PathBuf::from("/d/edited.md"), PathBuf::from("/d/new.md")]
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn disabled_store_erases_its_files() {
    let _env = crate::test_env::ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let xdg_data = crate::test_env::scratch_dir("bs-history-erase");
    let _data_home = crate::test_env::EnvVarGuard::set("XDG_DATA_HOME", xdg_data.to_str().unwrap());
    let objects = crate::config::history_dir().join("objects/ab");
    std::fs::create_dir_all(&objects).unwrap();
    std::fs::write(objects.join("cdef"), "kept").unwrap();
    drop(open_store(&crate::config::history_db_path()).unwrap());

    let mut store = History::open(&big_search_config::FileHistory {
        enabled: false,
        ..Default::default()
    });
    store.forget_everything().unwrap();
    assert!(!crate::config::history_dir().join("objects").exists());
    assert!(!crate::config::history_db_path().exists());
    std::fs::remove_dir_all(&xdg_data).ok();
}

/// Two objects never collide, and each lands in a shard.
#[test]
fn every_object_gets_its_own_name() {
    let first = object_name();
    let second = object_name();
    assert_ne!(first, second);
    assert_eq!(first.len(), 33, "{first}");
    assert_eq!(&first[2..3], "/");
}
