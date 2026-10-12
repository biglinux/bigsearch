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
fn cleared_state_hides_persisted_rows() {
    // A rebuild clears the cache; until the clear is persisted, lookups that
    // fell through to the still-populated SQLite rows would make the rescan
    // skip re-adding every wiped document.
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
fn scan_changes_ignore_page_size() {
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
fn children_are_one_level_deep() {
    let dir = std::env::temp_dir().join(format!("bs-state-children-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
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
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn removed_tree_spares_its_siblings() {
    let dir = std::env::temp_dir().join(format!("bs-state-tree-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
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
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn retained_prefix_keeps_accented_paths() {
    let dir = std::env::temp_dir().join(format!("bs-state-retain-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
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
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn entries_after_merges_pending_rows() {
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
fn stale_entries_stream_pending_work() {
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
