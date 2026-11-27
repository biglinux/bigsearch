use super::*;
use crate::test_env::{ENV_LOCK, EnvVarGuard};

#[test]
fn pending_batch_reports_more_work() {
    let base = crate::test_env::scratch_dir("lsearch-content-batch");
    let first = base.join("first.md");
    let second = base.join("second.md");
    std::fs::write(&first, b"first body").unwrap();
    std::fs::write(&second, b"second body").unwrap();

    let content_state = catalogue::State::empty(
        &base.join("state.sqlite"),
        crate::catalogue::CONTENT_NAMESPACE,
    )
    .unwrap();
    let batch = pending_batch_with_byte_limit(
        vec![first.clone(), second.clone()],
        &content_state,
        1,
        u64::MAX,
    );
    assert_eq!(batch.files, vec![first]);
    assert!(batch.has_more);

    let batch = pending_batch_with_byte_limit(vec![second.clone()], &content_state, 2, u64::MAX);
    assert_eq!(batch.files, vec![second]);
    assert!(!batch.has_more);
}

/// A tag write changes an extended attribute and nothing else, so only
/// forgetting the row makes the pass read the file again.
#[test]
fn forgotten_row_is_read_again() {
    let base = crate::test_env::scratch_dir("lsearch-content-forget");
    let note = base.join("note.md");
    std::fs::write(&note, b"a body").unwrap();
    let (mtime, size) = catalogue::meta(&note).unwrap();

    let mut content_state = catalogue::State::empty(
        &base.join("state.sqlite"),
        crate::catalogue::CONTENT_NAMESPACE,
    )
    .unwrap();
    content_state.set(&note, mtime, size);
    assert!(
        pending_batch_with_byte_limit(vec![note.clone()], &content_state, 10, u64::MAX)
            .files
            .is_empty(),
        "the file is unchanged, so the pass has nothing to do"
    );

    content_state.remove(&note);
    assert_eq!(
        pending_batch_with_byte_limit(vec![note.clone()], &content_state, 10, u64::MAX).files,
        vec![note],
        "forgetting the row is what makes the same bytes worth reading again"
    );
}

#[test]
fn pending_batch_stops_at_byte_budget() {
    let base = crate::test_env::scratch_dir("lsearch-content-byte-budget");
    let first = base.join("first.txt");
    let second = base.join("second.txt");
    std::fs::write(&first, vec![b'a'; 1024 * 1024]).unwrap();
    std::fs::write(&second, vec![b'b'; 1024 * 1024]).unwrap();

    let content_state = catalogue::State::empty(
        &base.join("state.sqlite"),
        crate::catalogue::CONTENT_NAMESPACE,
    )
    .unwrap();
    let batch =
        pending_batch_with_byte_limit(vec![first.clone(), second], &content_state, 50, 1024 * 1024);
    assert_eq!(batch.files, vec![first]);
    assert!(batch.has_more);
}

#[test]
fn pending_batch_prioritizes_low_cost_files() {
    let base = crate::test_env::scratch_dir("lsearch-content-priority");
    let large_pdf = base.join("large.pdf");
    let small_note = base.join("small.md");
    let large = std::fs::File::create(&large_pdf).unwrap();
    large.set_len(16 * 1024 * 1024).unwrap();
    drop(large);
    std::fs::write(&small_note, b"small personal note").unwrap();

    let content_state = catalogue::State::empty(
        &base.join("state.sqlite"),
        crate::catalogue::CONTENT_NAMESPACE,
    )
    .unwrap();
    let batch = pending_batch_with_byte_limit(
        vec![large_pdf, small_note.clone()],
        &content_state,
        1,
        8 * 1024 * 1024,
    );
    assert_eq!(batch.files, vec![small_note]);
    assert!(batch.has_more);
}

#[test]
fn catalog_batch_defers_recent_edits() {
    let base = crate::test_env::scratch_dir("lsearch-content-cooldown");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // Real files: a catalogue entry with nothing on disk is tombstoned
    // rather than queued, so a fictional path would not exercise this at all.
    let quiet = base.join("quieto.md");
    let editing = base.join("sendo-editado.md");
    std::fs::write(&quiet, b"conteudo").unwrap();
    std::fs::write(&editing, b"conteudo").unwrap();

    let mut catalog =
        catalogue::State::empty(&base.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE)
            .unwrap();
    catalog.set(&quiet, now - 3_600, 10); // untouched for 1h
    catalog.set(&editing, now - 10, 20); // saved 10s ago
    catalog.persist().unwrap();

    let cooldown = Duration::from_secs(900);
    let (batch, _) =
        pending_catalog_batch_with_byte_limit(&catalog, None, 10, u64::MAX, cooldown).unwrap();
    assert_eq!(batch.files, vec![quiet.clone()]);
    assert_eq!(batch.deferred, 1, "file inside cooldown must be deferred");
    let retry = batch.retry_in.expect("deferred file sets a retry");
    assert!(retry <= cooldown && retry >= Duration::from_secs(1));

    // A warm pass (search happened) bypasses the cooldown entirely.
    let (warm, _) =
        pending_catalog_batch_with_byte_limit(&catalog, None, 10, u64::MAX, Duration::ZERO)
            .unwrap();
    assert_eq!(warm.files.len(), 2);
    assert_eq!(warm.deferred, 0);
}

/// A deleted file must stop answering content searches.
#[test]
fn deleted_files_stop_answering_content_searches() {
    let _lock = ENV_LOCK.lock().unwrap();
    let base = crate::test_env::scratch_dir("lsearch-ghost");
    std::fs::create_dir_all(base.join("files")).unwrap();
    let _env = [
        EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap()),
        EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap()),
        EnvVarGuard::unset("BIG_SEARCH_DEDUP_CONTENT"),
        EnvVarGuard::unset("BIG_SEARCH_NAMES_ONLY"),
    ];

    let kept = base.join("files").join("mantido.md");
    let removed = base.join("files").join("removido.md");
    std::fs::write(&kept, b"palavrachave em um arquivo que fica").unwrap();
    std::fs::write(&removed, b"palavrachave em um arquivo que some").unwrap();

    let mut catalog = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::NAME_NAMESPACE,
    );
    for path in [&kept, &removed] {
        let (mtime, size) = catalogue::meta(path).unwrap();
        catalog.set(path, mtime, size);
    }
    catalog.persist().unwrap();

    let index = crate::index::open_content_or_create(&base.join("content-idx")).unwrap();
    assert_eq!(index_pending_catalog(&index).unwrap(), 2);
    assert_eq!(
        crate::query::search_content(&index.reader().unwrap(), "palavrachave", 10)
            .unwrap()
            .len(),
        2
    );

    // The file goes, and so does its catalogue row, as the
    // watcher and the scan sweeps do on a delete.
    std::fs::remove_file(&removed).unwrap();
    let mut catalog = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::NAME_NAMESPACE,
    );
    catalog.remove(&removed);
    catalog.persist().unwrap();

    let (pruned, more) = prune_orphaned_content(&index, 100).unwrap();
    assert_eq!(pruned, 1);
    assert!(!more);

    let hits = crate::query::search_content(&index.reader().unwrap(), "palavrachave", 10).unwrap();
    assert_eq!(hits.len(), 1, "the deleted file must not answer any more");
    assert_eq!(hits[0].path, kept.to_string_lossy());

    // Idempotent: nothing left to prune on a second run.
    assert_eq!(prune_orphaned_content(&index, 100).unwrap(), (0, false));
}

/// The sweep must not become a one-shot: clearing content state is exactly
/// what strands documents, so it has to re-arm the sweep that finds them.
#[test]
fn clearing_state_rearms_unreferenced_sweep() {
    let _lock = ENV_LOCK.lock().unwrap();
    let base = crate::test_env::scratch_dir("lsearch-rearm");
    let _env = [
        EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap()),
        EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap()),
    ];
    let dir = crate::config::content_index_dir();
    std::fs::create_dir_all(&dir).unwrap();

    crate::index::mark_unreferenced_pruned(&dir).unwrap();
    assert!(crate::index::is_unreferenced_pruned(&dir), "marcado");

    clear_content_state().unwrap();
    assert!(
        !crate::index::is_unreferenced_pruned(&dir),
        "limpar o estado de conteudo tem de re-armar a varredura"
    );
}

#[test]
fn skipped_entries_become_tombstones() {
    let base = crate::test_env::scratch_dir("lsearch-content-tombstone");

    let movie = base.join("filme.bin");
    let note = base.join("nota.md");
    // Catalogued, then deleted before the content pass reached it: the
    // extraction worker skips it silently and never records a content row,
    // so only the tombstone keeps it from staying pending forever.
    let vanished = base.join("apagado.md");
    // Extension-less, so a sniff candidate, but nothing to extract.
    let folder = base.join("uma-pasta");
    // A text name on a FIFO: opening it would block the worker.
    let pipe = base.join("fila.txt");
    std::fs::write(&movie, b"binario").unwrap();
    std::fs::write(&note, b"texto").unwrap();
    std::fs::create_dir_all(&folder).unwrap();
    crate::test_env::make_fifo(&pipe);

    let mut catalog =
        catalogue::State::empty(&base.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE)
            .unwrap();
    catalog.set(&movie, 100, 50); // never an extraction candidate
    catalog.set(&note, 100, 10);
    catalog.set(&vanished, 100, 10);
    catalog.set(&folder, 100, 4096);
    catalog.set(&pipe, 100, 0);
    catalog.persist().unwrap();

    let (batch, _) =
        pending_catalog_batch_with_byte_limit(&catalog, None, 10, u64::MAX, Duration::ZERO)
            .unwrap();
    assert_eq!(batch.files, vec![note.clone()]);
    assert_eq!(batch.deferred, 0, "a directory must never be deferred");
    assert_eq!(
        batch.non_candidates,
        vec![
            (vanished.clone(), 100, 10),
            (pipe.clone(), 100, 0),
            (movie.clone(), 100, 50),
            (folder.clone(), 100, 4096),
        ],
        "deleted file, FIFO, policy-skipped file and directory are all tombstoned"
    );

    // Record the tombstones + the extracted file in the content namespace:
    // the next pass must report nothing.
    let mut content_state = catalogue::State::empty(
        &base.join("state.sqlite"),
        crate::catalogue::CONTENT_NAMESPACE,
    )
    .unwrap();
    for (path, mtime, size) in &batch.non_candidates {
        content_state.set(path, *mtime, *size);
    }
    content_state.set(&note, 100, 10);
    content_state.persist().unwrap();

    let (next, _) =
        pending_catalog_batch_with_byte_limit(&catalog, None, 10, u64::MAX, Duration::ZERO)
            .unwrap();
    assert!(next.files.is_empty());
    assert!(next.non_candidates.is_empty());
    assert!(!next.has_more);
}

#[test]
fn symlinks_skip_content_extraction() {
    use std::os::unix::fs::symlink;

    // `index_content` writes the content state through `config`, so this must
    // own XDG_DATA_HOME: without the lock it would race other tests for the
    // process-wide variable and, with none set, write to the real database.
    let _lock = ENV_LOCK.lock().unwrap();
    let base = crate::test_env::scratch_dir("lsearch-content-symlink");
    let target = base.join("target.md");
    let link = base.join("link.md");
    let _env = [
        EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap()),
        EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap()),
    ];
    std::fs::write(&target, b"same target").unwrap();
    symlink(&target, &link).unwrap();

    // Through the pipeline: a link must not duplicate its target's body.
    let index = crate::index::open_content_or_create(&base.join("idx")).unwrap();
    index_content_limited(&index, vec![target.clone(), link], None).unwrap();
    let hits = crate::query::search_content(&index.reader().unwrap(), "target", 10).unwrap();
    assert_eq!(hits.len(), 1, "only the real file is extracted");
    assert_eq!(hits[0].path, target.to_string_lossy());
}

#[test]
fn extraction_passes_over_fifo() {
    let _lock = ENV_LOCK.lock().unwrap();
    let base = crate::test_env::scratch_dir("lsearch-content-fifo");
    let note = base.join("note.md");
    let pipe = base.join("pipe.txt");
    let _env = [
        EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap()),
        EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap()),
    ];
    std::fs::write(&note, b"reachable body").unwrap();
    crate::test_env::make_fifo(&pipe);

    // A file listed by a watcher event skips the selection's settling.
    let index = crate::index::open_content_or_create(&base.join("idx")).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker_index = index.clone();
    std::thread::spawn(move || {
        let indexed = index_content_limited(&worker_index, vec![pipe, note], None);
        let _ = done_tx.send(indexed.map_err(|e| format!("{e:#}")));
    });
    let indexed = done_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("extraction waits on the FIFO");
    assert_eq!(indexed, Ok(1));
    let hits = crate::query::search_content(&index.reader().unwrap(), "reachable", 10).unwrap();
    assert_eq!(hits.len(), 1);
}

/// A file another process holds a write lease on is left pending, not recorded
/// unread: the pass after the lease is gone indexes it.
#[test]
fn leased_file_waits_for_next_pass() {
    let _lock = ENV_LOCK.lock().unwrap();
    let base = crate::test_env::scratch_dir("lsearch-content-lease");
    let shared = base.join("shared.md");
    let _env = [
        EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap()),
        EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap()),
    ];
    std::fs::write(&shared, b"leased body").unwrap();
    let index = crate::index::open_content_or_create(&base.join("idx")).unwrap();

    let mut holder = crate::test_env::hold_write_lease(&shared);
    let under_lease = index_content_limited(&index, vec![shared.clone()], None);
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert_eq!(under_lease.unwrap(), 0);
    assert_eq!(
        index_content_limited(&index, vec![shared], None).unwrap(),
        1
    );
    let hits = crate::query::search_content(&index.reader().unwrap(), "leased", 10).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn content_knobs_resolve_overrides() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _clear = [
        EnvVarGuard::unset("BIG_SEARCH_CATALOG_CONTENT_BATCH"),
        EnvVarGuard::unset("BIG_SEARCH_DAEMON_CONTENT_BATCH"),
        EnvVarGuard::unset("BIG_SEARCH_DAEMON_CONTENT_BATCH_MB"),
        EnvVarGuard::unset("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS"),
        EnvVarGuard::unset("BIG_SEARCH_DEDUP_CONTENT"),
    ];
    let low_memory = crate::settings::low_memory_machine();
    let default_batch_bytes = if low_memory { 8u64 } else { 32u64 } << 20;

    assert_eq!(content_channel_capacity(), if low_memory { 2 } else { 4 });
    assert_eq!(catalog_content_batch_limit(), DEFAULT_CATALOG_CONTENT_BATCH);
    assert_eq!(daemon_batch_limit(), 250);
    assert_eq!(daemon_batch_byte_limit(), default_batch_bytes);
    assert_eq!(daemon_idle_delay(), Duration::from_secs(5));
    assert!(!dedupe_content_enabled());

    {
        // Zero is not a usable knob value: fall back to the defaults.
        let _zeros = [
            EnvVarGuard::set("BIG_SEARCH_CATALOG_CONTENT_BATCH", "0"),
            EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_BATCH", "0"),
            EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_BATCH_MB", "0"),
            EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS", "0"),
            EnvVarGuard::set("BIG_SEARCH_DEDUP_CONTENT", "off"),
        ];
        assert_eq!(catalog_content_batch_limit(), DEFAULT_CATALOG_CONTENT_BATCH);
        assert_eq!(daemon_batch_limit(), 250);
        assert_eq!(daemon_batch_byte_limit(), default_batch_bytes);
        assert_eq!(daemon_idle_delay(), Duration::from_secs(5));
        assert!(!dedupe_content_enabled());
    }
    {
        let _values = [
            EnvVarGuard::set("BIG_SEARCH_CATALOG_CONTENT_BATCH", "7"),
            EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_BATCH", "5"),
            EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_BATCH_MB", "2"),
            EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS", "250"),
            EnvVarGuard::set("BIG_SEARCH_DEDUP_CONTENT", "1"),
        ];
        assert_eq!(catalog_content_batch_limit(), 7);
        assert_eq!(daemon_batch_limit(), 5);
        assert_eq!(daemon_batch_byte_limit(), 2 << 20);
        assert_eq!(daemon_idle_delay(), Duration::from_millis(250));
        assert!(dedupe_content_enabled());
    }
}

#[test]
fn worker_pool_width_follows_machine() {
    assert_eq!(pool_width(8, false, None), 2);
    assert_eq!(pool_width(2, false, None), 1);
    assert_eq!(pool_width(8, true, None), 1);
    assert_eq!(
        pool_width(8, false, Some("0")),
        2,
        "0 workers is not usable"
    );
    assert_eq!(pool_width(8, false, Some("three")), 2);
    assert_eq!(pool_width(2, false, Some("3")), 3);
    assert_eq!(
        pool_width(8, false, Some("9")),
        4,
        "pool width is capped at 4"
    );
}

#[test]
fn catalog_byte_budget_is_exclusive() {
    let base = crate::test_env::scratch_dir("lsearch-catalog-byte-boundary");
    let mib = 1u64 << 20;
    // The catalogue records the sizes the budget uses; the files only have to
    // exist, since a missing one is tombstoned instead of queued.
    let (a, b, c) = (base.join("a.txt"), base.join("b.txt"), base.join("c.txt"));
    for path in [&a, &b, &c] {
        std::fs::write(path, b"x").unwrap();
    }
    let mut catalog =
        catalogue::State::empty(&base.join("state.sqlite"), crate::catalogue::NAME_NAMESPACE)
            .unwrap();
    catalog.set(&a, 1_000, mib);
    catalog.set(&b, 1_000, mib);
    catalog.set(&c, 1_000, mib);
    catalog.persist().unwrap();

    // Budget exactly two files: the third exceeds it, equality does not.
    let (batch, _) =
        pending_catalog_batch_with_byte_limit(&catalog, None, 10, 2 * mib, Duration::ZERO).unwrap();
    assert_eq!(batch.files, vec![a.clone(), b.clone()]);
    assert!(batch.has_more);

    // A budget below one file still ships the first file (progress guarantee).
    let (one, _) =
        pending_catalog_batch_with_byte_limit(&catalog, None, 10, 1, Duration::ZERO).unwrap();
    assert_eq!(one.files, vec![a.clone()]);
    assert!(one.has_more);
}

#[test]
fn pending_byte_budget_is_exclusive() {
    let base = crate::test_env::scratch_dir("lsearch-pending-byte-boundary");
    let mib = 1usize << 20;
    let first = base.join("a.txt");
    let second = base.join("b.txt");
    let third = base.join("c.txt");
    for file in [&first, &second, &third] {
        std::fs::write(file, vec![b'x'; mib]).unwrap();
    }
    let content_state = catalogue::State::empty(
        &base.join("state.sqlite"),
        crate::catalogue::CONTENT_NAMESPACE,
    )
    .unwrap();

    let files = vec![first.clone(), second.clone(), third.clone()];
    let batch = pending_batch_with_byte_limit(files.clone(), &content_state, 50, 2 * mib as u64);
    assert_eq!(batch.files, vec![first.clone(), second.clone()]);
    assert!(batch.has_more);

    let all = pending_batch_with_byte_limit(files, &content_state, 50, 8 * mib as u64);
    assert_eq!(all.files, vec![first, second, third]);
    assert!(!all.has_more);
}

#[test]
fn catalog_pass_indexes_every_batch() {
    let _lock = ENV_LOCK.lock().unwrap();
    let base = crate::test_env::scratch_dir("lsearch-catalog-pass");
    std::fs::create_dir_all(base.join("files")).unwrap();
    let _env = [
        EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap()),
        EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap()),
        // One file per batch: the pass must keep walking, not stop early.
        EnvVarGuard::set("BIG_SEARCH_CATALOG_CONTENT_BATCH", "1"),
        EnvVarGuard::unset("BIG_SEARCH_DEDUP_CONTENT"),
        EnvVarGuard::unset("BIG_SEARCH_NAMES_ONLY"),
    ];

    let notes = ["one.md", "two.md", "three.md"].map(|name| base.join("files").join(name));
    for (position, note) in notes.iter().enumerate() {
        std::fs::write(note, format!("corpo da nota {position}")).unwrap();
    }
    let movie = base.join("files").join("movie.bin");
    std::fs::write(&movie, b"not text").unwrap();

    let mut catalog = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::NAME_NAMESPACE,
    );
    for path in notes.iter().chain(std::iter::once(&movie)) {
        let (mtime, size) = catalogue::meta(path).unwrap();
        catalog.set(path, mtime, size);
    }
    catalog.persist().unwrap();

    let index = crate::index::open_content_or_create(&base.join("content-idx")).unwrap();
    assert_eq!(
        index_pending_catalog(&index).unwrap(),
        3,
        "every batch of the pass is extracted"
    );
    assert_eq!(
        index.searchable_segment_ids().unwrap().len(),
        1,
        "the pass ends in one segment"
    );
    let content_state = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::CONTENT_NAMESPACE,
    );
    assert!(
        content_state.contains(&movie),
        "policy-skipped entry becomes a tombstone"
    );
    assert_eq!(index_pending_catalog(&index).unwrap(), 0, "pass converged");

    // The bounded-list entry point counts extracted files, not calls.
    let extra = [
        base.join("files").join("four.md"),
        base.join("files").join("five.md"),
    ];
    for note in &extra {
        std::fs::write(note, b"mais texto pesquisavel").unwrap();
    }
    assert_eq!(
        index_content_limited(&index, extra.to_vec(), None).unwrap(),
        2
    );
}

#[test]
fn body_upsert_keeps_one_document() {
    let base = crate::test_env::scratch_dir("lsearch-content");
    let path = base.join("note.txt");

    let content_index = crate::index::open_content_or_create(&base.join("content-idx")).unwrap();
    let content_fields = crate::index::content_fields(&content_index).unwrap();
    for body in ["old text", "new text"] {
        let mut writer = crate::index::content_writer(&content_index).unwrap();
        upsert_with_body(&writer, &content_fields, &path, body).unwrap();
        writer.commit().unwrap();
    }
    let reader = content_index.reader().unwrap();
    assert_eq!(crate::query::count(&reader), 1);
    assert_eq!(
        crate::query::search_content(&reader, "new", 10)
            .unwrap()
            .len(),
        1
    );
    assert!(
        crate::query::search_content(&reader, "old", 10)
            .unwrap()
            .is_empty()
    );
}
