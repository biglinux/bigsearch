use super::*;
use crate::history::HistoryEvent;
use crate::test_env::{ENV_LOCK, EnvVarGuard, scratch_dir};
use notify::event::{ModifyKind, RenameMode};

/// Guards: persisted data: a rename the inotify thread passes on after the
/// stop, behind a request, still reaches the versions worker.
#[test]
fn stop_window_keeps_late_rename() {
    let _lock = ENV_LOCK.lock().unwrap();
    let base = scratch_dir("bs-watch-stop");
    let _data = EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap());
    let _config = EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap());
    let roots = [base.join("docs")];
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
    let (watches, queued, overflowed) =
        start_inotify(tx.clone(), &runtime, watch_list, None).unwrap();
    let watch_loop = WatchLoop {
        check_sources_at: None,
        content_backfill: ContentBackfill::new(Arc::default()),
        stop_status: None,
        history_pass_done: true,
        history_cursor: None,
        history_tx,
        origin_writer: origin::OriginWriter::disabled(),
        watches,
        overflowed,
        queued: Arc::clone(&queued),
        path_scope: scan::Scope::compile(),
        content_writer: None,
        writer: Some(crate::index::background_writer(&index).unwrap()),
        content_fields: content_fields(&content_index).unwrap(),
        f: fields(&index).unwrap(),
        rx,
        runtime,
        state: State::empty(&state_db, crate::catalogue::NAME_NAMESPACE).unwrap(),
        roots: &roots,
        content_index,
        index,
    };
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
    let late = std::thread::spawn(move || {
        std::thread::sleep(DEBOUNCE / 5);
        queued.fetch_add(1, Ordering::Relaxed);
        tx.send(WatchMessage::Fs(Ok(renamed))).unwrap();
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
