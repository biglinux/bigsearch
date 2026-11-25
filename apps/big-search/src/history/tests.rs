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
    follow_renames(&conn, [(Path::new("/d/a.md"), Path::new("/d/b.md"))]).unwrap();
    assert_eq!(
        version_owners(&conn),
        [
            ("/d/b.md".to_string(), "object-1".to_string()),
            ("/d/b.md".to_string(), "object-2".to_string()),
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A new file given a deleted document's inode number starts its own
/// history; a renamed document keeps its own.
#[test]
fn reused_inode_starts_a_new_history() {
    let dir = crate::test_env::scratch_dir("bs-history-inode");
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
    let (fs, ino, _) = identity(&notes, &metadata).unwrap();
    let deleted = |born: i64| {
        conn.execute("DELETE FROM files", []).unwrap();
        conn.execute(
            "INSERT INTO files(id, current_path, fs, ino, born, created_at)
             VALUES (7, '/gone/report.odt', ?1, ?2, ?3, 0)",
            params![fs, ino, born],
        )
        .unwrap();
    };

    deleted(born - 1);
    assert_ne!(store.upsert_file(&notes, &metadata).unwrap(), 7);
    deleted(born);
    assert_eq!(store.upsert_file(&notes, &metadata).unwrap(), 7);
    let _ = std::fs::remove_dir_all(&dir);
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// After a restart the pass over the catalogue checks only what the newest
/// version does not already hold, without touching the disk.
#[test]
fn catalogue_pass_skips_kept_documents() {
    let dir = crate::test_env::scratch_dir("bs-history-unkept");
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
    let _ = std::fs::remove_dir_all(&dir);
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
    let _ = std::fs::remove_dir_all(&xdg_data);
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

fn store_with_versions(name: &str, objects: &[&str]) -> (crate::test_env::ScratchDir, History) {
    let dir = crate::test_env::scratch_dir(name);
    let conn = open_store(&dir.join("history.sqlite")).unwrap();
    conn.execute(
        "INSERT INTO files(id, current_path, created_at) VALUES (1, '/d/a.md', 0)",
        [],
    )
    .unwrap();
    for object in objects {
        conn.execute(
            "INSERT INTO versions(file_id, saved_at, mtime_s, mtime_ns, size, object, reason)
             VALUES (1, 0, 0, 0, 1, ?1, 'saved')",
            params![object],
        )
        .unwrap();
    }
    let store = store_with(conn, &dir);
    (dir, store)
}

fn kept_objects(store: &History) -> Vec<String> {
    let mut objects = store.all_object_names().unwrap();
    objects.sort();
    objects
}

/// The sweep removes what a crash left, an object without a row and a row
/// without an object, and keeps every version whose object is in place.
#[test]
fn sweep_drops_only_orphans() {
    let (dir, mut store) = store_with_versions("bs-history-sweep", &["aa/kept", "aa/lost"]);
    let shard = dir.join("objects/aa");
    std::fs::create_dir_all(&shard).unwrap();
    std::fs::write(shard.join("kept"), "v1").unwrap();
    std::fs::write(shard.join("stray"), "v0").unwrap();
    std::fs::write(dir.join("objects/notes.txt"), "not a shard").unwrap();

    store.sweep_orphans().unwrap();
    assert_eq!(kept_objects(&store), ["aa/kept"]);
    assert!(shard.join("kept").exists());
    assert!(!shard.join("stray").exists());
    assert!(dir.join("objects/notes.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A shard the sweep cannot list is an error, never proof that its versions
/// are gone: taking it for one would drop the rows, then the objects.
#[test]
fn unreadable_shard_keeps_versions() {
    if crate::test_env::runs_as_root() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let (dir, mut store) = store_with_versions("bs-history-sweep-denied", &["bb/one"]);
    let shard = dir.join("objects/bb");
    std::fs::create_dir_all(&shard).unwrap();
    std::fs::write(shard.join("one"), "v1").unwrap();
    std::fs::set_permissions(&shard, std::fs::Permissions::from_mode(0o000)).unwrap();

    let swept = store.sweep_orphans();
    std::fs::set_permissions(&shard, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(swept.is_err());
    assert_eq!(kept_objects(&store), ["bb/one"]);
    assert!(shard.join("one").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A store beside `real/a.txt` and `alias`, a link to `real`; returns the
/// file's real and alias spellings.
fn alias_fixture(name: &str) -> (crate::test_env::ScratchDir, History, PathBuf, PathBuf) {
    let dir = crate::test_env::scratch_dir(name);
    let store = store_with(open_store(&dir.join("history.sqlite")).unwrap(), &dir);
    std::fs::create_dir_all(dir.join("real")).unwrap();
    std::os::unix::fs::symlink(dir.join("real"), dir.join("alias")).unwrap();
    let (real, alias) = (dir.join("real/a.txt"), dir.join("alias/a.txt"));
    std::fs::write(&real, "kept").unwrap();
    (dir, store, real, alias)
}

/// Row `id` under `path`, with `file`'s identity and one version `object-{id}`.
fn kept_under(conn: &Connection, path: &Path, id: i64, file: &Path) {
    let (fs, ino, born) = identity(file, &std::fs::symlink_metadata(file).unwrap()).unwrap();
    conn.execute(
        "INSERT INTO files(id, current_path, fs, ino, born, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        params![id, path.to_str().unwrap(), fs, ino, born],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO versions(file_id, saved_at, mtime_s, mtime_ns, size, object, reason)
         VALUES (?1, 0, ?1, 0, 1, ?2, 'saved')",
        params![id, format!("object-{id}")],
    )
    .unwrap();
}

/// Rows kept under a spelling that leads through a symlinked folder move to
/// where it leads when they were kept for the file there, joining its own
/// rows; they stay when nothing is there.
#[test]
fn spelling_adopted_by_identity() {
    let (dir, store, real, alias) = alias_fixture("bs-history-adopt");
    let conn = store.conn.as_ref().unwrap();
    kept_under(conn, &alias, 1, &real);
    kept_under(conn, &real, 2, &real);
    store.adopt_spelling(&alias, &real).unwrap();
    let real_key = real.to_str().unwrap().to_string();
    assert_eq!(
        version_owners(conn),
        [
            (real_key.clone(), "object-1".to_string()),
            (real_key, "object-2".to_string()),
        ]
    );

    let missing = dir.join("alias/missing.txt");
    kept_under(conn, &missing, 3, &real);
    store
        .adopt_spelling(&missing, &dir.join("real/missing.txt"))
        .unwrap();
    assert_eq!(file_id_for_path(conn, &missing).unwrap(), Some(3));
}

/// After an editor's save gave the file there a new inode, nothing tells it
/// from another file: the restore is refused naming both paths, and the rows
/// stay.
#[test]
fn replaced_file_spelling_refused() {
    let (_dir, store, real, alias) = alias_fixture("bs-history-adopt-saved");
    let conn = store.conn.as_ref().unwrap();
    kept_under(conn, &alias, 1, &real);
    let saved = real.with_added_extension("tmp");
    std::fs::write(&saved, "saved by an editor").unwrap();
    std::fs::rename(&saved, &real).unwrap();

    let refusal = store.adopt_spelling(&alias, &real).unwrap_err().to_string();
    let named = format!("{} leads to {} now", alias.display(), real.display());
    assert!(refusal.contains(&named), "{refusal}");
    assert_eq!(file_id_for_path(conn, &alias).unwrap(), Some(1));
}

/// Each part of the recorded identity decides: another filesystem or another
/// birth time is another file, and a row without a birth time matches on
/// `(fs, ino)` alone.
#[test]
fn adoption_checks_each_identity_part() {
    let (_dir, store, real, alias) = alias_fixture("bs-history-adopt-parts");
    let conn = store.conn.as_ref().unwrap();
    let (fs, ino, born) = identity(&real, &std::fs::symlink_metadata(&real).unwrap()).unwrap();
    let born = born.expect("this filesystem records birth times");
    for (row_fs, row_born, adopted) in [
        (fs + 1, Some(born), false),
        (fs, Some(born + 1), false),
        (fs, None, true),
    ] {
        conn.execute("DELETE FROM versions", []).unwrap();
        conn.execute("DELETE FROM files", []).unwrap();
        conn.execute(
            "INSERT INTO files(id, current_path, fs, ino, born, created_at)
             VALUES (1, ?1, ?2, ?3, ?4, 0)",
            params![alias.to_str().unwrap(), row_fs, ino, row_born],
        )
        .unwrap();
        let outcome = store.adopt_spelling(&alias, &real);
        assert_eq!(
            outcome.is_ok(),
            adopted,
            "{row_fs} {row_born:?}: {outcome:?}"
        );
        let key = if adopted { &real } else { &alias };
        assert_eq!(file_id_for_path(conn, key).unwrap(), Some(1));
    }
}

/// A copy of `real` into the object `name`, taken without the lock, as the
/// worker takes it.
fn copy_in_flight(store: &mut History, dir: &Path, real: &Path, name: &str) -> VersionToCopy {
    let object = dir.join("objects").join(name);
    std::fs::create_dir_all(object.parent().unwrap()).unwrap();
    std::fs::write(&object, "copied").unwrap();
    store.budget_unchecked = false;
    VersionToCopy {
        path: real.to_path_buf(),
        object,
        name: name.to_string(),
        metadata: std::fs::metadata(real).unwrap(),
        reason: Reason::Saved,
    }
}

/// Guards: persisted data: a restore that moves the rows, or a forget, while
/// the worker copies leaves the copy recorded under the row its path has, or
/// removed with nothing recorded.
#[test]
fn copy_follows_rows_changed_meanwhile() {
    let (dir, mut store, real, alias) = alias_fixture("bs-history-copy-race");
    {
        let conn = store.conn.as_ref().unwrap();
        kept_under(conn, &alias, 1, &real);
        kept_under(conn, &real, 2, &real);
    }
    let version = copy_in_flight(&mut store, &dir, &real, "ff/adopted");
    store.adopt_spelling(&alias, &real).unwrap();
    assert!(store.finish_version(version, Ok(())).unwrap());
    let owner: i64 = store
        .conn
        .as_ref()
        .unwrap()
        .query_row(
            "SELECT file_id FROM versions WHERE object = 'ff/adopted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owner, 1);

    let version = copy_in_flight(&mut store, &dir, &real, "ff/forgotten");
    let object = version.object.clone();
    store.forget(&[&real], None).unwrap();
    assert!(!store.finish_version(version, Ok(())).unwrap());
    assert!(!object.exists());
    assert_eq!(kept_objects(&store), Vec::<String>::new());
}

/// Ids of the rows left, in order.
fn row_ids(conn: &Connection) -> Vec<i64> {
    conn.prepare("SELECT id FROM files ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Guards: persisted data: a row kept under a path through a folder since
/// replaced by a link to where the folder went moves to the file's real path,
/// as a row whose path is gone does; a hard link at a real path keeps its own.
#[test]
fn upsert_takes_rows_the_file_left() {
    let (dir, store, real, alias) = alias_fixture("bs-history-upsert-left");
    let conn = store.conn.as_ref().unwrap();
    kept_under(conn, &alias, 1, &real);
    kept_under(conn, &dir.join("gone/a.txt"), 2, &real);
    let metadata = std::fs::symlink_metadata(&real).unwrap();
    assert_eq!(store.upsert_file(&real, &metadata).unwrap(), 1);
    assert_eq!(row_ids(conn), [1]);
    assert_eq!(file_id_for_path(conn, &real).unwrap(), Some(1));
    assert_eq!(version_owners(conn).len(), 2);

    let hard_link = dir.join("real/b.txt");
    std::fs::hard_link(&real, &hard_link).unwrap();
    let id = store.upsert_file(&hard_link, &metadata).unwrap();
    assert_eq!(row_ids(conn), [1, id]);
}

/// Guards: privacy action: forget erases the rows listed for the spelling
/// and every row of the file the path leads to, except when the listed rows
/// are another file's kept under another path: then only those.
#[test]
fn forget_reaches_the_file_rows() {
    let (dir, mut store, real, alias) = alias_fixture("bs-history-forget-rows");
    let other = dir.join("other.txt");
    std::fs::write(&other, "another document").unwrap();
    let elsewhere = dir.join("gone/a.txt");
    // Rows as (key, id, the file whose identity it records).
    let mut left_after = |rows: &[(&Path, i64, &Path)], spellings: &[&Path]| {
        let conn = store.conn.as_ref().unwrap();
        conn.execute_batch("DELETE FROM versions; DELETE FROM files;")
            .unwrap();
        for (key, id, file) in rows {
            kept_under(conn, key, *id, file);
        }
        store.forget(spellings, Some(&real)).unwrap();
        row_ids(store.conn.as_ref().unwrap())
    };
    let other_at_alias = [(alias.as_path(), 1, other.as_path()), (&real, 2, &real)];
    assert_eq!(left_after(&other_at_alias, &[&alias, &real]), [2]);
    let moved_at_alias = [(alias.as_path(), 1, real.as_path()), (&real, 2, &real)];
    assert_eq!(
        left_after(&moved_at_alias, &[&alias, &real]),
        Vec::<i64>::new()
    );
    let stale_at_real = [(real.as_path(), 1, other.as_path()), (&elsewhere, 2, &real)];
    assert_eq!(left_after(&stale_at_real, &[&real]), Vec::<i64>::new());
}

/// Rows are never moved to a key that is not UTF-8: it would name no file.
#[test]
fn adoption_into_non_utf8_refused() {
    use std::os::unix::ffi::OsStrExt;
    let dir = crate::test_env::scratch_dir("bs-history-adopt-utf8");
    let store = store_with(open_store(&dir.join("history.sqlite")).unwrap(), &dir);
    let folder = dir.join(std::ffi::OsStr::from_bytes(b"Relat\xf3rios"));
    std::fs::create_dir(&folder).unwrap();
    std::os::unix::fs::symlink(&folder, dir.join("alias")).unwrap();
    let (real, alias) = (folder.join("a.txt"), dir.join("alias/a.txt"));
    std::fs::write(&real, "kept").unwrap();
    let conn = store.conn.as_ref().unwrap();
    kept_under(conn, &alias, 1, &real);
    assert!(store.adopt_spelling(&alias, &real).is_err());
    assert_eq!(file_id_for_path(conn, &alias).unwrap(), Some(1));
}

/// The store as it was created while rows recorded `st_dev` and every row
/// needed a key, with one document and one version.
const STORE_RECORDING_DEVICES: &str = "
    CREATE TABLE files (
        id INTEGER PRIMARY KEY,
        current_path TEXT NOT NULL UNIQUE,
        dev INTEGER,
        ino INTEGER,
        created_at INTEGER NOT NULL,
        missing_since INTEGER,
        born INTEGER
    );
    CREATE INDEX idx_files_dev_ino ON files(dev, ino);
    CREATE TABLE versions (
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
    INSERT INTO files(id, current_path, dev, ino, created_at, missing_since, born)
    VALUES (4, '/d/a.md', 1, 2, 3, NULL, 5);
    INSERT INTO versions(file_id, saved_at, mtime_s, mtime_ns, size, object, reason)
    VALUES (4, 6, 7, 8, 9, 'ab/cd', 'saved');
";

/// A migrated row: key, filesystem, inode, created, missing since, born,
/// parked from.
type MigratedRow = (
    Option<String>,
    Option<i64>,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// [`STORE_RECORDING_DEVICES`] at `db`, with a second document kept at
/// `kept_here`.
fn write_store_recording_devices(db: &Path, kept_here: &Path) {
    let old = Connection::open(db).unwrap();
    old.execute_batch(STORE_RECORDING_DEVICES).unwrap();
    old.execute(
        "INSERT INTO files(id, current_path, dev, ino, created_at) VALUES (6, ?1, 1, 2, 3)",
        params![kept_here.to_str().unwrap()],
    )
    .unwrap();
}

/// The rows of a migrated store, in id order.
fn migrated_rows(conn: &Connection) -> Vec<MigratedRow> {
    conn.prepare(
        "SELECT current_path, fs, ino, created_at, missing_since, born, parked_from
           FROM files ORDER BY id",
    )
    .unwrap()
    .query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
        ))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// Guards: persisted format: a store whose rows recorded `st_dev` and all
/// needed a key opens with its rows, versions and indexes kept, each keyed
/// row given its folder's filesystem (none when the folder is gone), and
/// takes a row with none.
#[test]
fn store_recording_devices_migrated() {
    let dir = crate::test_env::scratch_dir("bs-history-migrate");
    let db = dir.join("history.sqlite");
    let kept_here = dir.join("b.md");
    write_store_recording_devices(&db, &kept_here);

    let conn = open_store(&db).unwrap();
    let rows = migrated_rows(&conn);
    let here = filesystem_id(&dir, std::fs::metadata(&dir).unwrap().dev()).unwrap();
    assert_eq!(
        rows,
        [
            (Some("/d/a.md".to_string()), None, 2, 3, None, Some(5), None),
            (
                kept_here.to_str().map(str::to_string),
                Some(here),
                2,
                3,
                None,
                None,
                None
            ),
        ]
    );
    assert_eq!(
        version_owners(&conn),
        [("/d/a.md".to_string(), "ab/cd".to_string())]
    );
    let index: Option<String> = conn
        .query_row(
            "SELECT name FROM pragma_index_list('files') WHERE name = 'idx_files_fs_ino'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert!(index.is_some(), "the identity index is missing");
    conn.execute(
        "INSERT INTO files(id, current_path, created_at) VALUES (5, NULL, 0)",
        [],
    )
    .unwrap();
    let dangling: Option<i64> = conn
        .query_row("SELECT 1 FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .optional()
        .unwrap();
    assert_eq!(dangling, None);
    assert!(
        conn.execute("DELETE FROM files WHERE id = 4", []).is_err(),
        "versions still refer to their row"
    );
}

/// The filesystem in a file's identity is the `f_fsid` coreutils reports
/// (its two words printed in the other order), or the device number where
/// the filesystem reports none.
#[test]
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "coreutils is the oracle, outside this crate"
)]
fn identity_names_the_filesystem() {
    let dir = crate::test_env::scratch_dir("bs-history-fsid");
    let file = dir.join("a.odt");
    std::fs::write(&file, "kept").unwrap();
    let metadata = std::fs::symlink_metadata(&file).unwrap();
    let (fs, _, _) = identity(&file, &metadata).unwrap();
    let output = std::process::Command::new("stat")
        .args(["--file-system", "--format=%i"])
        .arg(&file)
        .output()
        .unwrap();
    let printed =
        u64::from_str_radix(String::from_utf8(output.stdout).unwrap().trim(), 16).unwrap();
    let expected = if printed == 0 {
        metadata.dev()
    } else {
        printed.rotate_left(32)
    };
    assert_eq!(fs.cast_unsigned(), expected);
}
