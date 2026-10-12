//! Incremental index updates via inotify (notify crate), scoped to visible dirs.
//!
//! One non-recursive watch per visible directory (tens of thousands on a
//! typical home, well under the kernel `max_user_watches`): unprivileged, and
//! the hidden/gitignored churn (most changes) is never watched, so it never
//! wakes us. New visible dirs get a watch + a scoped rescan. Events are
//! coalesced over a short window, then reconciled by existence: present →
//! upsert, gone → delete.
use crate::content;
use crate::index::{ContentFields, Fields, content_fields, fields};
use crate::origin;
use crate::scan;
use crate::state::{self, State};
use anyhow::{Context, Result};
use notify::event::{AccessKind, AccessMode, CreateKind, EventKind, ModifyKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
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
/// Longest wait after content backfill batches keep failing (a full disk, a
/// damaged index): the daemon must not wake every few seconds to fail again.
const MAX_BACKFILL_RETRY: Duration = Duration::from_secs(600);
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

struct WatchBatch {
    paths: HashSet<PathBuf>,
    origin_paths: Vec<PathBuf>,
    origin_overflow: bool,
    /// Files that may have just been saved, for the versions worker. Bounded,
    /// because an unpacked archive must not grow a list the size of itself on a
    /// machine with little memory.
    history_paths: HashSet<PathBuf>,
    /// Renames, kept as the pair the kernel reported. The destination alone is
    /// not enough: the save that usually follows replaces the inode, and then
    /// nothing connects the document to its own past.
    history_renames: Vec<(PathBuf, PathBuf)>,
}

struct ContentBackfill {
    batch_limit: usize,
    idle_delay: Duration,
    is_pending: bool,
    /// Keyset position inside the current pass over the catalogue (`None` at
    /// pass boundaries). Batches resume here instead of rescanning from the start.
    cursor: content::CatalogCursor,
    /// A change arrived while a pass was already running: run one more full
    /// pass after the current one ends, so nothing behind the cursor is missed.
    rescan_requested: bool,
    /// Extract even files still inside the edit cooldown (a content search
    /// wants them now). Reset when a pass completes with nothing deferred.
    bypass_cooldown: bool,
    /// All remaining pending files are inside their cooldown: sleep until the
    /// earliest one becomes eligible instead of polling every idle tick.
    snooze_until: Option<Instant>,
    /// When the next batch is due: in a running pass, set from how long the
    /// last batch took and how busy the machine is; before one, by the first
    /// content search (see [`Self::warm`]). A deadline, not a delay: file
    /// events and searches that arrive meanwhile must not keep pushing it back.
    next_batch_at: Option<Instant>,
    /// Shared gauge read by the IPC thread: files currently deferred by the
    /// cooldown (the "content results may be incomplete" hint).
    deferred_gauge: Arc<AtomicU64>,
    /// A catalogue row was removed since the last orphan prune, so there may be
    /// content documents left behind. Nothing else can create an orphan.
    deletion_seen: bool,
    /// Batches that failed in a row; each one doubles the wait before the next.
    failures: u32,
}

struct ContentIndexWrite<'a> {
    index: &'a Index,
    writer: &'a mut Option<IndexWriter>,
}

impl ContentBackfill {
    fn new(deferred_gauge: Arc<AtomicU64>) -> Self {
        Self {
            batch_limit: content::daemon_batch_limit(),
            idle_delay: content::daemon_idle_delay(),
            is_pending: false,
            cursor: None,
            rescan_requested: false,
            bypass_cooldown: false,
            snooze_until: None,
            next_batch_at: None,
            deferred_gauge,
            // The first pass after a start prunes unconditionally: deletions may
            // have happened while the daemon was down.
            deletion_seen: true,
            failures: 0,
        }
    }

    /// A batch failed: try again later, waiting longer after each failure in a
    /// row. A file change in between asks again at the normal cadence.
    fn retry_after_failure(&mut self) {
        self.request();
        self.failures = self.failures.saturating_add(1);
        let wait = self
            .idle_delay
            .saturating_mul(1 << self.failures.min(10))
            .min(MAX_BACKFILL_RETRY);
        self.snooze_until = Some(Instant::now() + wait);
    }

    fn note_deletion(&mut self) {
        self.deletion_seen = true;
    }

    /// Whether to prune now, clearing the flag.
    fn take_deletion_seen(&mut self) -> bool {
        std::mem::replace(&mut self.deletion_seen, false)
    }

    fn request(&mut self) {
        if self.is_pending {
            self.rescan_requested = true;
        } else {
            self.is_pending = true;
        }
        // New work may be older than the deferred set: wake at the normal cadence.
        self.snooze_until = None;
    }

    /// A content search happened: run a cooldown-free pass once the idle delay
    /// has passed since the first search. A deadline: searches typed faster
    /// than the delay must not keep pushing the pass back.
    fn warm(&mut self) {
        self.bypass_cooldown = true;
        self.request();
        self.next_batch_at
            .get_or_insert_with(|| Instant::now() + self.idle_delay);
    }

    fn clear(&mut self) {
        self.is_pending = false;
    }

    /// End-of-pass bookkeeping: `true` when the backfill is fully done, `false`
    /// when a change arrived mid-pass and one more pass must run.
    fn finish_pass(&mut self) -> bool {
        self.cursor = None;
        self.bypass_cooldown = false;
        if self.rescan_requested {
            self.rescan_requested = false;
            return false;
        }
        self.clear();
        true
    }

    /// End of a pass that deferred cooldown-protected files: stay pending and
    /// sleep until the earliest one becomes eligible.
    fn finish_pass_deferred(&mut self, retry_in: Duration) {
        self.cursor = None;
        self.bypass_cooldown = false;
        if self.rescan_requested {
            self.rescan_requested = false;
            return; // fresh work exists; the next idle tick runs at normal cadence
        }
        self.snooze_until = Some(Instant::now() + retry_in.max(self.idle_delay));
    }

    fn should_run_when_idle(&self, paused: &AtomicBool) -> bool {
        self.is_pending && !paused.load(Ordering::Relaxed)
    }

    /// How long the watcher may sleep before the next backfill attempt.
    fn wait_before_batch(&self) -> Duration {
        match self.snooze_until {
            Some(deadline) => deadline
                .saturating_duration_since(Instant::now())
                .max(self.idle_delay),
            None => self.next_batch_at.map_or(self.idle_delay, |at| {
                at.saturating_duration_since(Instant::now())
            }),
        }
    }
}

impl WatchBatch {
    fn new() -> Self {
        Self {
            paths: HashSet::new(),
            origin_paths: Vec::new(),
            origin_overflow: false,
            history_paths: HashSet::new(),
            history_renames: Vec::new(),
        }
    }

    fn add_origin_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if self.origin_overflow || !origin::settings().watch_enabled {
            return;
        }
        for path in paths {
            if self.origin_paths.len() >= origin::settings().max_batch {
                self.origin_paths.clear();
                self.origin_overflow = true;
                return;
            }
            self.origin_paths.push(path);
        }
    }

    /// Most files one batch offers the versions worker.
    ///
    /// Unpacking an archive of ten thousand documents is one batch; the worker
    /// would get to them through the catalogue pass anyway, and a list that size
    /// is memory a small machine does not have to spare.
    const HISTORY_LIMIT: usize = 512;

    /// Note what may have been saved, and the renames as pairs.
    fn add_history(&mut self, kind: EventKind, paths: &[PathBuf]) {
        if let EventKind::Modify(ModifyKind::Name(RenameMode::Both)) = kind
            && let (Some(from), Some(to)) = (paths.first(), paths.get(1))
        {
            self.history_renames.push((from.clone(), to.clone()));
            self.remember_history_path(to.clone());
            return;
        }
        // A close after writing is the strongest signal a save finished. Create
        // and rename-to cover the file that arrives already written (from a
        // download, from another folder) and never sees a close at all.
        let worth_a_version = matches!(
            kind,
            EventKind::Access(notify::event::AccessKind::Close(
                notify::event::AccessMode::Write
            )) | EventKind::Create(_)
                | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Any))
        );
        if !worth_a_version {
            return;
        }
        for path in paths {
            self.remember_history_path(path.clone());
        }
    }

    fn remember_history_path(&mut self, path: PathBuf) {
        // Asked here rather than in the worker: on an ordinary home folder most
        // of what changes is not a document, and each one that is not costs a
        // clone, a channel send and a place in the worker's queue to find out.
        if self.history_paths.len() < Self::HISTORY_LIMIT
            && crate::history::may_be_a_document(&path)
        {
            self.history_paths.insert(path);
        }
    }
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
    let mut content_writer = None;

    // Compiled once: every event is tested against the same rules the scan walk
    // uses, so the two cannot disagree about what belongs in the index.
    let path_scope = scan::Scope::compile();

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
    let mut watches = Watches {
        watcher,
        failed: Arc::clone(&runtime.unwatched_dirs),
    };

    let mut watched = 0usize;
    let initial_dirs = if scope.initial_dirs.is_empty() {
        scope
            .roots
            .iter()
            .flat_map(|root| scan::visible_dirs(root))
            .collect()
    } else {
        scope.initial_dirs
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
    // What changed between the startup walk reaching a folder and its watch:
    // a folder whose entries changed since the walk began is compared again,
    // its entries on disk and in the catalogue. Only its direct entries; a new
    // folder among them is walked whole, a vanished one leaves with its tree.
    if let Some(walk_started) = scope.walk_started {
        let since = walk_started - Duration::from_secs(1);
        let mut missed = HashSet::new();
        for dir in &initial_dirs {
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
                active_index_writer(&mut writer),
                &f,
                &mut watches,
                &mut state,
                &path_scope,
                missed,
            )?;
            if outcome.index_changed {
                active_index_writer(&mut writer)
                    .commit()
                    .context("commit startup catch-up")?;
            }
            if outcome.any() {
                state.persist()?;
            }
        }
    }
    drop(initial_dirs);
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
    // Earlier versions of documents live in their own thread, fed by this one.
    // Separate on purpose: a version that cannot be written must never delay the
    // search, and the store it writes to is the person's data rather than a
    // cache this service can rebuild.
    let (history_tx, history_rx) = std::sync::mpsc::channel::<crate::history::HistoryEvent>();
    let history_store = std::sync::Arc::clone(&history);
    std::thread::Builder::new()
        .name("history".into())
        .spawn(move || crate::history::run_worker(&history_store, history_rx))
        .ok();
    // Where the pass over the catalogue has got to. It covers what no event
    // could: the moments the service was not running, and the bursts that
    // overflowed the kernel's queue.
    let mut history_cursor: Option<String> = None;
    let mut history_pass_done = false;

    let mut content_backfill = ContentBackfill::new(runtime.pending_content.clone());
    content_backfill.request();
    runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);

    loop {
        let watch_message = if content_backfill.should_run_when_idle(&runtime.paused) {
            match rx.recv_timeout(content_backfill.wait_before_batch()) {
                Ok(message) => message,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    content_backfill.snooze_until = None; // slept through the cooldown
                    content_backfill.next_batch_at = None;
                    // One page of the catalogue per idle turn, and only while
                    // the machine has nothing better to do.
                    if !history_pass_done {
                        history_pass_done =
                            send_history_page(&state, &history_tx, &mut history_cursor);
                    }
                    runtime
                        .activity
                        .store(ACTIVITY_BACKFILLING, Ordering::Relaxed);
                    match run_content_backfill_batch(
                        &content_index,
                        &mut content_writer,
                        &content_fields,
                        &mut content_backfill,
                    ) {
                        // Nothing left to read: the time to tidy the indexes, unless
                        // the charge is someone else's.
                        Ok(true) if !crate::throttle::on_battery() => {
                            content_backfill.failures = 0;
                            compact_when_worth_it(&index, active_index_writer(&mut writer), "name");
                            match open_content_writer(&content_index, &mut content_writer) {
                                Ok(content) => {
                                    compact_when_worth_it(&content_index, content, "content")
                                }
                                Err(e) => log::warn!("content index not compacted: {e:#}"),
                            }
                        }
                        Ok(_) => content_backfill.failures = 0,
                        Err(e) => {
                            log::error!("content backfill failed: {e:#}");
                            // A writer that failed mid-batch is not trusted for the next one.
                            let _ = close_index_writer(&mut content_writer);
                            content_backfill.retry_after_failure();
                        }
                    }
                    runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else if history_pass_done || runtime.paused.load(Ordering::Relaxed) {
            // Paused stops the versions pass too; Resume picks it up.
            match rx.recv() {
                Ok(message) => message,
                Err(_) => break,
            }
        } else {
            // The pass over the catalogue also runs when there is no indexing
            // left to do, which is most of the time: blocking on the channel
            // would make the first version of anything wait for the next file
            // to change.
            match rx.recv_timeout(HISTORY_PASS_TICK) {
                Ok(message) => message,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    history_pass_done = send_history_page(&state, &history_tx, &mut history_cursor);
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        let first = match watch_message {
            WatchMessage::ControlCommand(WatchControlCommand::WarmContent) => {
                content_backfill.warm();
                continue;
            }
            WatchMessage::ControlCommand(WatchControlCommand::ReindexPath(path)) => {
                // ipc.rs refuses these with an answer; this is the backstop for
                // the sources having changed since.
                if runtime.paused.load(Ordering::Relaxed)
                    || !path.is_absolute()
                    || !scope.roots.iter().any(|root| path.starts_with(root))
                {
                    log::warn!("reindex of {} refused: outside the roots", path.display());
                    continue;
                }
                runtime.activity.store(ACTIVITY_INDEXING, Ordering::Relaxed);
                // Forgetting the path is what forces the work: the reconcile
                // below then sees it as new instead of unchanged.
                state.remove(&path);
                // The content index keeps its own record of the same path, and
                // the only reason to ask for a forced reindex is a change the
                // file's size and modification time do not show, such as a tag. Left
                // in place, that record makes the content pass skip the file and
                // the tag never becomes searchable.
                content::forget_content_state(&path).context("forget content state")?;
                let mut forced = std::collections::HashSet::new();
                forced.insert(path);
                let outcome = reconcile(
                    active_index_writer(&mut writer),
                    &f,
                    &mut watches,
                    &mut state,
                    &path_scope,
                    forced,
                )?;
                if outcome.index_changed {
                    active_index_writer(&mut writer)
                        .commit()
                        .context("commit forced reindex")?;
                    recycle_background_writer(&index, &mut writer)
                        .context("recycle index writer")?;
                }
                if outcome.state_changed {
                    state.persist()?;
                }
                content_backfill.request();
                runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);
                continue;
            }
            WatchMessage::ControlCommand(command) => {
                if apply_control_command(
                    command,
                    active_index_writer(&mut writer),
                    ContentIndexWrite {
                        index: &content_index,
                        writer: &mut content_writer,
                    },
                    &f,
                    &mut state,
                    scope.roots,
                    &runtime,
                    &mut watches,
                )? {
                    recycle_background_writer(&index, &mut writer)
                        .context("recycle index writer")?;
                    content_backfill.request();
                    crate::throttle::release_idle_memory();
                }
                continue;
            }
            WatchMessage::Fs(watcher_result) => {
                queued.fetch_sub(1, Ordering::Relaxed);
                watcher_result
            }
        };
        if runtime.paused.load(Ordering::Relaxed) {
            continue; // dropped while paused; Resume reconciles what was missed
        }

        runtime.activity.store(ACTIVITY_INDEXING, Ordering::Relaxed);
        let mut batch = WatchBatch::new();
        let mut rescan = ingest(&mut batch, first);
        // Coalesce a burst; a control command breaks the window and runs after the
        // batch commits (no reordering of fs work, no lost command).
        let mut deferred_control_command: Option<WatchControlCommand> = None;
        let deadline = Instant::now() + DEBOUNCE;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(remaining) {
                Ok(WatchMessage::Fs(event_result)) => {
                    queued.fetch_sub(1, Ordering::Relaxed);
                    rescan |= ingest(&mut batch, event_result);
                }
                Ok(WatchMessage::ControlCommand(command)) => {
                    deferred_control_command = Some(command);
                    break;
                }
                Err(_) => break,
            }
        }

        observe_origin_batch(&origin_writer, &batch);
        // Reconciled first, history second: only a file that reached a coherent
        // state is worth a version, and the search never waits on this.
        let history_paths = std::mem::take(&mut batch.history_paths);
        let history_renames = std::mem::take(&mut batch.history_renames);
        rescan |= overflowed.swap(false, Ordering::Relaxed);
        let mut outcome = reconcile(
            active_index_writer(&mut writer),
            &f,
            &mut watches,
            &mut state,
            &path_scope,
            batch.paths,
        )?;
        for (from, to) in history_renames {
            let _ = history_tx.send(crate::history::HistoryEvent::Renamed(from, to));
        }
        for path in history_paths {
            let _ = history_tx.send(crate::history::HistoryEvent::Changed(path));
        }
        if rescan {
            // inotify queue overflowed → events were dropped. Re-walk to recover.
            log::warn!("inotify overflow — full rescan");
            // The versions worker cannot trust what it heard either, so its pass
            // over the catalogue starts again from the top.
            history_pass_done = false;
            history_cursor = None;
            outcome.absorb(scan::reconcile_inline(
                active_index_writer(&mut writer),
                &f,
                &mut state,
                scope.roots,
                &mut |dir| {
                    watches.watch(dir);
                },
            )?);
        }
        if outcome.index_changed {
            active_index_writer(&mut writer)
                .commit()
                .context("commit incremental")?;
            recycle_background_writer(&index, &mut writer).context("recycle index writer")?;
        }
        if outcome.deleted {
            content_backfill.note_deletion();
        }
        if outcome.any() {
            state.persist()?;
            content_backfill.request();
            crate::throttle::release_idle_memory();
        }
        runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);
        if let Some(command) = deferred_control_command {
            match command {
                WatchControlCommand::WarmContent => content_backfill.warm(),
                command => {
                    if apply_control_command(
                        command,
                        active_index_writer(&mut writer),
                        ContentIndexWrite {
                            index: &content_index,
                            writer: &mut content_writer,
                        },
                        &f,
                        &mut state,
                        scope.roots,
                        &runtime,
                        &mut watches,
                    )? {
                        recycle_background_writer(&index, &mut writer)
                            .context("recycle index writer")?;
                        content_backfill.request();
                        crate::throttle::release_idle_memory();
                    }
                }
            }
        }
    }
    Ok(())
}

/// One batch of the content backfill; `true` when it finished a pass with
/// nothing left to read.
fn run_content_backfill_batch(
    content_index: &Index,
    writer: &mut Option<IndexWriter>,
    f: &ContentFields,
    content_backfill: &mut ContentBackfill,
) -> Result<bool> {
    let started = Instant::now();
    crate::throttle::begin_cpu_window();
    // Only when something was actually deleted. Proving "no orphans" still costs
    // a full scan of the content namespace, and at the start of every pass it
    // would be paid after every burst of file changes.
    if content_backfill.cursor.is_none() && content_backfill.take_deletion_seen() {
        close_index_writer(writer).context("close content writer before prune")?;
        match content::prune_orphaned_content(content_index, content::ORPHAN_PRUNE_STEADY_LIMIT) {
            Ok((0, _)) => {}
            Ok((removed, more)) => {
                log::info!(
                    "content prune: dropped {removed} document(s) for deleted files{}",
                    if more { ", more remain" } else { "" }
                );
                // The prune runs at a pass's start only: ask for another pass,
                // or a large deleted tree would leave orphans until a restart.
                if more {
                    content_backfill.note_deletion();
                    content_backfill.request();
                }
            }
            Err(e) => log::warn!("content prune failed: {e:#}"),
        }
    }

    let cooldown = if content_backfill.bypass_cooldown {
        Duration::ZERO
    } else {
        crate::settings::content_cooldown()
    };
    let (batch, next_cursor) = content::pending_catalog_batch_after(
        content_backfill.cursor.take(),
        content_backfill.batch_limit,
        cooldown,
    )?;
    content_backfill.cursor = next_cursor;
    content_backfill
        .deferred_gauge
        .store(batch.deferred as u64, Ordering::Relaxed);
    content::record_skipped_candidates(&batch.non_candidates)
        .context("record policy-skipped entries")?;

    let processed = if batch.files.is_empty() {
        None
    } else {
        let attempted = batch.files.len();
        let indexed = content::index_content_limited_with_writer(
            open_content_writer(content_index, writer)?,
            f,
            batch.files,
            Some(content_backfill.batch_limit),
        )?;
        crate::throttle::release_idle_memory();
        Some((attempted, indexed))
    };
    let work = started.elapsed();
    // Mid-pass the writer stays open, so a batch costs its own commit and not a
    // wait for every merge the pass has started so far; the rest before the
    // next one follows how long this one took and how busy the machine is.
    let pace = batch.has_more.then(|| crate::throttle::backfill_pace(work));
    if let Some((attempted, indexed)) = processed {
        let next = pace.map_or_else(String::new, |(load, pause)| {
            format!(", {load:?}: next batch in {} ms", pause.as_millis())
        });
        log::info!(
            "content backfill: processed {attempted} files, indexed {indexed} bodies in {} ms{next} (limit {} files / {} MiB)",
            work.as_millis(),
            content_backfill.batch_limit,
            content::daemon_batch_byte_limit() / (1 << 20)
        );
    }
    if let Some((_, pause)) = pace {
        content_backfill.next_batch_at = Some(Instant::now() + pause);
        return Ok(false);
    }
    close_index_writer(writer).context("close content writer")?;
    crate::throttle::release_idle_memory();
    if batch.deferred > 0 {
        // Only cooldown-protected files remain: sleep until the earliest one
        // becomes eligible (or a content search warms the backfill sooner).
        let retry_in = batch.retry_in.unwrap_or(content_backfill.idle_delay);
        log::info!(
            "content backfill: {} file(s) deferred by edit cooldown; retry in ~{}s",
            batch.deferred,
            retry_in.as_secs()
        );
        content_backfill.finish_pass_deferred(retry_in);
        return Ok(false);
    }
    if content_backfill.finish_pass() {
        content_backfill.deferred_gauge.store(0, Ordering::Relaxed);
        log::info!("content backfill: complete");
        crate::index::mark_content_state_synced(&crate::config::content_index_dir())
            .context("mark content index synced")?;
        return Ok(true);
    }
    Ok(false)
}

/// Merge every segment of `index` into one when enough has changed (see
/// [`crate::index::worth_compacting`]); short of that, merge the small segments
/// that piled up beside the largest into one. Each segment costs a cold query
/// its own disk reads, and joining the small ones costs a fraction of a full
/// merge.
///
/// The writer's merge thread does the work, at the indexer's idle priority, and
/// publishes the result itself; nothing waits for it here, and whoever closes
/// the writer next waits for it to end. A merge already running on one of the
/// segments makes tantivy refuse this one, which the next finished pass asks
/// again.
fn compact_when_worth_it(index: &Index, writer: &mut IndexWriter, label: &str) {
    let Ok(metas) = index.searchable_segment_metas() else {
        return;
    };
    let segments: Vec<(u32, u32)> = metas
        .iter()
        .map(|meta| (meta.max_doc(), meta.num_deleted_docs()))
        .collect();
    let ids: Vec<_> = if crate::index::worth_compacting(&segments) {
        log::info!("compacting the {label} index: {} segments", metas.len());
        metas.iter().map(|meta| meta.id()).collect()
    } else if metas.len() >= 3 {
        let largest = metas
            .iter()
            .max_by_key(|meta| meta.max_doc())
            .map(|meta| meta.id());
        log::info!(
            "joining the {} small segments of the {label} index",
            metas.len() - 1
        );
        metas
            .iter()
            .map(|meta| meta.id())
            .filter(|id| Some(*id) != largest)
            .collect()
    } else {
        return;
    };
    drop(writer.merge(&ids));
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
        WatchControlCommand::Rebuild => {
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
            close_index_writer(content.writer)
                .context("close content writer after rebuild clear")?;
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
    }
}

/// Add a batch's paths; returns whether a queue overflow was signalled.
fn ingest(batch: &mut WatchBatch, watcher_result: notify::Result<notify::Event>) -> bool {
    match watcher_result {
        Ok(event) => {
            let rescan = event.need_rescan();
            batch.add_origin_paths(origin_paths_for_event(event.kind, &event.paths));
            batch.add_history(event.kind, &event.paths);
            for p in event.paths {
                batch.paths.insert(p);
            }
            rescan
        }
        Err(_) => false,
    }
}

fn observe_origin_batch(origin_writer: &origin::OriginWriter, batch: &WatchBatch) {
    if batch.origin_overflow {
        log::debug!("origin watcher batch exceeded limit; skipping origin heuristics");
        return;
    }
    for path in &batch.origin_paths {
        origin_writer.observe_recent_path(path);
    }
}

fn origin_paths_for_event(kind: EventKind, paths: &[PathBuf]) -> Vec<PathBuf> {
    match kind {
        EventKind::Create(CreateKind::Any | CreateKind::File | CreateKind::Other) => paths.to_vec(),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            paths.get(1).into_iter().cloned().collect()
        }
        EventKind::Modify(ModifyKind::Name(
            RenameMode::Any | RenameMode::To | RenameMode::Other,
        )) => paths.to_vec(),
        _ => Vec::new(),
    }
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
            let (mtime, size) = state::meta_pair(&metadata);
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
        } else if recorded.is_some() && (gone || out_of_scope) {
            // An unplugged removable or network source keeps its catalogue;
            // its files are reported as offline, not deleted.
            if gone
                && offline_sources
                    .get_or_insert_with(crate::query::OfflineSources::default)
                    .is_retained_offline(&path)
            {
                continue;
            }
            // A folder moved away, to the trash included, is one event naming
            // the folder and none for what it held: its whole tree goes.
            scan::delete_tree(writer, f, &path);
            state.remove_tree(&path);
            outcome.index_changed = true;
            outcome.state_changed = true;
            outcome.deleted = true;
        }
        // Otherwise never catalogued, or not readable now (EACCES, EIO on a
        // disk going away): nothing to drop on that evidence.
    }
    Ok(outcome)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Searches typed faster than the idle delay must not keep restarting it,
    /// or the bodies they ask for are never extracted.
    #[test]
    fn repeated_warm_keeps_the_deadline() {
        let mut backfill = ContentBackfill::new(Arc::new(AtomicU64::new(0)));
        backfill.warm();
        let first = backfill.wait_before_batch();
        std::thread::sleep(Duration::from_millis(20));
        backfill.warm();
        assert!(backfill.wait_before_batch() < first);
    }

    /// The merge is left running on the writer's merge thread; it must still
    /// land, and an index with nothing to tidy must be left alone.
    #[test]
    fn compaction_merges_into_one_segment() {
        let dir = std::env::temp_dir().join(format!("bs-compact-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let index = crate::index::open_or_create(&dir).unwrap();
        let f = fields(&index).unwrap();
        let mut writer = crate::index::background_writer(&index).unwrap();
        for n in 0..4 {
            let path = std::path::PathBuf::from(format!("/docs/{n}.md"));
            crate::scan::add_name_only(&writer, &f, &path, None).unwrap();
            writer.commit().unwrap();
        }
        assert_eq!(index.searchable_segment_ids().unwrap().len(), 4);

        compact_when_worth_it(&index, &mut writer, "test");
        writer.wait_merging_threads().unwrap();
        assert_eq!(index.searchable_segment_ids().unwrap().len(), 1);

        let mut writer = crate::index::background_writer(&index).unwrap();
        let before = index.searchable_segment_ids().unwrap();
        compact_when_worth_it(&index, &mut writer, "test");
        writer.wait_merging_threads().unwrap();
        assert_eq!(index.searchable_segment_ids().unwrap(), before);

        // Below a tenth changed, the small segments beside the largest are joined.
        let mut writer = crate::index::background_writer(&index).unwrap();
        for n in 4..24 {
            let path = std::path::PathBuf::from(format!("/docs/{n}.md"));
            crate::scan::add_name_only(&writer, &f, &path, None).unwrap();
        }
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
        let mut writer = crate::index::background_writer(&index).unwrap();
        compact_when_worth_it(&index, &mut writer, "test");
        writer.wait_merging_threads().unwrap();
        let mut writer = crate::index::background_writer(&index).unwrap();
        for n in 24..26 {
            let path = std::path::PathBuf::from(format!("/docs/{n}.md"));
            crate::scan::add_name_only(&writer, &f, &path, None).unwrap();
            writer.commit().unwrap();
        }
        assert_eq!(index.searchable_segment_ids().unwrap().len(), 3);
        compact_when_worth_it(&index, &mut writer, "test");
        writer.wait_merging_threads().unwrap();
        let metas = index.searchable_segment_metas().unwrap();
        let mut sizes: Vec<u32> = metas.iter().map(|meta| meta.max_doc()).collect();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![2, 24]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
