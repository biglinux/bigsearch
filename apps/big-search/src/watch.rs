//! Incremental index updates via inotify (notify crate), scoped to visible dirs.
//!
//! One non-recursive watch per visible directory (tens of thousands on a
//! typical home, well under the kernel `max_user_watches`): unprivileged, and
//! the hidden/gitignored churn (most changes) is never watched, so it never
//! wakes us. New visible dirs get a watch + a scoped rescan. Events are
//! coalesced over a short window, then reconciled by existence: present →
//! upsert, gone → delete it and everything under it.
mod content_backfill;
mod event_batch;
mod reconcile;
mod start;
#[cfg(test)]
mod tests;
mod watches;

use crate::catalogue::State;
use crate::content;
use crate::index::{ContentFields, Fields, content_fields, fields};
use crate::origin;
use crate::scan;
use anyhow::{Context, Result};
use content_backfill::{ContentBackfill, compact_when_worth_it, run_content_backfill_batch};
use event_batch::{Heard, MAX_FOLDERS, Rescan, WatchBatch, observe_origin_batch};
use notify::{Config, EventKindMask, RecommendedWatcher, Watcher};
use reconcile::{evict, reconcile_batch, reconcile_forced};
use start::{catch_up_at_start, watch_initial_dirs};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};
use tantivy::{Index, IndexWriter};
use watches::{Watches, epoch_secs};
pub use watches::{fingerprint, forget_saved_run, saved_run_started};

const DEBOUNCE: Duration = Duration::from_millis(250);
/// How often an idle watcher offers the versions worker another catalogue page.
/// Slow on purpose: the first-version pass must be an unnoticed trickle.
const HISTORY_PASS_TICK: Duration = Duration::from_millis(750);
pub const ACTIVITY_IDLE: u8 = 0;
pub const ACTIVITY_INDEXING: u8 = 1;
pub const ACTIVITY_BACKFILLING: u8 = 2;

/// How long after a start from the saved folder list the sources are walked
/// once, for what changed while the daemon was stopped and moved no folder: a
/// file rewritten in place, an edited `.gitignore`. The walk waits out the
/// busiest minute of a session's start.
fn saved_start_check_delay() -> Duration {
    crate::settings::test_seam("BIG_SEARCH_SAVED_START_CHECK_MS")
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(Duration::from_secs(60), Duration::from_millis)
}

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
    pub roots: &'a [PathBuf],
    pub dirs: StartDirs,
    /// The folders watched, as the last run saved them, or cleared after a walk.
    pub watch_list: State,
    /// When this start began, recorded for the next one.
    pub started: SystemTime,
    /// [`fingerprint`] of the sources and ignore rules; `None` when the rules
    /// could not be written, and then this run records nothing.
    pub fingerprint: Option<i64>,
}

/// The folders a start watches first.
pub enum StartDirs {
    /// What the startup walk found, and whether it could not read a folder
    /// for a reason other than permission.
    Walked(Vec<PathBuf>, bool),
    /// The list the last run saved, each folder with its inode then, in path
    /// order, and when that run started.
    Saved(Vec<(PathBuf, u64)>, SystemTime),
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
/// rescan/rebuild verbs). Carried on the same channel as filesystem events so the watcher
/// reacts immediately and still never wakes when there is nothing to do.
pub enum WatchControlCommand {
    Pause,
    /// Unpause and walk the sources against the catalogue.
    Resume,
    /// Walk the sources against the catalogue; refused while paused.
    Rescan,
    Rebuild,
    /// Reindex one path whose size and mtime did not change, e.g. after a tag
    /// (extended attribute) write. Requested explicitly by the tagging program,
    /// which is cheaper than reindexing on every `ATTRIB`.
    ReindexPath(std::path::PathBuf),
    /// A content search happened: extract every pending body now, bypassing the
    /// edit cooldown, so the next search sees the freshest content.
    WarmContent,
}

/// What the watcher loop consumes: a wake for what the inotify thread heard,
/// a control command or a stop.
pub enum WatchMessage {
    /// The inotify thread heard the first event since the loop last took
    /// what it heard.
    Heard,
    ControlCommand(WatchControlCommand),
    /// End [`run`], which returns this exit status, once the events heard
    /// before the stop are applied and the versions worker has noted them.
    Stop(i32),
}

struct ContentIndexWrite<'a> {
    index: &'a Index,
    writer: &'a mut Option<IndexWriter>,
}

/// Watch `roots` and keep `index` (and the `state` cache) in sync until a
/// [`WatchMessage::Stop`], and return its exit status. `paused` mirrors the runtime
/// state for the IPC `status` reply. `rx` receives what `inotify` hears and what
/// the IPC thread sends. Blocking.
///
/// # Errors
///
/// Returns an error when the index writers cannot be started, or a batch cannot
/// be committed; the caller ends the daemon.
pub fn run(
    index: Index,
    content_index: Index,
    scope: WatchScope<'_>,
    mut state: State,
    runtime: WatchRuntime,
    inotify: Inotify,
    rx: Receiver<WatchMessage>,
    history: crate::history::SharedHistory,
) -> Result<i32> {
    let f = fields(&index)?;
    let content_fields = content_fields(&content_index)?;
    // Resident for the daemon's whole life, so size it at tantivy's minimum arena
    // (15 MB) rather than the 64 MB bulk-reindex budget: incremental batches are
    // tiny (debounced fs events), and this is the dominant term in steady-state RSS.
    let mut writer = Some(crate::index::background_writer(&index).context("index writer")?);

    // Compiled once: every event is tested against the same rules the scan walk
    // uses, so the two cannot disagree about what belongs in the index.
    let path_scope = scan::Scope::compile();

    let check_sources_at = matches!(scope.dirs, StartDirs::Saved(..))
        .then(|| Instant::now() + saved_start_check_delay());
    let (history_tx, history_worker) = spawn_history_worker(&history);
    let mut watches = Watches::new(
        inotify.watcher,
        &runtime,
        scope.watch_list,
        scope.fingerprint,
    );
    let catch_up = watch_initial_dirs(&mut watches, scope.roots, scope.dirs, scope.started);
    catch_up_at_start(
        &mut writer,
        &f,
        &mut watches,
        &mut state,
        &path_scope,
        catch_up,
    )?;
    watches.record_run(scope.started)?;
    let origin_writer = open_origin_writer();

    let mut content_backfill = ContentBackfill::new(runtime.pending_content.clone());
    content_backfill.request();
    runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);

    let status = WatchLoop {
        check_sources_at,
        content_backfill,
        stop_status: None,
        history_pass_done: false,
        history_cursor: None,
        history_tx,
        origin_writer,
        watches,
        heard: inotify.heard,
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
    .run_until_stopped()?;
    // The loop took the worker's queue with it: the worker notes each change
    // still queued, then returns.
    if let Some(worker) = history_worker
        && worker.join().is_err()
    {
        log::error!("the versions worker panicked while stopping");
    }
    Ok(status)
}

/// The file watcher, with no watch yet, and what its thread hears for [`run`].
pub struct Inotify {
    watcher: RecommendedWatcher,
    heard: Arc<Heard>,
}

/// Start inotify for the sources `roots`; the first event heard after the
/// watcher loop takes a batch wakes it through `wake`. notify's reader thread takes its caller's policy, nice
/// value and cgroup: the daemon calls this through
/// [`crate::throttle::spawn_lowered`].
///
/// # Errors
///
/// Returns an error when inotify cannot be initialized.
pub fn start_inotify(wake: Sender<WatchMessage>, roots: Vec<PathBuf>) -> Result<Inotify> {
    let heard = Arc::new(Heard::new(roots, wake));
    let hearing = Arc::clone(&heard);
    // Without IN_OPEN, which notify asks for by default: every read under a
    // source, the indexer's own included, would cost the reader an event.
    // A close after writing is how a save ends.
    let config =
        Config::default().with_event_kinds(EventKindMask::CORE | EventKindMask::ACCESS_CLOSE);
    let watcher =
        RecommendedWatcher::new(move |watcher_result| hearing.hear(watcher_result), config)
            .context("inotify init")?;
    Ok(Inotify { watcher, heard })
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
///
/// Called before any watch: what changed while the service was stopped is
/// settled here before a change is heard, and the start's walk catches the
/// rest. The worker returns once the sender is dropped and the queue drained.
fn spawn_history_worker(
    history: &crate::history::SharedHistory,
) -> (
    Sender<crate::history::HistoryEvent>,
    Option<std::thread::JoinHandle<()>>,
) {
    if let Err(error) = crate::history::check_kept_paths(history) {
        log::warn!("could not check the kept documents: {error:#}");
    }
    let (history_tx, history_rx) = std::sync::mpsc::channel::<crate::history::HistoryEvent>();
    let history_store = history.clone();
    let spawned = std::thread::Builder::new()
        .name("history".into())
        .spawn(move || crate::history::run_worker(&history_store, history_rx));
    let worker = match spawned {
        Ok(worker) => Some(worker),
        Err(e) => {
            log::error!("no versions are kept: the history thread did not start: {e}");
            None
        }
    };
    (history_tx, worker)
}

/// What the watcher loop owns between messages.
struct WatchLoop<'a> {
    /// When a start from the saved list walks the sources once
    /// ([`saved_start_check_delay`]).
    check_sources_at: Option<Instant>,
    content_backfill: ContentBackfill,
    /// The exit status of the [`WatchMessage::Stop`] taken, which ends the loop.
    stop_status: Option<i32>,
    history_pass_done: bool,
    /// Where the pass over the catalogue has got to. It covers what no event
    /// could: the bursts that overflowed the kernel's queue, and saves made
    /// while the service was not running that a walk or the start's catch-up
    /// recorded in the catalogue.
    history_cursor: Option<String>,
    history_tx: Sender<crate::history::HistoryEvent>,
    origin_writer: origin::OriginWriter,
    watches: Watches,
    heard: Arc<Heard>,
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
    fn run_until_stopped(mut self) -> Result<i32> {
        loop {
            let message = self.next_message()?;
            self.handle(message)?;
            if let Some(status) = self.stop_status {
                return Ok(status);
            }
        }
    }

    /// Wait for the next message, doing idle work (a content backfill batch,
    /// a page of the versions pass) each time the wait runs out, and the walk
    /// a start from the saved list owes once its time comes.
    fn next_message(&mut self) -> Result<WatchMessage> {
        loop {
            if self.check_due() {
                self.check_sources_at = None;
                // A paused watcher drops events; `resume` walks.
                if !self.runtime.paused.load(Ordering::Relaxed) {
                    log::info!("checking the sources for changes made while stopped");
                    self.walk_sources()?;
                }
            }
            let backfill = self
                .content_backfill
                .should_run_when_idle(&self.runtime.paused);
            // The pass over the catalogue also runs when there is no indexing
            // left to do, which is most of the time: blocking on the channel
            // would make the first version of anything wait for the next file
            // to change. Paused stops it too; Resume picks it up.
            let history_page = !backfill
                && !self.history_pass_done
                && !self.runtime.paused.load(Ordering::Relaxed);
            let idle_wait = if backfill {
                Some(self.content_backfill.wait_before_batch())
            } else if history_page {
                Some(HISTORY_PASS_TICK)
            } else {
                None
            };
            let check_wait = self
                .check_sources_at
                .map(|at| at.saturating_duration_since(Instant::now()));
            let received = match idle_wait.into_iter().chain(check_wait).min() {
                Some(wait) => self.rx.recv_timeout(wait),
                None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(message) => return Ok(message),
                Err(RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("every sender of the watcher's messages is gone")
                }
                // The wait ran out for the walk, not for idle work.
                Err(RecvTimeoutError::Timeout) if self.check_due() => {}
                Err(RecvTimeoutError::Timeout) if backfill => self.idle_backfill_turn(),
                Err(RecvTimeoutError::Timeout) if history_page => {
                    self.history_pass_done =
                        send_history_page(&self.state, &self.history_tx, &mut self.history_cursor);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn check_due(&self) -> bool {
        self.check_sources_at.is_some_and(|at| at <= Instant::now())
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
        match message {
            WatchMessage::ControlCommand(WatchControlCommand::WarmContent) => {
                self.content_backfill.warm();
                return Ok(());
            }
            WatchMessage::ControlCommand(WatchControlCommand::ReindexPath(path)) => {
                return self.reindex_path(path);
            }
            WatchMessage::ControlCommand(WatchControlCommand::Resume) => {
                // The flag was cleared when the command was accepted; set
                // again, a pause sent after this resume is waiting in the queue.
                if self.runtime.paused.load(Ordering::Relaxed) {
                    return Ok(());
                }
                log::info!("indexing resumed: reconciling the sources with the catalogue");
                return self.walk_sources();
            }
            WatchMessage::ControlCommand(WatchControlCommand::Rescan) => {
                // The socket refuses it while paused; this guards the queue. A
                // paused watcher drops events, so no walk may record its run
                // as covered: `resume` walks.
                if self.runtime.paused.load(Ordering::Relaxed) {
                    log::warn!("rescan refused: indexing is paused");
                    return Ok(());
                }
                log::info!("rescan requested: reconciling the sources with the catalogue");
                return self.walk_sources();
            }
            WatchMessage::ControlCommand(command) => return self.apply_control(command),
            WatchMessage::Stop(status) => {
                self.stop_status = Some(status);
                return self.apply_last_burst();
            }
            WatchMessage::Heard => {}
        }
        if self.runtime.paused.load(Ordering::Relaxed) {
            // Resume reconciles what the index missed; saves and renames still
            // reach the versions worker, or the next start parks the rows of
            // documents saved while paused.
            if let Some(batch) = self.heard.take() {
                self.send_history(batch.history_renames, batch.history_paths);
            }
            return Ok(());
        }
        self.index_burst()
    }

    /// Apply what the inotify thread hears within one more [`DEBOUNCE`]
    /// after a stop, so a save made just before it still reaches the versions
    /// worker. A request or another stop that arrives meanwhile is dropped.
    fn apply_last_burst(&mut self) -> Result<()> {
        let deadline = Instant::now() + DEBOUNCE;
        while self.wait_out_burst(deadline).is_some() {}
        let Some(batch) = self.heard.take() else {
            return Ok(());
        };
        if self.runtime.paused.load(Ordering::Relaxed) {
            self.send_history(batch.history_renames, batch.history_paths);
            return Ok(());
        }
        self.apply_batch(batch)
    }

    fn reindex_path(&mut self, path: PathBuf) -> Result<()> {
        // The socket refuses these; this guards against the sources, or a
        // folder on the way, having changed since.
        let key = crate::settings::sources()
            .resolved_client_path(&path)
            .filter(|key| self.roots.iter().any(|root| key.starts_with(root)));
        if self.runtime.paused.load(Ordering::Relaxed) {
            log::warn!("reindex of {} refused: indexing is paused", path.display());
            return Ok(());
        }
        let Some(path) = key else {
            log::warn!("reindex of {} refused: outside the roots", path.display());
            return Ok(());
        };
        self.runtime
            .activity
            .store(ACTIVITY_INDEXING, Ordering::Relaxed);
        // The content index keeps its own record of the same path, and
        // the only reason to ask for a forced reindex is a change the
        // file's size and modification time do not show, such as a tag. Left
        // as it is, that record makes the content pass skip the file and
        // the tag never becomes searchable.
        content::mark_content_stale(&path).context("mark content stale")?;
        let outcome = reconcile_forced(
            active_index_writer(&mut self.writer),
            &self.f,
            &mut self.watches,
            &mut self.state,
            &self.path_scope,
            &path,
        )?;
        if outcome.index_changed {
            active_index_writer(&mut self.writer)
                .commit()
                .context("commit forced reindex")?;
            recycle_background_writer(&self.index, &mut self.writer)
                .context("recycle index writer")?;
        }
        if outcome.deleted {
            self.content_backfill.note_deletion();
        }
        self.watches.persist()?;
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

    /// Index what the inotify thread hears within one [`DEBOUNCE`], then run
    /// the control command that ended the burst early, if one did.
    fn index_burst(&mut self) -> Result<()> {
        self.runtime
            .activity
            .store(ACTIVITY_INDEXING, Ordering::Relaxed);
        let deferred = self.wait_out_burst(Instant::now() + DEBOUNCE);
        if let Some(batch) = self.heard.take() {
            self.apply_batch(batch)?;
        }
        self.runtime
            .activity
            .store(ACTIVITY_IDLE, Ordering::Relaxed);
        if let Some(message) = deferred {
            self.handle(message)?;
        }
        Ok(())
    }

    /// Let a burst gather until `deadline`. A control command or a stop ends
    /// it early and is returned, for the caller to run after the batch commits.
    fn wait_out_burst(&self, deadline: Instant) -> Option<WatchMessage> {
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(remaining) {
                Ok(WatchMessage::Heard) => {}
                Ok(message) => return Some(message),
                Err(_) => break,
            }
        }
        None
    }

    /// Reconcile one batch, commit the name index and persist the catalogue
    /// when they changed, and hand the saves it saw to the versions worker.
    fn apply_batch(&mut self, mut batch: WatchBatch) -> Result<()> {
        observe_origin_batch(&self.origin_writer, &batch);
        // Reconciled first, history second: only a file that reached a coherent
        // state is worth a version, and the search never waits on this.
        let history_paths = std::mem::take(&mut batch.history_paths);
        let history_renames = std::mem::take(&mut batch.history_renames);
        let mut outcome = reconcile_batch(
            active_index_writer(&mut self.writer),
            &self.f,
            &mut self.watches,
            &mut self.state,
            &self.path_scope,
            batch.paths,
            None,
        )?;
        for folder in &batch.folders {
            outcome.absorb(reconcile_forced(
                active_index_writer(&mut self.writer),
                &self.f,
                &mut self.watches,
                &mut self.state,
                &self.path_scope,
                folder,
            )?);
        }
        self.send_history(history_renames, history_paths);
        let walk_started = if let Some(cause) = batch.rescan {
            match cause {
                Rescan::Overflowed => log::warn!("inotify overflow: full rescan"),
                Rescan::Collapsed => {
                    log::info!("changes in over {MAX_FOLDERS} folders at once: full rescan");
                }
            }
            Some(self.rescan_sources(&mut outcome)?)
        } else {
            None
        };
        self.finish_batch(outcome, walk_started)
    }

    fn send_history(
        &self,
        renames: Vec<(PathBuf, PathBuf)>,
        saves: std::collections::HashSet<PathBuf>,
    ) {
        for (from, to) in renames {
            let _ = self
                .history_tx
                .send(crate::history::HistoryEvent::Renamed(from, to));
        }
        for path in saves {
            let _ = self
                .history_tx
                .send(crate::history::HistoryEvent::Changed(path));
        }
    }

    /// Walk every source against the catalogue on request, as one batch.
    fn walk_sources(&mut self) -> Result<()> {
        self.runtime
            .activity
            .store(ACTIVITY_INDEXING, Ordering::Relaxed);
        let mut outcome = scan::ReconcileOutcome::default();
        let started = self.rescan_sources(&mut outcome)?;
        self.finish_batch(outcome, Some(started))?;
        self.runtime
            .activity
            .store(ACTIVITY_IDLE, Ordering::Relaxed);
        Ok(())
    }

    /// Walk every source against the catalogue into `outcome`: after events
    /// were dropped, or on request. Returns when the walk began.
    fn rescan_sources(&mut self, outcome: &mut scan::ReconcileOutcome) -> Result<SystemTime> {
        let started = SystemTime::now();
        self.watches.walk_begins()?;
        // This walk is the one a start from the saved list owes.
        self.check_sources_at = None;
        // The versions worker cannot trust what it heard either, so its pass
        // over the catalogue starts again from the top.
        self.history_pass_done = false;
        self.history_cursor = None;
        let roots = self.roots;
        let walked = self
            .watches
            .relist(roots.iter().map(PathBuf::as_path), |watches| {
                scan::reconcile_inline(
                    active_index_writer(&mut self.writer),
                    &self.f,
                    &mut self.state,
                    roots,
                    &mut |dir| {
                        watches.watch(dir);
                    },
                )
            })?;
        // A folder the walk could not read leaves this run unable to vouch
        // for it.
        self.watches.missed |= walked.unread;
        outcome.absorb(walked);
        Ok(started)
    }

    /// Commit the name index and persist the catalogue when `outcome` changed
    /// them, and after a walk of the sources that began at `walk_started`,
    /// record the run for the next start.
    fn finish_batch(
        &mut self,
        outcome: scan::ReconcileOutcome,
        walk_started: Option<SystemTime>,
    ) -> Result<()> {
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
        self.watches.persist()?;
        if outcome.any() {
            self.state.persist()?;
            self.content_backfill.request();
            crate::throttle::release_idle_memory();
        }
        if let Some(started) = walk_started {
            self.watches.record_run(started)?;
        }
        Ok(())
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

/// Apply a lifecycle command the run loop leaves to this function: Pause
/// forgets the run record; Rebuild wipes the index/cache and re-indexes names
/// from scratch, then schedules content backfill. Whether the index changed.
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
        WatchControlCommand::Pause => {
            // The flag was set when the command was accepted; cleared again,
            // a resume accepted after this pause is waiting in the queue.
            if !runtime.paused.load(Ordering::Relaxed) {
                return Ok(false);
            }
            // Events are dropped while paused, so the next start must walk.
            watches.forget_run()?;
            runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);
            log::info!("indexing paused");
            Ok(false)
        }
        // Handled in the loop, where the reconcile and backfill state live; they
        // never reach this fallback.
        WatchControlCommand::WarmContent
        | WatchControlCommand::Resume
        | WatchControlCommand::Rescan
        | WatchControlCommand::ReindexPath(_) => Ok(false),
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
    let started = SystemTime::now();
    watches.walk_begins()?;
    runtime.activity.store(ACTIVITY_INDEXING, Ordering::Relaxed);
    log::info!("rebuild requested: clearing index and re-indexing");
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
    let walked = watches.relist(roots.iter().map(PathBuf::as_path), |watches| {
        scan::reconcile_inline(writer, f, state, roots, &mut |dir| {
            watches.watch(dir);
        })
    })?;
    watches.missed |= walked.unread;
    // The clear above must reach disk even when the walk found nothing.
    writer.commit().context("commit rebuild")?;
    watches.persist()?;
    state.persist()?;
    watches.record_run(started)?;
    runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);
    Ok(true)
}

/// Offer the versions worker one page of the catalogue.
///
/// Returns whether the pass is finished. This is what covers the saves the
/// watcher could not hear: the kernel's queue overflowed, or the service was
/// not running and a walk or the start's catch-up recorded the change in the
/// catalogue. The worker compares each catalogue entry against the newest
/// version, so a document that has not changed costs one lookup.
fn send_history_page(
    state: &State,
    history: &Sender<crate::history::HistoryEvent>,
    cursor: &mut Option<String>,
) -> bool {
    const PAGE: usize = 256;
    let entries = match state.entries_after(cursor.as_deref(), PAGE) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!("the versions pass stopped: cannot read the catalogue: {error:#}");
            return true;
        }
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
