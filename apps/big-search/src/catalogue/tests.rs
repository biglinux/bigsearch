use super::*;

#[test]
fn dedup_roundtrip() {
    let dir = crate::test_env::scratch_dir("bs-state");
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

    let _ = std::fs::remove_dir_all(&dir);
}

/// A folder's inode, kept in the size column of the watch list, reads back as
/// it was written even when it does not fit an `i64`.
#[test]
fn full_width_inode_kept() {
    let dir = crate::test_env::scratch_dir("bs-state-inode");
    let file = dir.join("state.sqlite");
    let folder = Path::new("/x/folder");
    let inode = u64::MAX - 7;
    let mut state = State::empty(&file, WATCH_NAMESPACE);
    state.set(folder, 0, inode);
    state.persist().unwrap();
    let recorded = State::load(&file, WATCH_NAMESPACE).recorded(folder);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(recorded, Some((0, inode)));
}

#[test]
fn cleared_state_hides_persisted_rows() {
    // A rebuild clears the cache; until the clear is persisted, lookups that
    // fell through to the still-populated SQLite rows would make the rescan
    // skip re-adding every wiped document.
    let dir = crate::test_env::scratch_dir("bs-state-clearmem");
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

    let _ = std::fs::remove_dir_all(&dir);
}

/// The pages of the two sides end at different paths; whatever the page
/// size, the merge must report each difference once and nothing else.
#[test]
fn scan_changes_ignore_page_size() {
    let dir = crate::test_env::scratch_dir("bs-state-scan");
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
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn children_are_one_level_deep() {
    let dir = crate::test_env::scratch_dir("bs-state-children");
    let mut state = State::empty(&dir.join("state.sqlite"), NAME_NAMESPACE);
    for path in ["/p/ção", "/p/ção/a", "/p/ção/s", "/p/ção/s/b", "/p/ção-2/c"] {
        state.set(Path::new(path), 1, 1);
    }
    state.persist().unwrap();
    let mut children = state.children(Path::new("/p/ção")).unwrap();
    children.sort();
    assert_eq!(
        children,
        [PathBuf::from("/p/ção/a"), PathBuf::from("/p/ção/s")]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn removed_tree_spares_its_siblings() {
    let dir = crate::test_env::scratch_dir("bs-state-tree");
    let mut state = State::empty(&dir.join("state.sqlite"), NAME_NAMESPACE);
    for path in ["/p/t", "/p/t/a", "/p/t/s/b", "/p/t-2", "/p/t0"] {
        state.set(Path::new(path), 1, 1);
    }
    state.persist().unwrap();
    state.set(Path::new("/p/t/pending"), 1, 1);

    state.remove_tree(Path::new("/p/t"));
    // Recreated before the persist: the new row must survive the range.
    state.set(Path::new("/p/t/a"), 2, 2);
    assert_eq!(state.recorded(Path::new("/p/t/s/b")), None);
    assert_eq!(state.recorded(Path::new("/p/t/a")), Some((2, 2)));
    state.persist().unwrap();

    let state = State::load(&dir.join("state.sqlite"), NAME_NAMESPACE);
    let kept: Vec<_> = [
        "/p/t",
        "/p/t/a",
        "/p/t/s/b",
        "/p/t/pending",
        "/p/t-2",
        "/p/t0",
    ]
    .into_iter()
    .filter(|path| state.contains(Path::new(path)))
    .collect();
    assert_eq!(kept, ["/p/t/a", "/p/t-2", "/p/t0"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn retained_prefix_keeps_accented_paths() {
    let dir = crate::test_env::scratch_dir("bs-state-retain");
    let mut state = State::empty(&dir.join("state.sqlite"), NAME_NAMESPACE);
    for path in [
        "/run/media/joão/USB",
        "/run/media/joão/USB/a.txt",
        "/run/media/joão/USB-2/b",
    ] {
        state.set(Path::new(path), 1, 1);
    }
    state.persist().unwrap();
    state.begin_scan().unwrap();
    state
        .mark_seen_prefixes(&[PathBuf::from("/run/media/joão/USB")])
        .unwrap();

    let (changes, _) = state.scan_changes_after("", 10).unwrap();
    assert_eq!(
        changes,
        vec![ScanChange::Gone(PathBuf::from("/run/media/joão/USB-2/b"))]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn entries_after_merges_pending_rows() {
    let dir = crate::test_env::scratch_dir("bs-state-paths");
    let mut state = State::empty(&dir.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
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

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn entries_after_pages_pending_rows() {
    let dir = crate::test_env::scratch_dir("bs-state-page-pending");
    let mut state = State::empty(&dir.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
    for path in ["/a", "/b", "/c", "/d/x", "/e"] {
        state.set(Path::new(path), 1, 1);
    }
    state.persist().unwrap();
    state.remove(Path::new("/a"));
    state.remove_tree(Path::new("/d"));
    state.set(Path::new("/z"), 2, 2);

    // Pages of 2 walked by their last key, as the watcher and the CLI page.
    let mut pages = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = state.entries_after(after.as_deref(), 2).unwrap();
        let full = page.len() == 2;
        after = page
            .last()
            .map(|(path, _, _)| path.to_string_lossy().into_owned());
        pages.push(
            page.into_iter()
                .map(|(path, _, _)| path)
                .collect::<Vec<_>>(),
        );
        if !full {
            break;
        }
    }
    let expected: Vec<Vec<PathBuf>> = [vec!["/b", "/c"], vec!["/e", "/z"], vec![]]
        .into_iter()
        .map(|page| page.into_iter().map(PathBuf::from).collect())
        .collect();
    assert_eq!(pages, expected);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stale_entries_stream_pending_work() {
    let dir = crate::test_env::scratch_dir("bs-state-stale");
    let mut names = State::empty(&dir.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
    let mut content = State::empty(
        &dir.join("state.sqlite"),
        crate::catalogue::CONTENT_NAMESPACE,
    );

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

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn len_tracks_pending_overlaps_and_removals() {
    let dir = crate::test_env::scratch_dir("bs-state-len");
    let mut state = State::empty(&dir.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
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

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn on_disk_state_uses_wal_journal() {
    // tune_connection_for_low_memory switches to WAL; the default rollback
    // journal would double write amplification on every persist.
    let dir = crate::test_env::scratch_dir("bs-state-wal");
    let state = State::empty(&dir.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE);
    assert_eq!(state.journal_mode(), "wal");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A catalogue database holding `rows` in the name namespace, and a reader of it.
fn catalogue_of(
    label: &str,
    rows: &[(&str, i64, u64)],
) -> (crate::test_env::ScratchDir, CatalogueReader) {
    let dir = crate::test_env::scratch_dir(&format!("bs-catalogue-{label}"));
    let db = dir.join("state.sqlite");
    let mut names = State::empty(&db, NAME_NAMESPACE);
    for (path, mtime, size) in rows {
        names.set(Path::new(path), *mtime, *size);
    }
    names.persist().unwrap();
    (dir, CatalogueReader::new(&db))
}

fn newest_paths(reader: &CatalogueReader, filter: &Filter) -> Vec<String> {
    let (total, paths) = reader.list(filter, ListOrder::Newest, 0, 100).unwrap();
    assert_eq!(total, paths.len(), "{paths:?}");
    paths
}

#[test]
fn newest_first_with_exact_total() {
    let (dir, reader) = catalogue_of(
        "order",
        &[
            ("/p/old.jpg", 100, 1),
            ("/p/new.jpg", 300, 1),
            ("/p/mid-b.jpg", 200, 1),
            ("/p/mid-a.jpg", 200, 1),
            ("/p/newest.jpg", 400, 1),
        ],
    );
    // The content namespace shares the table and must not leak in.
    let mut content = State::empty(&dir.join("state.sqlite"), CONTENT_NAMESPACE);
    content.set(Path::new("/p/content-only.jpg"), 999, 1);
    content.persist().unwrap();

    assert_eq!(
        newest_paths(&reader, &Filter::default()),
        [
            "/p/newest.jpg",
            "/p/new.jpg",
            "/p/mid-a.jpg", // same second: the path breaks the tie
            "/p/mid-b.jpg",
            "/p/old.jpg",
        ]
    );
    let page = |offset, limit| {
        reader
            .list(&Filter::default(), ListOrder::Newest, offset, limit)
            .unwrap()
    };
    assert_eq!(
        page(0, 2),
        (
            5,
            vec!["/p/newest.jpg".to_string(), "/p/new.jpg".to_string()]
        )
    );
    assert_eq!(
        page(2, 2),
        (
            5,
            vec!["/p/mid-a.jpg".to_string(), "/p/mid-b.jpg".to_string()]
        )
    );
    assert_eq!(page(4, 2), (5, vec!["/p/old.jpg".to_string()]));
    assert_eq!(page(9, 2), (5, Vec::new()), "past the end");
    assert_eq!(page(0, 0), (5, Vec::new()), "a count alone");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn under_stops_at_the_folder_boundary() {
    let (dir, reader) = catalogue_of(
        "under",
        &[
            ("/home/a", 1, 0),
            ("/home/a/x.jpg", 2, 1),
            ("/home/a/sub/y.jpg", 3, 1),
            ("/home/ab/z.jpg", 4, 1),
            ("/home/a-old/w.jpg", 5, 1),
            ("/home/a0/v.jpg", 6, 1),
            ("/other/u.jpg", 7, 1),
        ],
    );
    let under = |dir: &str| Filter {
        under: Some(dir.to_string()),
        ..Default::default()
    };
    for folder in ["/home/a", "/home/a/", "/home/a//"] {
        assert_eq!(
            newest_paths(&reader, &under(folder)),
            ["/home/a/sub/y.jpg", "/home/a/x.jpg"],
            "under {folder}"
        );
    }
    // The root, written either way, holds everything.
    for root in ["/", ""] {
        assert_eq!(
            newest_paths(&reader, &under(root)).len(),
            7,
            "under {root:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ext_matches_ending_in_any_case() {
    let (dir, reader) = catalogue_of(
        "ext",
        &[
            ("/p/a.JPG", 1, 1),
            ("/p/b.jpeg", 2, 1),
            ("/p/c.png", 3, 1),
            ("/p/d.jpg.part", 4, 1),
            ("/p/jpg", 5, 1),
            ("/p/e.Jpg", 6, 1),
            ("/p/archive.tar.gz", 7, 1),
        ],
    );
    let ext = |wanted: &[&str]| Filter {
        ext: wanted.iter().map(|ext| ext.to_string()).collect(),
        ..Default::default()
    };
    assert_eq!(
        newest_paths(&reader, &ext(&["jpg", ".JPEG"])),
        ["/p/e.Jpg", "/p/b.jpeg", "/p/a.JPG"]
    );
    assert_eq!(newest_paths(&reader, &ext(&["gz"])), ["/p/archive.tar.gz"]);
    // No file's extension holds a dot, so these match nothing, as in the
    // name index; a valid one beside them still counts.
    assert!(newest_paths(&reader, &ext(&["tar.gz", "", "."])).is_empty());
    assert_eq!(
        newest_paths(&reader, &ext(&["tar.gz", "png"])),
        ["/p/c.png"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ranges_compare_recorded_values() {
    let (dir, reader) = catalogue_of(
        "range",
        &[
            ("/p/a", 100, 10),
            ("/p/b", 200, 20),
            ("/p/c", 300, 30),
            // The largest size a file can have: `off_t` is signed.
            ("/p/huge", 400, i64::MAX.cast_unsigned()),
        ],
    );
    let mtime = Filter {
        mtime: Some([200, 300]),
        ..Default::default()
    };
    assert_eq!(newest_paths(&reader, &mtime), ["/p/c", "/p/b"]);
    let size = Filter {
        size: Some([0, 20]),
        ..Default::default()
    };
    assert_eq!(newest_paths(&reader, &size), ["/p/b", "/p/a"]);
    let both = Filter {
        mtime: Some([150, 500]),
        size: Some([25, u64::MAX]),
        ..Default::default()
    };
    assert_eq!(newest_paths(&reader, &both), ["/p/huge", "/p/c"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_catalogue_lists_nothing() {
    let (dir, reader) = catalogue_of("empty", &[]);
    assert_eq!(
        reader
            .list(&Filter::default(), ListOrder::Newest, 0, 10)
            .unwrap(),
        (0, Vec::new())
    );
    let _ = std::fs::remove_dir_all(&dir);
    let missing = CatalogueReader::new(&dir.join("state.sqlite"));
    assert_eq!(
        missing
            .list(&Filter::default(), ListOrder::Newest, 0, 10)
            .unwrap(),
        (0, Vec::new())
    );
    assert!(!dir.exists(), "a reader must not create the database");
}

#[test]
fn newest_page_uses_mtime_index() {
    let (dir, _) = catalogue_of("plan", &[("/p/a.jpg", 1, 1)]);
    let filter = Filter {
        under: Some("/p".into()),
        ext: vec!["jpg".into()],
        ..Default::default()
    };
    let (clause, args) = catalogue_where(&filter, true).unwrap();
    let conn = open_catalogue_reader(&dir.join("state.sqlite")).unwrap();
    let mut plan = conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN SELECT path FROM file_state WHERE {clause}
             ORDER BY mtime DESC, path LIMIT 10"
        ))
        .unwrap();
    let steps: Vec<String> = plan
        .query_map(params_from_iter(args), |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        steps
            .iter()
            .any(|step| step.contains("file_state_name_by_mtime")),
        "{steps:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
