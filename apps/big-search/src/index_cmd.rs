//! The `reindex` and `daemon` subcommands: the two that hold the index
//! writers and the daemon lock. A child module of the crate root, so
//! `use super::*` supplies the shared imports and the library modules.

use super::*;

pub(super) fn cmd_reindex(roots: &[String]) -> Result<()> {
    // The daemon's lock: a running daemon holds the index writers and the state
    // database, and the recovery below deletes index files from under them.
    // Without a runtime directory no daemon can run, so there is nothing to
    // exclude.
    let _daemon_lock = match config::lock_path() {
        Some(path) => Some(acquire_daemon_lock(&path)?.context(
            "the big-search service is running; use `big-search rebuild` instead, \
             or stop the service first",
        )?),
        None => None,
    };
    throttle::lower_priority();
    settings::seed_config_template();
    scan::seed_user_ignore();
    let mut index = index::open_or_create(&config::index_dir())?;
    let mut content_index = index::open_content_or_create(&config::content_index_dir())?;
    scan::write_effective_ignore()?;

    let (scan_roots, mut state, rebuild_content_from_state) = if roots.is_empty() {
        reconcile_plan(&mut index, &mut content_index)?
    } else {
        explicit_rebuild_plan(roots, &index, &mut content_index)?
    };

    let t0 = Instant::now();
    let sync_outcome = scan::sync(&index, &scan_roots, &mut state)?;
    let names = index.reader()?.searcher().num_docs();
    println!(
        "indexed {names} names in {:.2}s",
        t0.elapsed().as_secs_f64()
    );
    log::info!(
        "sync touched {} files that may need content/metadata",
        sync_outcome.content_candidates
    );
    let c = if settings::content_index_enabled() {
        if rebuild_content_from_state {
            content::clear_content_state()?;
        }
        let indexed = content::index_pending_catalog(&content_index)?;
        index::mark_content_state_synced(&config::content_index_dir())?;
        indexed
    } else {
        clear_content_sidecar(&content_index)?;
        0
    };
    state.persist()?;
    print_reindex_summary(t0, names, c);
    Ok(())
}

fn print_reindex_summary(t0: Instant, names: u64, c: u64) {
    if settings::content_index_enabled() {
        println!(
            "{} content for {c} files ({:.2}s total)",
            green("extracted"),
            t0.elapsed().as_secs_f64()
        );
    } else {
        println!(
            "{} ({:.2}s total)",
            yellow("content disabled; indexed filenames only"),
            t0.elapsed().as_secs_f64()
        );
    }
    // Machine-readable line for the metadata on/off benchmark. RUSAGE_SELF
    // aggregates the worker threads; VmHWM is the whole-process peak.
    if std::env::var_os("BIG_SEARCH_BENCH").is_some() {
        println!(
            "BENCH names={names} content={c} wall_ms={:.1} cpu_ms={:.1} peak_rss_kb={}",
            t0.elapsed().as_secs_f64() * 1000.0,
            self_cpu_ms(),
            peak_rss_kb()
        );
    }
}

/// A reindex of the configured sources: reconcile what changed, rebuilding
/// only what an empty or divergent index forces.
fn reconcile_plan(
    index: &mut tantivy::Index,
    content_index: &mut tantivy::Index,
) -> Result<(Vec<PathBuf>, catalogue::State, bool)> {
    recover_inflated_index(index)?;
    // Config-driven: reconcile the available sources, keeping offline
    // removable/network sources catalogued (a reindex while a device is
    // unplugged must not drop it). A fresh/recreated index starts empty.
    let sources = settings::sources();
    let available: Vec<PathBuf> = sources
        .all()
        .iter()
        .filter(|s| settings::is_available(s))
        .map(|s| s.path.clone())
        .collect();
    log::info!(
        "scan sources: {:?} (of {} configured)",
        available,
        sources.all().len()
    );
    let index_empty = index.reader()?.searcher().num_docs() == 0;
    let state = if index_empty {
        *content_index = index::recreate_content_on_disk(&config::content_index_dir())?;
        content::clear_content_state()?;
        catalogue::State::empty(
            &crate::config::state_db_path(),
            crate::catalogue::NAME_NAMESPACE,
        )
    } else {
        ensure_content_state_matches_index(content_index)?;
        catalogue::State::load(
            &crate::config::state_db_path(),
            crate::catalogue::NAME_NAMESPACE,
        )
    };
    Ok((
        available,
        state,
        !index::is_content_state_synced(&config::content_index_dir()),
    ))
}

/// A reindex of explicit roots: a clean full rebuild of exactly those paths.
fn explicit_rebuild_plan(
    roots: &[String],
    index: &tantivy::Index,
    content_index: &mut tantivy::Index,
) -> Result<(Vec<PathBuf>, catalogue::State, bool)> {
    let explicit: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
    log::info!("scan roots (explicit): {explicit:?}");
    let mut writer = index::bulk_writer(index)?;
    writer.delete_all_documents()?;
    writer.commit()?;
    *content_index = index::recreate_content_on_disk(&config::content_index_dir())?;
    content::clear_content_state()?;
    Ok((
        explicit,
        catalogue::State::empty(
            &crate::config::state_db_path(),
            crate::catalogue::NAME_NAMESPACE,
        ),
        false,
    ))
}

fn clear_content_sidecar(content_index: &tantivy::Index) -> Result<()> {
    content::clear_content_state()?;
    let mut writer = index::content_writer(content_index).context("content writer")?;
    writer.delete_all_documents()?;
    writer.commit().context("clear content index")?;
    index::mark_content_state_synced(&config::content_index_dir())
}

fn ensure_content_state_matches_index(content_index: &tantivy::Index) -> Result<()> {
    if index::is_content_state_synced(&config::content_index_dir()) {
        return Ok(());
    }
    let cached_content_paths = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::CONTENT_NAMESPACE,
    )
    .len();
    if cached_content_paths == 0 {
        return Ok(());
    }
    // Only an empty index proves the rows lie. Fewer documents than rows is the
    // normal state: policy-skipped files and files with no extractable body
    // keep a row without a document. Treating that as divergence wiped every
    // extraction done so far at each restart before the first pass completed.
    let indexed_content_documents = index::document_count(&content_index.reader()?)?;
    if indexed_content_documents == 0 {
        log::warn!(
            "content index/state divergence detected: {indexed_content_documents} content docs for {cached_content_paths} cached paths; rebuilding content cache"
        );
        content::clear_content_state()?;
    }
    Ok(())
}

/// Peak resident set of this process (KB), from `/proc/self/status` `VmHWM`.
fn peak_rss_kb() -> i64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|kb| kb.parse().ok())
            })
        })
        .unwrap_or(0)
}

/// Total CPU time of this process (all threads), milliseconds, via `getrusage`.
fn self_cpu_ms() -> f64 {
    // SAFETY: getrusage on a zeroed rusage with RUSAGE_SELF is always valid.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
    }
    let ms = |tv: libc::timeval| tv.tv_sec as f64 * 1000.0 + tv.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

/// Take an exclusive, non-blocking advisory lock so exactly one daemon runs per
/// user. Returns the held file on success (keep it alive for the daemon's life;
/// the kernel releases the lock when the fd closes, including on crash), `None` if
/// another daemon already holds it, `Err` only on an actual I/O failure.
fn acquire_daemon_lock(path: &Path) -> Result<Option<std::fs::File>> {
    if let Some(dir) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false) // lock target only; never written, so don't clobber it
        .open(path)
        .with_context(|| format!("open daemon lock {}", path.display()))?;
    // SAFETY: flock on a freshly-opened valid fd; lock lifetime is tied to `file`.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(file));
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(None), // another daemon holds the lock
        _ => Err(anyhow::Error::new(err).context("flock daemon lock")),
    }
}

pub(super) fn cmd_daemon() -> Result<()> {
    // Before anything is written: the data directory must not be readable by
    // other local accounts (it maps the user's whole home).
    config::ensure_private_data_dir().context("secure data directory")?;
    // Single-instance guard: a second daemon must refuse, not steal the socket and
    // deadlock the watcher on the Tantivy writer lock. Held for the whole run.
    // No runtime directory means no private place for the socket and the lock.
    // Serving from `/tmp` instead would put the index behind a socket any local
    // account can replace, so the service refuses to start at all.
    let lock_path = config::lock_path()
        .context("this session has no XDG_RUNTIME_DIR; refusing to serve from /tmp")?;
    let socket_path = config::socket_path().expect("runtime directory checked above");
    let _daemon_lock = match acquire_daemon_lock(&lock_path)? {
        Some(lock) => lock,
        None => {
            log::info!("another big-search daemon is already running — exiting");
            return Ok(());
        }
    };
    let mut index = index::open_or_create(&config::index_dir())?;
    let content_index = index::open_content_or_create(&config::content_index_dir())?;
    if !settings::content_index_enabled() {
        clear_content_sidecar(&content_index)?;
    }
    recover_inflated_index(&mut index)?;

    // `paused` and `activity` mirror indexing state for `status`; the channel
    // carries both fs events and lifecycle commands so the watcher reacts without
    // a polling timer. `pending_content` counts cooldown-deferred extractions.
    let (commands, rx) = std::sync::mpsc::channel::<watch::WatchMessage>();
    let control = ipc::Control {
        commands,
        paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        activity: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(watch::ACTIVITY_INDEXING)),
        pending_content: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        unwatched_dirs: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };
    // One versions store for the whole daemon: the thread that writes versions
    // and the connections that answer questions about them are looking at the
    // same thing, so what one learns the other can say.
    let history: big_search::history::SharedHistory = std::sync::Arc::new(std::sync::Mutex::new(
        big_search::history::History::open(&big_search_config::read().history),
    ));

    // All indexing runs in the watcher thread at background priority; the main
    // thread stays at normal priority so queries are never slowed.
    match throttle::prepare_idle_cgroup() {
        Some(how) => log::info!("indexing runs in its own cgroup ({how})"),
        None => log::info!("indexing shares the service's cgroup (no cgroup delegated)"),
    }
    spawn_watcher(&index, &content_index, &control, rx, &history)?;

    let (name_reader, content_reader) = open_query_readers()?;
    ipc::serve(
        name_reader,
        content_reader,
        &socket_path,
        control,
        config::origin_db_path(),
        history,
    )
}

/// Start the watcher thread: the startup reconcile, then incremental indexing
/// for the daemon's life. It ends the process if it ever returns.
fn spawn_watcher(
    index: &tantivy::Index,
    content_index: &tantivy::Index,
    control: &ipc::Control,
    rx: std::sync::mpsc::Receiver<watch::WatchMessage>,
    history: &big_search::history::SharedHistory,
) -> Result<()> {
    let watch_history = std::sync::Arc::clone(history);
    let watch_commands = control.commands.clone();
    let watch_paused = control.paused.clone();
    let watch_activity = control.activity.clone();
    let watch_pending_content = control.pending_content.clone();
    let watch_unwatched_dirs = control.unwatched_dirs.clone();
    let watch_index = index.clone();
    let watch_content_index = content_index.clone();
    std::thread::Builder::new()
        .name("watcher".into())
        .spawn(move || {
            let _watcher_exit_guard = WatcherExitGuard;
            throttle::lower_priority();
            let start = startup_sync(
                &watch_index,
                &watch_content_index,
                &watch_pending_content,
                &watch_activity,
            );
            // Startup sync + first backfill batch allocate bulk writer arenas;
            // return the freed pages before settling into the resident loop.
            throttle::release_idle_memory();
            if let Err(e) = watch::run(
                watch_index,
                watch_content_index,
                watch::WatchScope::new(&start.roots, start.initial_dirs, Some(start.walk_started)),
                start.state,
                watch::WatchRuntime::new(
                    watch_paused,
                    watch_activity,
                    watch_pending_content,
                    watch_unwatched_dirs,
                ),
                watch_commands,
                rx,
                watch_history,
            ) {
                log::error!("watcher stopped: {e:#}");
            }
        })?;
    Ok(())
}

/// What the startup reconcile hands to the incremental watcher.
struct StartupSync {
    roots: Vec<PathBuf>,
    state: catalogue::State,
    initial_dirs: Vec<PathBuf>,
    walk_started: std::time::SystemTime,
}

/// Reconcile the catalogue against the disk, prune stale content and extract
/// the first content batch. Exits the process when the walk keeps failing.
fn startup_sync(
    index: &tantivy::Index,
    content_index: &tantivy::Index,
    pending_content: &std::sync::atomic::AtomicU64,
    activity: &std::sync::atomic::AtomicU8,
) -> StartupSync {
    let sources = settings::sources();
    let roots: Vec<PathBuf> = sources
        .all()
        .iter()
        .filter(|s| settings::is_available(s))
        .map(|s| s.path.clone())
        .collect();
    log::debug!("scan sources: {roots:?}");
    if let Err(e) = scan::write_effective_ignore() {
        log::warn!("ignore file: {e:#}");
    }
    // Reconcile against the cache: builds a fresh index, or applies only the
    // deltas accumulated while the daemon was down (crash/offline recovery).
    let empty = index
        .reader()
        .map(|r| r.searcher().num_docs() == 0)
        .unwrap_or(true);
    let mut state = startup_name_state(empty, content_index);
    let walk_started = std::time::SystemTime::now();
    let initial_watch_dirs = match sync_with_retry(index, &roots, &mut state) {
        Ok(sync_outcome) => {
            log::debug!(
                "sync: {} changed files may need content",
                sync_outcome.content_candidates
            );
            let _ = state.persist();
            prune_content_at_start(content_index);
            backfill_first_batch(content_index, pending_content, activity);
            let _ = state.persist();
            sync_outcome.visible_dirs
        }
        Err(e) => {
            // A failed startup sync means the index may be missing most
            // of the catalogue (e.g. a sandbox/filesystem fault). Serving
            // a near-empty index silently is worse than dying: exit so
            // systemd restarts the unit (visible failure, fresh attempt).
            log::error!("sync failed after retries — exiting for restart: {e:#}");
            std::process::exit(1);
        }
    };
    StartupSync {
        roots,
        state,
        initial_dirs: initial_watch_dirs,
        walk_started,
    }
}

/// The name state the startup walk reconciles against.
fn startup_name_state(empty: bool, content_index: &tantivy::Index) -> catalogue::State {
    // An empty name index is rebuilt from the walk below. The content
    // index is another matter: its rows are kept unless it is empty too,
    // the same rule as for a full one: a new name schema (which
    // recreates the name index) must not re-extract every document,
    // hours on a slow disk. Rows of files gone by then are pruned after
    // the walk.
    if empty {
        if let Err(e) = ensure_content_state_matches_index(content_index) {
            log::warn!("check content index state: {e:#}");
        }
        catalogue::State::empty(
            &crate::config::state_db_path(),
            crate::catalogue::NAME_NAMESPACE,
        )
    } else {
        if let Err(e) = ensure_content_state_matches_index(content_index) {
            log::warn!("check content index state: {e:#}");
        }
        catalogue::State::load(
            &crate::config::state_db_path(),
            crate::catalogue::NAME_NAMESPACE,
        )
    }
}

fn prune_content_at_start(content_index: &tantivy::Index) {
    // The sync just deleted every catalogue row whose file is
    // gone. Their content documents would otherwise keep
    // answering searches, so clear them before serving, and
    // uncapped, because a first start after an upgrade can face
    // millions of them and the per-pass cap would take days.
    match content::prune_orphaned_content(content_index, usize::MAX) {
        Ok((0, _)) => {}
        Ok((removed, _)) => {
            log::info!("content prune: dropped {removed} row(s) for deleted files")
        }
        Err(e) => log::warn!("content prune at start failed: {e:#}"),
    }
    // Documents no state row references at all, left behind by
    // every past `clear_content_state`. Only a walk of the index
    // itself finds them.
    match content::prune_unreferenced_content_docs(content_index) {
        Ok(0) => {}
        Ok(removed) => {
            log::info!("content prune: dropped {removed} unreferenced document(s)")
        }
        Err(e) => log::warn!("unreferenced content prune failed: {e:#}"),
    }
}

/// Extract the first content batch of this start, publishing progress for
/// `status`.
fn backfill_first_batch(
    content_index: &tantivy::Index,
    pending_content: &std::sync::atomic::AtomicU64,
    activity: &std::sync::atomic::AtomicU8,
) {
    let content_limit = content::daemon_batch_limit();
    match content::pending_catalog_batch(content_limit, settings::content_cooldown()) {
        Ok(pending_batch) => {
            if let Err(e) = content::record_skipped_candidates(&pending_batch.non_candidates) {
                log::warn!("record policy-skipped entries: {e:#}");
            }
            log::debug!(
                "content backfill: {} files this start, {} deferred by cooldown (limit {content_limit} files / {} MiB)",
                pending_batch.files.len(),
                pending_batch.deferred,
                content::daemon_batch_byte_limit() / (1 << 20),
            );
            pending_content.store(
                pending_batch.deferred as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            activity.store(
                watch::ACTIVITY_BACKFILLING,
                std::sync::atomic::Ordering::Relaxed,
            );
            if let Err(e) = content::index_content_limited(
                content_index,
                pending_batch.files,
                Some(content_limit),
            ) {
                log::error!("content pass failed: {e:#}");
            }
            activity.store(watch::ACTIVITY_IDLE, std::sync::atomic::Ordering::Relaxed);
            if !pending_batch.has_more
                && pending_batch.deferred == 0
                && let Err(e) = index::mark_content_state_synced(&config::content_index_dir())
            {
                log::warn!("mark content index synced: {e:#}");
            }
        }
        Err(e) => log::error!("content backfill selection failed: {e:#}"),
    }
}

fn open_query_readers() -> Result<(tantivy::IndexReader, tantivy::IndexReader)> {
    // Main thread serves queries + lifecycle commands; the reader reloads on commit.
    // Built once and held for the daemon's life; `ReloadPolicy::OnCommitWithDelay`
    // (tantivy's default) refreshes them from the meta.json watch after a commit.
    // Their own instances of the indexes, mapped for random reads: the watcher's
    // keep read-ahead for merges and scans (see `index::open_for_queries`).
    let name_reader = index::open_for_queries(&config::index_dir())
        .and_then(|index| index::query_reader(&index))
        .context("open name index reader")?;
    let content_reader = index::open_for_queries(&config::content_index_dir())
        .and_then(|index| index::query_reader(&index))
        .context("open content index reader")?;
    Ok((name_reader, content_reader))
}

struct WatcherExitGuard;

impl Drop for WatcherExitGuard {
    fn drop(&mut self) {
        log::error!("watcher thread exited; terminating daemon");
        std::process::exit(1);
    }
}

/// Startup reconcile with bounded retries: a transient failure (slow mount,
/// I/O hiccup) gets two more chances before the daemon gives up and exits.
fn sync_with_retry(
    index: &tantivy::Index,
    roots: &[PathBuf],
    state: &mut catalogue::State,
) -> Result<scan::SyncOutcome> {
    const ATTEMPTS: u32 = 3;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
    let mut last_error = None;
    for attempt in 1..=ATTEMPTS {
        match scan::sync(index, roots, state) {
            Ok(count) => return Ok(count),
            Err(e) => {
                log::warn!("sync attempt {attempt}/{ATTEMPTS} failed: {e:#}");
                last_error = Some(e);
                if attempt < ATTEMPTS {
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
    }
    Err(last_error.expect("at least one sync attempt ran"))
}

fn recover_inflated_index(index: &mut tantivy::Index) -> Result<()> {
    let mut cached_state = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::NAME_NAMESPACE,
    );
    let cached_paths = cached_state.len() as u64;
    let indexed_documents = index::document_count(&index.reader()?)?;
    if !index::is_divergent_document_count(indexed_documents, cached_paths) {
        return Ok(());
    }

    if index::is_deflated_document_count(indexed_documents, cached_paths) {
        let deficit = cached_paths.saturating_sub(indexed_documents);
        if deficit <= index::max_repairable_name_deficit(cached_paths) {
            log::warn!(
                "index/cache deficit detected: {deficit} missing name docs; repairing index"
            );
            index::repair_deflated_against_state(index, &mut cached_state)?;
            return Ok(());
        }
    }

    log::warn!(
        "index/cache divergence detected: {indexed_documents} indexed docs for {} cached paths; rebuilding index",
        cached_paths
    );
    *index = index::recreate_on_disk(&config::index_dir())?;
    cached_state.clear();
    cached_state.persist()?;
    Ok(())
}
