//! Incremental index updates via inotify (notify crate), scoped to visible dirs.
//!
//! One non-recursive watch per visible directory (~28k on a typical home, well under
//! the kernel `max_user_watches`) — unprivileged, and the hidden/gitignored churn
//! (the 90 %) is never watched, so it never wakes us. New visible dirs get a watch +
//! a scoped rescan. Events are coalesced over a short window, then reconciled by
//! existence: present → upsert, gone → delete.
use crate::content;
use crate::index::{ContentFields, Fields, content_fields, fields};
use crate::origin;
use crate::scan;
use crate::state::{self, State};
use anyhow::{Context, Result};
use notify::event::{CreateKind, EventKind, ModifyKind, RenameMode};
use notify::{RecursiveMode, Watcher, recommended_watcher};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
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
}

pub struct WatchScope<'a> {
    roots: &'a [PathBuf],
    initial_dirs: Vec<PathBuf>,
}

impl<'a> WatchScope<'a> {
    pub fn new(roots: &'a [PathBuf], initial_dirs: Vec<PathBuf>) -> Self {
        Self {
            roots,
            initial_dirs,
        }
    }
}

impl WatchRuntime {
    pub fn new(
        paused: Arc<AtomicBool>,
        activity: Arc<AtomicU8>,
        pending_content: Arc<AtomicU64>,
    ) -> Self {
        Self {
            paused,
            activity,
            pending_content,
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
    /// Forget every recorded file origin, at the person's request.
    ClearOrigin,
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
    /// Shared gauge read by the IPC thread: files currently deferred by the
    /// cooldown (the "content results may be incomplete" hint).
    deferred_gauge: Arc<AtomicU64>,
    /// A catalogue row was removed since the last orphan prune, so there may be
    /// content documents left behind. Nothing else can create an orphan.
    deletion_seen: bool,
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
            deferred_gauge,
            // The first pass after a start prunes unconditionally: deletions may
            // have happened while the daemon was down.
            deletion_seen: true,
        }
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

    /// A content search happened: run a cooldown-free pass as soon as idle.
    fn warm(&mut self) {
        self.bypass_cooldown = true;
        self.snooze_until = None;
        self.request();
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
            return; // fresh work exists — next idle tick runs at normal cadence
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
            None => self.idle_delay,
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
        // and rename-to cover the file that arrives already written — from a
        // download, from another folder — and never sees a close at all.
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
    let mut watcher = recommended_watcher(move |watcher_result| {
        let _ = fs_tx.send(WatchMessage::Fs(watcher_result));
    })
    .context("inotify init")?;

    let mut watched = 0usize;
    if scope.initial_dirs.is_empty() {
        for root in scope.roots {
            for dir in scan::visible_dirs(root) {
                if watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
                    watched += 1;
                }
            }
        }
    } else {
        for dir in scope.initial_dirs {
            if watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
                watched += 1;
            }
        }
    }
    log::info!("watching {watched} visible directories");
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
                    // One page of the catalogue per idle turn, and only while
                    // the machine has nothing better to do.
                    if !history_pass_done {
                        history_pass_done =
                            send_history_page(&state, &history_tx, &mut history_cursor);
                    }
                    runtime
                        .activity
                        .store(ACTIVITY_BACKFILLING, Ordering::Relaxed);
                    if let Err(e) = run_content_backfill_batch(
                        &content_index,
                        &mut content_writer,
                        &content_fields,
                        &mut content_backfill,
                    ) {
                        log::error!("content backfill failed: {e:#}");
                        content_backfill.request();
                    }
                    runtime.activity.store(ACTIVITY_IDLE, Ordering::Relaxed);
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else if history_pass_done {
            match rx.recv() {
                Ok(message) => message,
                Err(_) => break,
            }
        } else {
            // The pass over the catalogue also runs when there is no indexing
            // left to do — which is most of the time, and was where it never got
            // a turn: the loop simply blocked on the channel and the first
            // version of anything waited for the next file to change.
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
                runtime.activity.store(ACTIVITY_INDEXING, Ordering::Relaxed);
                // Forgetting the path is what forces the work: the reconcile
                // below then sees it as new instead of unchanged.
                state.remove(&path);
                // The content index keeps its own record of the same path, and
                // the only reason to ask for a forced reindex is a change the
                // file's size and modification time do not show — a tag. Left
                // in place, that record makes the content pass skip the file and
                // the tag never becomes searchable.
                content::forget_content_state(&path).context("forget content state")?;
                let mut forced = std::collections::HashSet::new();
                forced.insert(path);
                let outcome = reconcile(
                    active_index_writer(&mut writer),
                    &f,
                    &mut watcher,
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
                )? {
                    recycle_background_writer(&index, &mut writer)
                        .context("recycle index writer")?;
                    content_backfill.request();
                    crate::throttle::release_idle_memory();
                }
                continue;
            }
            WatchMessage::Fs(watcher_result) => watcher_result,
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
                Ok(WatchMessage::Fs(event_result)) => rescan |= ingest(&mut batch, event_result),
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
        let mut outcome = reconcile(
            active_index_writer(&mut writer),
            &f,
            &mut watcher,
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
                // Handled here because this is where the origin database's own
                // connection lives. A client cannot do it by deleting the file:
                // this thread would keep writing into an inode nobody can read.
                WatchControlCommand::ClearOrigin => {
                    if let Err(error) = origin_writer.forget_all() {
                        log::warn!("origins could not be forgotten: {error:#}");
                    }
                }
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

fn run_content_backfill_batch(
    content_index: &Index,
    writer: &mut Option<IndexWriter>,
    f: &ContentFields,
    content_backfill: &mut ContentBackfill,
) -> Result<()> {
    // Only when something was actually deleted. Proving "no orphans" still costs
    // a full scan of the content namespace — 19 865 page reads on this corpus —
    // and running that at the start of every pass meant paying it after every
    // burst of file changes, which is most of them.
    if content_backfill.cursor.is_none() && content_backfill.take_deletion_seen() {
        close_index_writer(writer).context("close content writer before prune")?;
        match content::prune_orphaned_content(content_index, content::ORPHAN_PRUNE_STEADY_LIMIT) {
            Ok((0, _)) => {}
            Ok((removed, more)) => {
                log::info!(
                    "content prune: dropped {removed} document(s) for deleted files{}",
                    if more { ", more remain" } else { "" }
                );
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

    if !batch.files.is_empty() {
        let attempted = batch.files.len();
        let indexed = content::index_content_limited_with_writer(
            open_content_writer(content_index, writer)?,
            f,
            batch.files,
            Some(content_backfill.batch_limit),
        )?;
        close_index_writer(writer).context("close content writer")?;
        crate::throttle::release_idle_memory();
        log::info!(
            "content backfill: processed {attempted} files, indexed {indexed} bodies (limit {} files / {} MiB)",
            content_backfill.batch_limit,
            content::daemon_batch_byte_limit() / (1 << 20)
        );
    }
    if batch.has_more {
        return Ok(()); // still mid-pass; the next idle tick continues at the cursor
    }
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
        return Ok(());
    }
    if content_backfill.finish_pass() {
        content_backfill.deferred_gauge.store(0, Ordering::Relaxed);
        log::info!("content backfill: complete");
        crate::index::mark_content_state_synced(&crate::config::content_index_dir())
            .context("mark content index synced")?;
    }
    Ok(())
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
) -> Result<bool> {
    match command {
        // Handled by the run loop (needs the backfill state); nothing to do here.
        WatchControlCommand::WarmContent => Ok(false),
        // Likewise: the run loop owns the origin database's connection.
        WatchControlCommand::ClearOrigin => Ok(false),
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
            let outcome = scan::reconcile_inline(writer, f, state, roots)?;
            if outcome.index_changed {
                writer.commit().context("commit resume reconcile")?;
            }
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
            scan::reconcile_inline(writer, f, state, roots)?;
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
    watcher: &mut notify::RecommendedWatcher,
    state: &mut State,
    scope: &scan::Scope,
    batch: HashSet<PathBuf>,
) -> Result<scan::ReconcileOutcome> {
    let mut outcome = scan::ReconcileOutcome::default();
    for path in batch {
        let metadata = std::fs::symlink_metadata(&path).ok();
        let out_of_scope = scope.excluded(&path, metadata.as_ref().is_some_and(|m| m.is_dir()));
        if out_of_scope && !state.contains(&path) {
            continue; // out of scope and never catalogued: nothing to do
        }
        // Below this line an excluded path is treated exactly like a vanished
        // one, so the rules also evict what an earlier version indexed before
        // they covered it. Skipping it instead would strand those documents.
        if let Some(metadata) = metadata.filter(|_| !out_of_scope) {
            let (mtime, size) = state::meta_pair(&metadata);
            if state.unchanged(&path, mtime, size) {
                continue; // metadata-only event (e.g. ATTRIB) → nothing to reindex
            }
            if !state.contains(&path) {
                // New path → index its name. A known path that merely grew or
                // was rewritten keeps its identical name doc (content work is
                // scheduled through the state change below).
                scan::upsert_path(writer, f, &path)?;
                outcome.index_changed = true;
            }
            state.set(&path, mtime, size);
            outcome.state_changed = true;
            if metadata.is_dir() {
                // New or moved-in dir: reconcile current contents (the watch was
                // not yet placed, so those entries may have produced no events).
                let (subdirs, subtree_outcome) =
                    scan::reconcile_subtree_inline(writer, f, state, scope, &path)?;
                outcome.absorb(subtree_outcome);
                for sub in subdirs {
                    let _ = watcher.watch(&sub, RecursiveMode::NonRecursive);
                }
                let _ = watcher.watch(&path, RecursiveMode::NonRecursive);
            }
        } else {
            // Gone, unstattable, or newly out of scope → drop it.
            scan::delete_path(writer, f, &path);
            state.remove(&path);
            outcome.index_changed = true;
            outcome.state_changed = true;
            outcome.deleted = true;
        }
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
    let Ok(paths) = state.paths_after(cursor.as_deref(), PAGE) else {
        return true;
    };
    if paths.is_empty() {
        return true;
    }
    *cursor = paths.last().map(|path| path.to_string_lossy().into_owned());
    let done = paths.len() < PAGE;
    // The catalogue is mostly not documents, and the worker would learn that one
    // path at a time, after a clone and a queue insert each.
    let documents: Vec<PathBuf> = paths
        .into_iter()
        .filter(|path| crate::history::may_be_a_document(path))
        .collect();
    if !documents.is_empty() {
        let _ = history.send(crate::history::HistoryEvent::Catalogued(documents));
    }
    done
}
