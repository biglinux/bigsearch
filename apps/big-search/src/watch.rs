//! Incremental index updates via inotify (notify crate), scoped to visible dirs.
//!
//! One non-recursive watch per visible directory (tens of thousands on a
//! typical home, well under the kernel `max_user_watches`): unprivileged, and
//! the hidden/gitignored churn (most changes) is never watched, so it never
//! wakes us. New visible dirs get a watch + a scoped rescan. Events are
//! coalesced over a short window, then reconciled by existence: present →
//! upsert, gone → delete.
mod content_backfill;
mod event_batch;

use crate::catalogue::{self, State};
use crate::content;
use crate::index::{ContentFields, Fields, content_fields, fields};
use crate::origin;
use crate::scan;
use anyhow::{Context, Result};
use content_backfill::{ContentBackfill, compact_when_worth_it, run_content_backfill_batch};
use event_batch::{WatchBatch, ingest, observe_origin_batch};
use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use tantivy::{Index, IndexWriter};

const DEBOUNCE: Duration = Duration::from_millis(250);
/// How often an idle watcher offers the versions worker another page of the
/// catalogue.
///
/// Slow on purpose: this is the pass that gives every document its first
/// version, and on a slow disk it should be a trickle nobody notices rather than
/// a burst that makes the machine feel busy.
const HISTORY_PASS_TICK: Duration = Duration::from_millis(750);
/// Filesystem events waiting for the watcher, at most. Past it they are
/// dropped and the loop rescans, as the kernel does when its own queue fills:
/// a backup or a recursive copy must not queue its whole tree in memory.
const MAX_QUEUED_EVENTS: usize = 16_384;
pub const ACTIVITY_IDLE: u8 = 0;
pub const ACTIVITY_INDEXING: u8 = 1;
pub const ACTIVITY_BACKFILLING: u8 = 2;

pub fn activity_label(paused: bool, activity: u8) -> &'static str {
    if paused {
        "paused"
    } else {
        match activity {
            ACTIVITY_INDEXING => "indexing",
            ACTIVITY_BACKFILLING => "backfilling",
            _ => "idle",
        }
    }
}

#[derive(Clone)]
pub struct WatchRuntime {
    paused: Arc<AtomicBool>,
    activity: Arc<AtomicU8>,
    pending_content: Arc<AtomicU64>,
    /// Folders a watch could not be placed on, for `status`.
    unwatched_dirs: Arc<AtomicU64>,
}

pub struct WatchScope<'a> {
    roots: &'a [PathBuf],
    initial_dirs: Vec<PathBuf>,
    /// When the walk that found `initial_dirs` began. A folder changed since
    /// then may hold entries the walk missed and no watch reported.
    walk_started: Option<std::time::SystemTime>,
}

impl<'a> WatchScope<'a> {
    pub fn new(
        roots: &'a [PathBuf],
        initial_dirs: Vec<PathBuf>,
        walk_started: Option<std::time::SystemTime>,
    ) -> Self {
        Self {
            roots,
            initial_dirs,
            walk_started,
        }
    }
}

/// The watcher and how many watches it failed to place, shared with `status`.
struct Watches {
    watcher: RecommendedWatcher,
    failed: Arc<AtomicU64>,
}

impl Watches {
    /// Watch one folder. The usual failure is the inotify watch limit, which
    /// leaves every later folder unwatched too, so the first one is logged.
    fn watch(&mut self, dir: &Path) -> bool {
        match self.watcher.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => true,
            Err(error) => {
                if self.failed.fetch_add(1, Ordering::Relaxed) == 0 {
                    log::warn!(
                        "cannot watch {}: {error}; changes in it and in other unwatched \
                         folders are seen only by the next full reconcile",
                        dir.display()
                    );
                }
                false
            }
        }
    }
}

impl WatchRuntime {
    pub fn new(
        paused: Arc<AtomicBool>,
        activity: Arc<AtomicU8>,
        pending_content: Arc<AtomicU64>,
        unwatched_dirs: Arc<AtomicU64>,
    ) -> Self {
        Self {
            paused,
            activity,
            pending_content,
            unwatched_dirs,
        }
    }
}

/// Index lifecycle command from the IPC thread (the `big-indexd` pause/resume/
/// rebuild verbs). Carried on the same channel as filesystem events so the watcher
/// reacts immediately and still never wakes when there is nothing to do.
pub enum WatchControlCommand {
    Pause,
    Resume,
    Rebuild,
    /// Index this one path again even though nothing about its size or its
    /// modification time changed.
    ///
    /// The incremental check exists to skip exactly this case, and it is right to:
    /// a `chmod` or a `touch -a` must not cost a re-index. But writing a *tag*
    /// changes only an extended attribute, and then the file's searchable text is
    /// stale with no event that says so. The program that wrote the tag asks for
    /// this explicitly, which is cheaper and far less noisy than teaching the
    /// watcher to believe every `ATTRIB`.
    ReindexPath(std::path::PathBuf),
    /// A content search happened: extract every pending body now, bypassing the
    /// edit cooldown, so the next search sees the freshest content.
    WarmContent,
}

/// What the watcher loop consumes: a filesystem event or a control command.
pub enum WatchMessage {
    Fs(notify::Result<notify::Event>),
    ControlCommand(WatchControlCommand),
}

struct ContentIndexWrite<'a> {
    index: &'a Index,
    writer: &'a mut Option<IndexWriter>,
}

/// Watch `roots` and keep `index` (and the `state` cache) in sync until the channel
/// closes. `paused` mirrors the runtime state for the IPC `status` reply. `tx`/`rx`
/// are the shared `WatchMessage` channel: `tx` is cloned into the inotify callback, and the
/// IPC thread holds another clone to deliver control commands. Blocking.
pub fn run(
    index: Index,
    content_index: Index,
    scope: WatchScope<'_>,
    mut state: State,
    runtime: WatchRuntime,
    tx: Sender<WatchMessage>,
    rx: Receiver<WatchMessage>,
    history: crate::history::SharedHistory,
) -> Result<()> {
    let f = fields(&index)?;
    let content_fields = content_fields(&content_index)?;
    // Resident for the daemon's whole life, so size it at tantivy's minimum arena
    // (15 MB) rather than the 64 MB bulk-reindex budget: incremental batches are
    // tiny (debounced fs events), and this is the dominant term in steady-state RSS.
    let mut writer = Some(crate::index::background_writer(&index).context("index writer")?);

    // Compiled once: every event is tested against the same rules the scan walk
    // uses, so the two cannot disagree about what belongs in the index.
    let path_scope = scan::Scope::compile();

    let (mut watches, queued, overflowed) = start_inotify(tx, &runtime.unwatched_dirs)?;
    let initial_dirs = watch_initial_dirs(&mut watches, scope.roots, scope.initial_dirs);
    catch_up_after_walk(
        &mut writer,
        &f,
        &mut watches,
        &mut state,
        &path_scope,
        &initial_dirs,
        scope.walk_started,
    )?;
    drop(initial_dirs);
    let origin_writer = open_origin_writer();
    let history_tx = spawn_history_worker(&history);

    let mut content_backfill = ContentBackfill::new(runtime.pending_content.clone());
    content_backfill.request();
    runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);

    WatchLoop {
        content_backfill,
        history_pass_done: false,
        history_cursor: None,
        history_tx,
        origin_writer,
        watches,
        overflowed,
        queued,
        path_scope,
        content_writer: None,
        writer,
        content_fields,
        f,
        rx,
        runtime,
        state,
        roots: scope.roots,
        content_index,
        index,
    }
    .run_until_closed()
}

/// Start inotify. Returns the watches, the count of events the callback has
/// sent and the loop not yet received, and the flag it raises when it had to
/// drop one.
fn start_inotify(
    tx: Sender<WatchMessage>,
    unwatched_dirs: &Arc<AtomicU64>,
) -> Result<(Watches, Arc<AtomicUsize>, Arc<AtomicBool>)> {
    let fs_tx = tx;
    let queued = Arc::new(AtomicUsize::new(0));
    let overflowed = Arc::new(AtomicBool::new(false));
    let (callback_queued, callback_overflowed) = (Arc::clone(&queued), Arc::clone(&overflowed));
    let watcher = recommended_watcher(move |watcher_result: notify::Result<notify::Event>| {
        // notify always asks for IN_OPEN; a read changes nothing and must not
        // wake the indexer. A close after writing is how a save ends.
        if let Ok(event) = &watcher_result
            && matches!(event.kind, EventKind::Access(kind)
                if kind != AccessKind::Close(AccessMode::Write))
        {
            return;
        }
        if callback_queued.fetch_add(1, Ordering::Relaxed) >= MAX_QUEUED_EVENTS {
            callback_queued.fetch_sub(1, Ordering::Relaxed);
            callback_overflowed.store(true, Ordering::Relaxed);
            return;
        }
        let _ = fs_tx.send(WatchMessage::Fs(watcher_result));
    })
    .context("inotify init")?;
    let watches = Watches {
        watcher,
        failed: Arc::clone(unwatched_dirs),
    };
    Ok((watches, queued, overflowed))
}

/// Watch every folder the startup walk found (every visible folder under
/// `roots` when it found none) and return that list.
fn watch_initial_dirs(
    watches: &mut Watches,
    roots: &[PathBuf],
    initial_dirs: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut watched = 0usize;
    let initial_dirs = if initial_dirs.is_empty() {
        roots
            .iter()
            .flat_map(|root| scan::visible_dirs(root))
            .collect()
    } else {
        initial_dirs
    };
    for dir in &initial_dirs {
        if watches.watch(dir) {
            watched += 1;
        }
    }
    log::info!("watching {watched} visible directories");
    let unwatched = watches.failed.load(Ordering::Relaxed);
    if unwatched > 0 {
        log::warn!(
            "{unwatched} directories could not be watched; raise fs.inotify.max_user_watches"
        );
    }
    initial_dirs
}

/// What changed between the startup walk reaching a folder and its watch:
/// a folder whose entries changed since the walk began is compared again,
/// its entries on disk and in the catalogue. Only its direct entries; a new
/// folder among them is walked whole, a vanished one leaves with its tree.
fn catch_up_after_walk(
    writer: &mut Option<IndexWriter>,
    f: &Fields,
    watches: &mut Watches,
    state: &mut State,
    path_scope: &scan::Scope,
    initial_dirs: &[PathBuf],
    walk_started: Option<std::time::SystemTime>,
) -> Result<()> {
    let Some(walk_started) = walk_started else {
        return Ok(());
    };
    let since = walk_started - Duration::from_secs(1);
    let mut missed = HashSet::new();
    for dir in initial_dirs {
        let changed = std::fs::symlink_metadata(dir)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified >= since);
        if changed && let Ok(entries) = std::fs::read_dir(dir) {
            missed.extend(entries.flatten().map(|entry| entry.path()));
            missed.extend(state.children(dir)?);
        }
    }
    if !missed.is_empty() {
        let outcome = reconcile(
            active_index_writer(writer),
            f,
            watches,
            state,
            path_scope,
            missed,
        )?;
        if outcome.index_changed {
            active_index_writer(writer)
                .commit()
                .context("commit startup catch-up")?;
        }
        if outcome.any() {
            state.persist()?;
        }
    }
    Ok(())
}

/// The origin database, with what the browsers already know imported into it.
fn open_origin_writer() -> origin::OriginWriter {
    let origin_writer = match origin::OriginWriter::open(crate::config::origin_db_path()) {
        Ok(writer) => writer,
        Err(e) => {
            log::warn!("origin watcher disabled: {e:#}");
            origin::OriginWriter::disabled()
        }
    };
    // What the browsers already know about where files came from, read once at
    // start. Off unless the person turned it on; see `browser_downloads`.
    crate::browser_downloads::import(&origin_writer);
    origin_writer
}

/// Earlier versions of documents live in their own thread, fed by this one.
/// Separate on purpose: a version that cannot be written must never delay the
/// search, and the store it writes to is the person's data rather than a
/// cache this service can rebuild.
fn spawn_history_worker(
    history: &crate::history::SharedHistory,
) -> Sender<crate::history::HistoryEvent> {
    let (history_tx, history_rx) = std::sync::mpsc::channel::<crate::history::HistoryEvent>();
    let history_store = std::sync::Arc::clone(history);
    std::thread::Builder::new()
        .name("history".into())
        .spawn(move || crate::history::run_worker(&history_store, history_rx))
        .ok();
    history_tx
}

/// What the watcher loop owns between messages.
struct WatchLoop<'a> {
    content_backfill: ContentBackfill,
    history_pass_done: bool,
    /// Where the pass over the catalogue has got to. It covers what no event
    /// could: the moments the service was not running, and the bursts that
    /// overflowed the kernel's queue.
    history_cursor: Option<String>,
    history_tx: Sender<crate::history::HistoryEvent>,
    origin_writer: origin::OriginWriter,
    watches: Watches,
    /// Set by the inotify callback when it dropped an event.
    overflowed: Arc<AtomicBool>,
    /// Events the inotify callback sent and the loop has not received yet.
    queued: Arc<AtomicUsize>,
    path_scope: scan::Scope,
    content_writer: Option<IndexWriter>,
    writer: Option<IndexWriter>,
    content_fields: ContentFields,
    f: Fields,
    rx: Receiver<WatchMessage>,
    runtime: WatchRuntime,
    state: State,
    roots: &'a [PathBuf],
    content_index: Index,
    index: Index,
}

impl WatchLoop<'_> {
    fn run_until_closed(mut self) -> Result<()> {
        while let Some(message) = self.next_message() {
            self.handle(message)?;
        }
        Ok(())
    }

    /// Wait for the next message, doing idle work (a content backfill batch,
    /// a page of the versions pass) each time the wait runs out. `None` once
    /// the channel is closed.
    fn next_message(&mut self) -> Option<WatchMessage> {
        loop {
            if self
                .content_backfill
                .should_run_when_idle(&self.runtime.paused)
            {
                match self
                    .rx
                    .recv_timeout(self.content_backfill.wait_before_batch())
                {
                    Ok(message) => return Some(message),
                    Err(RecvTimeoutError::Timeout) => self.idle_backfill_turn(),
                    Err(RecvTimeoutError::Disconnected) => return None,
                }
            } else if self.history_pass_done || self.runtime.paused.load(Ordering::Relaxed) {
                // Paused stops the versions pass too; Resume picks it up.
                return self.rx.recv().ok();
            } else {
                // The pass over the catalogue also runs when there is no indexing
                // left to do, which is most of the time: blocking on the channel
                // would make the first version of anything wait for the next file
                // to change.
                match self.rx.recv_timeout(HISTORY_PASS_TICK) {
                    Ok(message) => return Some(message),
                    Err(RecvTimeoutError::Timeout) => {
                        self.history_pass_done = send_history_page(
                            &self.state,
                            &self.history_tx,
                            &mut self.history_cursor,
                        );
                    }
                    Err(RecvTimeoutError::Disconnected) => return None,
                }
            }
        }
    }

    /// Nothing arrived before the next backfill batch was due: one page of the
    /// versions pass, one content batch, and compaction once a pass is done.
    fn idle_backfill_turn(&mut self) {
        self.content_backfill.snooze_until = None; // slept through the cooldown
        self.content_backfill.next_batch_at = None;
        // One page of the catalogue per idle turn, and only while
        // the machine has nothing better to do.
        if !self.history_pass_done {
            self.history_pass_done =
                send_history_page(&self.state, &self.history_tx, &mut self.history_cursor);
        }
        self.runtime
            .activity
            .store(ACTIVITY_BACKFILLING, Ordering::Relaxed);
        match run_content_backfill_batch(
            &self.content_index,
            &mut self.content_writer,
            &self.content_fields,
            &mut self.content_backfill,
        ) {
            // Nothing left to read: the time to tidy the indexes, unless
            // the charge is someone else's.
            Ok(true) if !crate::throttle::on_battery() => {
                self.content_backfill.failures = 0;
                compact_when_worth_it(&self.index, active_index_writer(&mut self.writer), "name");
                match open_content_writer(&self.content_index, &mut self.content_writer) {
                    Ok(content) => compact_when_worth_it(&self.content_index, content, "content"),
                    Err(e) => log::warn!("content index not compacted: {e:#}"),
                }
            }
            Ok(_) => self.content_backfill.failures = 0,
            Err(e) => {
                log::error!("content backfill failed: {e:#}");
                // A writer that failed mid-batch is not trusted for the next one.
                let _ = close_index_writer(&mut self.content_writer);
                self.content_backfill.retry_after_failure();
            }
        }
        self.runtime
            .activity
            .store(ACTIVITY_IDLE, Ordering::Relaxed);
    }

    fn handle(&mut self, message: WatchMessage) -> Result<()> {
        let first = match message {
            WatchMessage::ControlCommand(WatchControlCommand::WarmContent) => {
                self.content_backfill.warm();
                return Ok(());
            }
            WatchMessage::ControlCommand(WatchControlCommand::ReindexPath(path)) => {
                return self.reindex_path(path);
            }
            WatchMessage::ControlCommand(command) => return self.apply_control(command),
            WatchMessage::Fs(watcher_result) => {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                watcher_result
            }
        };
        if self.runtime.paused.load(Ordering::Relaxed) {
            return Ok(()); // dropped while paused; Resume reconciles what was missed
        }
        self.index_burst(first)
    }

    fn reindex_path(&mut self, path: PathBuf) -> Result<()> {
        // ipc.rs refuses these with an answer; this is the backstop for
        // the sources having changed since.
        if self.runtime.paused.load(Ordering::Relaxed)
            || !path.is_absolute()
            || !self.roots.iter().any(|root| path.starts_with(root))
        {
            log::warn!("reindex of {} refused: outside the roots", path.display());
            return Ok(());
        }
        self.runtime
            .activity
            .store(ACTIVITY_INDEXING, Ordering::Relaxed);
        // Forgetting the path is what forces the work: the reconcile
        // below then sees it as new instead of unchanged.
        self.state.remove(&path);
        // The content index keeps its own record of the same path, and
        // the only reason to ask for a forced reindex is a change the
        // file's size and modification time do not show, such as a tag. Left
        // in place, that record makes the content pass skip the file and
        // the tag never becomes searchable.
        content::forget_content_state(&path).context("forget content state")?;
        let mut forced = std::collections::HashSet::new();
        forced.insert(path);
        let outcome = reconcile(
            active_index_writer(&mut self.writer),
            &self.f,
            &mut self.watches,
            &mut self.state,
            &self.path_scope,
            forced,
        )?;
        if outcome.index_changed {
            active_index_writer(&mut self.writer)
                .commit()
                .context("commit forced reindex")?;
            recycle_background_writer(&self.index, &mut self.writer)
                .context("recycle index writer")?;
        }
        if outcome.state_changed {
            self.state.persist()?;
        }
        self.content_backfill.request();
        self.runtime
            .activity
            .store(ACTIVITY_IDLE, Ordering::Relaxed);
        Ok(())
    }

    fn apply_control(&mut self, command: WatchControlCommand) -> Result<()> {
        if apply_control_command(
            command,
            active_index_writer(&mut self.writer),
            ContentIndexWrite {
                index: &self.content_index,
                writer: &mut self.content_writer,
            },
            &self.f,
            &mut self.state,
            self.roots,
            &self.runtime,
            &mut self.watches,
        )? {
            recycle_background_writer(&self.index, &mut self.writer)
                .context("recycle index writer")?;
            self.content_backfill.request();
            crate::throttle::release_idle_memory();
        }
        Ok(())
    }

    /// Index a burst of filesystem events that begins with `first`, then run
    /// the control command that ended the burst early, if one did.
    fn index_burst(&mut self, first: notify::Result<notify::Event>) -> Result<()> {
        self.runtime
            .activity
            .store(ACTIVITY_INDEXING, Ordering::Relaxed);
        let mut batch = WatchBatch::new();
        let mut rescan = ingest(&mut batch, first);
        let deferred_control_command = self.coalesce_burst(&mut batch, &mut rescan);
        self.apply_batch(batch, rescan)?;
        self.runtime
            .activity
            .store(ACTIVITY_IDLE, Ordering::Relaxed);
        if let Some(command) = deferred_control_command {
            match command {
                WatchControlCommand::WarmContent => self.content_backfill.warm(),
                command => self.apply_control(command)?,
            }
        }
        Ok(())
    }

    /// Coalesce a burst; a control command breaks the window and runs after the
    /// batch commits (no reordering of fs work, no lost command).
    fn coalesce_burst(
        &self,
        batch: &mut WatchBatch,
        rescan: &mut bool,
    ) -> Option<WatchControlCommand> {
        let deadline = Instant::now() + DEBOUNCE;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(remaining) {
                Ok(WatchMessage::Fs(event_result)) => {
                    self.queued.fetch_sub(1, Ordering::Relaxed);
                    *rescan |= ingest(batch, event_result);
                }
                Ok(WatchMessage::ControlCommand(command)) => return Some(command),
                Err(_) => break,
            }
        }
        None
    }

    /// Reconcile one coalesced batch, commit the name index and persist the
    /// catalogue when they changed, and hand the saves it saw to the versions
    /// worker.
    fn apply_batch(&mut self, mut batch: WatchBatch, mut rescan: bool) -> Result<()> {
        observe_origin_batch(&self.origin_writer, &batch);
        // Reconciled first, history second: only a file that reached a coherent
        // state is worth a version, and the search never waits on this.
        let history_paths = std::mem::take(&mut batch.history_paths);
        let history_renames = std::mem::take(&mut batch.history_renames);
        rescan |= self.overflowed.swap(false, Ordering::Relaxed);
        let mut outcome = reconcile(
            active_index_writer(&mut self.writer),
            &self.f,
            &mut self.watches,
            &mut self.state,
            &self.path_scope,
            batch.paths,
        )?;
        for (from, to) in history_renames {
            let _ = self
                .history_tx
                .send(crate::history::HistoryEvent::Renamed(from, to));
        }
        for path in history_paths {
            let _ = self
                .history_tx
                .send(crate::history::HistoryEvent::Changed(path));
        }
        if rescan {
            outcome.absorb(self.rescan_after_overflow()?);
        }
        if outcome.index_changed {
            active_index_writer(&mut self.writer)
                .commit()
                .context("commit incremental")?;
            recycle_background_writer(&self.index, &mut self.writer)
                .context("recycle index writer")?;
        }
        if outcome.deleted {
            self.content_backfill.note_deletion();
        }
        if outcome.any() {
            self.state.persist()?;
            self.content_backfill.request();
            crate::throttle::release_idle_memory();
        }
        Ok(())
    }

    /// inotify queue overflowed → events were dropped. Re-walk to recover.
    fn rescan_after_overflow(&mut self) -> Result<scan::ReconcileOutcome> {
        log::warn!("inotify overflow — full rescan");
        // The versions worker cannot trust what it heard either, so its pass
        // over the catalogue starts again from the top.
        self.history_pass_done = false;
        self.history_cursor = None;
        scan::reconcile_inline(
            active_index_writer(&mut self.writer),
            &self.f,
            &mut self.state,
            self.roots,
            &mut |dir| {
                self.watches.watch(dir);
            },
        )
    }
}

fn active_index_writer(writer: &mut Option<IndexWriter>) -> &mut IndexWriter {
    writer.as_mut().expect("watcher index writer is available")
}

fn recycle_background_writer(index: &Index, writer: &mut Option<IndexWriter>) -> Result<()> {
    let wait_result = close_index_writer(writer);
    let next_writer =
        crate::index::background_writer(index).context("open recycled index writer")?;
    *writer = Some(next_writer);
    wait_result
}

fn close_index_writer(writer: &mut Option<IndexWriter>) -> Result<()> {
    writer
        .take()
        .map(IndexWriter::wait_merging_threads)
        .unwrap_or(Ok(()))
        .context("wait previous index writer")
}

fn open_content_writer<'a>(
    index: &Index,
    writer: &'a mut Option<IndexWriter>,
) -> Result<&'a mut IndexWriter> {
    if writer.is_none() {
        *writer = Some(crate::index::content_writer(index).context("content index writer")?);
    }
    Ok(active_index_writer(writer))
}

/// Apply a lifecycle command. Pause/Resume flip the shared flag (Resume also
/// reconciles changes missed while paused); Rebuild wipes the index/cache and
/// re-indexes names from scratch, then schedules content backfill.
fn apply_control_command(
    command: WatchControlCommand,
    writer: &mut IndexWriter,
    content: ContentIndexWrite<'_>,
    f: &Fields,
    state: &mut State,
    roots: &[PathBuf],
    runtime: &WatchRuntime,
    watches: &mut Watches,
) -> Result<bool> {
    match command {
        // Handled by the run loop (needs the backfill state); nothing to do here.
        WatchControlCommand::WarmContent => Ok(false),
        WatchControlCommand::Pause => {
            runtime.paused.store(true, Ordering::Relaxed);
            runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);
            log::info!("indexing paused");
            Ok(false)
        }
        WatchControlCommand::Resume => {
            runtime.paused.store(false, Ordering::Relaxed);
            runtime.activity.store(ACTIVITY_INDEXING, Ordering::Relaxed);
            log::info!("indexing resumed — reconciling changes missed while paused");
            let outcome = scan::reconcile_inline(writer, f, state, roots, &mut |dir| {
                watches.watch(dir);
            })?;
            if outcome.any() {
                state.persist()?;
            }
            runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);
            Ok(outcome.any())
        }
        // Handled in the loop, where the reconcile machinery lives; it never
        // reaches this fallback.
        WatchControlCommand::ReindexPath(_) => Ok(false),
        WatchControlCommand::Rebuild => rebuild(writer, content, f, state, roots, runtime, watches),
    }
}

/// Wipe the name index, the content index and both catalogues, re-index names
/// from scratch and commit, even when the walk found nothing.
fn rebuild(
    writer: &mut IndexWriter,
    content: ContentIndexWrite<'_>,
    f: &Fields,
    state: &mut State,
    roots: &[PathBuf],
    runtime: &WatchRuntime,
    watches: &mut Watches,
) -> Result<bool> {
    runtime.activity.store(ACTIVITY_INDEXING, Ordering::Relaxed);
    log::info!("rebuild requested — clearing index and re-indexing");
    writer
        .delete_all_documents()
        .context("rebuild clear index")?;
    open_content_writer(content.index, content.writer)?
        .delete_all_documents()
        .context("rebuild clear content index")?;
    active_index_writer(content.writer)
        .commit()
        .context("commit content index rebuild clear")?;
    close_index_writer(content.writer).context("close content writer after rebuild clear")?;
    state.clear();
    content::clear_content_state().context("clear content state for rebuild")?;
    scan::reconcile_inline(writer, f, state, roots, &mut |dir| {
        watches.watch(dir);
    })?;
    // The clear above must reach disk even when the walk found nothing.
    writer.commit().context("commit rebuild")?;
    state.persist()?;
    runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);
    Ok(true)
}

/// Apply one coalesced batch. The outcome says whether the name index needs a
/// commit and whether state/content work happened.
fn reconcile(
    writer: &IndexWriter,
    f: &Fields,
    watches: &mut Watches,
    state: &mut State,
    scope: &scan::Scope,
    batch: HashSet<PathBuf>,
) -> Result<scan::ReconcileOutcome> {
    let mut outcome = scan::ReconcileOutcome::default();
    let mut offline_sources = None;
    for path in batch {
        // Catalogue keys are text: a name with invalid bytes shares its key
        // with others, and acting on it would touch theirs.
        if path.to_str().is_none() {
            continue;
        }
        let stat = std::fs::symlink_metadata(&path);
        let gone = matches!(&stat, Err(error) if error.kind() == std::io::ErrorKind::NotFound);
        let metadata = stat.ok();
        let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());
        let recorded = state.recorded(&path);
        let out_of_scope = scope.excluded(&path, is_dir)
            || (recorded.is_none() && scan::repo_ignored(&path, is_dir));
        if out_of_scope && recorded.is_none() {
            continue; // out of scope and never catalogued: nothing to do
        }
        // Below this line an excluded path is treated exactly like a vanished
        // one, so the rules also evict what an earlier version indexed before
        // they covered it. Skipping it instead would strand those documents.
        if let Some(metadata) = metadata.filter(|_| !out_of_scope) {
            let (mtime, size) = catalogue::meta_pair(&metadata);
            if recorded == Some((mtime, size)) {
                continue; // metadata-only event (e.g. ATTRIB) → nothing to reindex
            }
            if scan::name_doc_stale(recorded, mtime) {
                // A new path, or one changed on another day: (re)index its name.
                // A file rewritten the same day keeps its name doc (content
                // work is scheduled through the state change below).
                scan::upsert_path(writer, f, &path, Some(&metadata))?;
                outcome.index_changed = true;
            }
            state.set(&path, mtime, size);
            outcome.state_changed = true;
            if metadata.is_dir() {
                // New or moved-in dir: watch it before reading it, and each
                // subfolder as the walk reaches it, so an entry created during
                // the walk is either read or reported.
                watches.watch(&path);
                let subtree_outcome =
                    scan::reconcile_subtree_inline(writer, f, state, scope, &path, &mut |dir| {
                        watches.watch(dir);
                    })?;
                outcome.absorb(subtree_outcome);
            }
        } else if recorded.is_some()
            && (gone || out_of_scope)
            && evict(writer, f, state, &path, gone, &mut offline_sources)
        {
            outcome.index_changed = true;
            outcome.state_changed = true;
            outcome.deleted = true;
        }
        // Otherwise never catalogued, or not readable now (EACCES, EIO on a
        // disk going away): nothing to drop on that evidence.
    }
    Ok(outcome)
}

/// Drop a catalogued path that vanished or left the scope, with its tree.
/// `false` when it is kept: a vanished path on an unplugged source.
fn evict(
    writer: &IndexWriter,
    f: &Fields,
    state: &mut State,
    path: &Path,
    gone: bool,
    offline_sources: &mut Option<crate::query::OfflineSources>,
) -> bool {
    // An unplugged removable or network source keeps its catalogue;
    // its files are reported as offline, not deleted.
    if gone
        && offline_sources
            .get_or_insert_with(crate::query::OfflineSources::default)
            .is_retained_offline(path)
    {
        return false;
    }
    // A folder moved away, to the trash included, is one event naming
    // the folder and none for what it held: its whole tree goes.
    scan::delete_tree(writer, f, path);
    state.remove_tree(path);
    true
}

/// Offer the versions worker one page of the catalogue.
///
/// Returns whether the pass is finished. This is what covers every save the
/// watcher could not hear: the service was not running, or the kernel's queue
/// overflowed. The worker compares each file against its newest version, so a
/// document that has not changed costs one `stat` and nothing else.
fn send_history_page(
    state: &State,
    history: &Sender<crate::history::HistoryEvent>,
    cursor: &mut Option<String>,
) -> bool {
    const PAGE: usize = 256;
    let Ok(entries) = state.entries_after(cursor.as_deref(), PAGE) else {
        return true;
    };
    if entries.is_empty() {
        return true;
    }
    *cursor = entries
        .last()
        .map(|(path, _, _)| path.to_string_lossy().into_owned());
    let done = entries.len() < PAGE;
    // The catalogue is mostly not documents, and the worker would learn that one
    // path at a time, after a clone and a queue insert each.
    let documents: Vec<(PathBuf, i64, u64)> = entries
        .into_iter()
        .filter(|(path, _, _)| crate::history::may_be_a_document(path))
        .collect();
    if !documents.is_empty() {
        let _ = history.send(crate::history::HistoryEvent::Catalogued(documents));
    }
    done
}
