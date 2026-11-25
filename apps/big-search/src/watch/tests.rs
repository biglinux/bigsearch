use super::event_batch::{MAX_FOLDERS, MAX_PATHS};
use super::*;
use crate::history::HistoryEvent;
use crate::test_env::{ENV_LOCK, EnvVarGuard, scratch_dir};
use notify::event::{CreateKind, EventKind, ModifyKind, RenameMode};
use std::collections::BTreeSet;

/// Guards: persisted data: a rename the inotify thread passes on after the
/// stop, behind a request, still reaches the versions worker.
#[test]
fn stop_window_keeps_late_rename() {
    let _lock = ENV_LOCK.lock().unwrap();
    let base = scratch_dir("bs-watch-stop");
    let _data = EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap());
    let _config = EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap());
    let roots = [base.join("docs")];
    let (watch_loop, tx, history_rx) = stopping_loop(&base, &roots);
    let (from, to) = (roots[0].join("a.txt"), roots[0].join("b.txt"));
    let renamed = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
        .add_path(from.clone())
        .add_path(to.clone());
    tx.send(WatchMessage::Stop(0)).unwrap();
    tx.send(WatchMessage::ControlCommand(
        WatchControlCommand::WarmContent,
    ))
    .unwrap();
    // Well inside the window, and after the loop took the stop.
    let heard = Arc::clone(&watch_loop.heard);
    let late = std::thread::spawn(move || {
        std::thread::sleep(DEBOUNCE / 5);
        heard.hear(Ok(renamed));
    });
    assert_eq!(watch_loop.run_until_stopped().unwrap(), 0);
    late.join().unwrap();
    let heard: Vec<HistoryEvent> = history_rx.try_iter().collect();
    assert!(
        heard
            .iter()
            .any(|event| matches!(event, HistoryEvent::Renamed(a, b) if *a == from && *b == to)),
        "the versions worker heard {} events, not the rename",
        heard.len()
    );
}

/// A burst past [`MAX_PATHS`] collapses into the folders that hold its
/// paths, a source standing for itself, and past [`MAX_FOLDERS`] into a walk
/// of the sources; the loop is woken once per batch.
#[test]
fn heard_paths_collapse_then_rescan() {
    let root = PathBuf::from("/r");
    let (tx, rx) = std::sync::mpsc::channel();
    let heard = Heard::new(vec![root.clone()], tx);
    let hear = |path: PathBuf| heard.hear(Ok(created(path)));
    let folders =
        |paths: &[&str]| -> BTreeSet<PathBuf> { paths.iter().map(PathBuf::from).collect() };

    for n in 0..=MAX_PATHS {
        hear(root.join(["a", "b/c"][n % 2]).join(format!("f{n}")));
    }
    hear(root.join("a/late"));
    let batch = heard.take().unwrap();
    assert!(batch.paths.is_empty() && !batch.rescan);
    assert_eq!(batch.folders, folders(&["/r/a", "/r/b/c"]));
    assert_eq!(rx.try_iter().count(), 1, "one wake per batch");

    hear(root.clone());
    for n in 0..MAX_PATHS {
        hear(root.join("a").join(format!("f{n}")));
    }
    let batch = heard.take().unwrap();
    assert_eq!(batch.folders, folders(&["/r"]));

    for n in 0..=MAX_PATHS {
        hear(root.join(format!("d{}/f{n}", n % (MAX_FOLDERS + 1))));
    }
    hear(root.join("after"));
    let batch = heard.take().unwrap();
    assert!(batch.rescan);
    assert!(batch.paths.is_empty() && batch.folders.is_empty());
    assert!(heard.take().is_none());
}

/// Guards: an in-place save heard among more paths than a batch holds still
/// reaches the catalogue, through the walk of its folder.
#[test]
fn collapsed_folder_reads_changes() {
    let _lock = ENV_LOCK.lock().unwrap();
    let base = scratch_dir("bs-watch-collapse");
    let _data = EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap());
    let _config = EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap());
    let roots = [base.join("docs")];
    let folder = roots[0].join("sub");
    let file = folder.join("a.txt");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(&file, "a").unwrap();
    let (mut watch_loop, _tx, _history_rx) = stopping_loop(&base, &roots);
    watch_loop.walk_sources().unwrap();
    let size = |watch_loop: &WatchLoop<'_>| watch_loop.state.recorded(&file).map(|(_, size)| size);
    assert_eq!(size(&watch_loop), Some(1));

    // Rewritten in place, so its folder's time does not move.
    std::fs::write(&file, "edited").unwrap();
    for n in 0..=MAX_PATHS {
        watch_loop
            .heard
            .hear(Ok(created(folder.join(format!("unpacked-{n}")))));
    }
    let batch = watch_loop.heard.take().unwrap();
    assert_eq!(batch.folders, BTreeSet::from([folder]));
    watch_loop.apply_batch(batch).unwrap();
    assert_eq!(size(&watch_loop), Some(6));
}

fn created(path: PathBuf) -> notify::Event {
    notify::Event::new(EventKind::Create(CreateKind::File)).add_path(path)
}

/// A watcher loop over `roots` with its index and catalogue under `base`, and
/// the ends a test drives it through: its message sender and what the
/// versions worker hears.
fn stopping_loop<'a>(
    base: &std::path::Path,
    roots: &'a [PathBuf],
) -> (WatchLoop<'a>, Sender<WatchMessage>, Receiver<HistoryEvent>) {
    let index = crate::index::open_or_create(&base.join("index")).unwrap();
    let content_index = crate::index::create_content_fixture(
        &base.join("content"),
        tantivy::schema::IndexRecordOption::Basic,
    )
    .unwrap();
    let state_db = base.join("state.db");
    let (tx, rx) = std::sync::mpsc::channel();
    let (history_tx, history_rx) = std::sync::mpsc::channel();
    let runtime = WatchRuntime::new(
        Arc::default(),
        Arc::default(),
        Arc::default(),
        Arc::default(),
    );
    let watch_list = State::empty(&state_db, crate::catalogue::WATCH_NAMESPACE).unwrap();
    let inotify = start_inotify(tx.clone(), roots.to_vec()).unwrap();
    let watches = Watches::new(inotify.watcher, &runtime, watch_list, None);
    let watch_loop = WatchLoop {
        check_sources_at: None,
        content_backfill: ContentBackfill::new(Arc::default()),
        stop_status: None,
        history_pass_done: true,
        history_cursor: None,
        history_tx,
        origin_writer: origin::OriginWriter::disabled(),
        watches,
        heard: inotify.heard,
        path_scope: scan::Scope::compile(),
        content_writer: None,
        writer: Some(crate::index::background_writer(&index).unwrap()),
        content_fields: content_fields(&content_index).unwrap(),
        f: fields(&index).unwrap(),
        rx,
        runtime,
        state: State::empty(&state_db, crate::catalogue::NAME_NAMESPACE).unwrap(),
        roots,
        content_index,
        index,
    };
    (watch_loop, tx, history_rx)
}
