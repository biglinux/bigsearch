//! The versions worker: an idle-priority thread of its own, fed by the watcher.
//!
//! [`run_worker`] receives [`HistoryEvent`]s, holds each changed document in
//! [`Pending`] until it has been quiet, saves its version, and between saves
//! keeps the store inside its budget and the disk usable. The store's lock is
//! taken per step, so a file manager's request never waits behind a batch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::disk::{clone_file, disk_capacity, low_disk_threshold, recovered_disk_target};
use super::store::now_secs;
use super::{Reason, SharedHistory, Support, Unavailable};

/// How long a file is kept in the catalogue after its path disappears.
const MISSING_GRACE_SECONDS: i64 = 7 * 24 * 60 * 60;

/// Forget documents whose file has been gone long enough.
///
/// The store is locked to read the catalogue and to write the outcome, not
/// while the filesystem is asked about every document: the worker runs at idle
/// priority, and a file manager's request for a version waits on the same lock.
/// A row renamed or saved in between stops matching what was checked and is
/// left for the next sweep.
fn expire_missing(history: &SharedHistory) -> Result<()> {
    let files: Vec<(i64, String, Option<i64>)> = {
        let store = history.lock().expect("versions store lock");
        let Some(conn) = &store.conn else {
            return Ok(());
        };
        conn.prepare("SELECT id, current_path, missing_since FROM files")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let checked: Vec<_> = files
        .into_iter()
        .map(|(id, path, missing_since)| {
            let exists = Path::new(&path).exists();
            (id, path, missing_since, exists)
        })
        .collect();

    let now = now_secs();
    let mut store = history.lock().expect("versions store lock");
    let Some(conn) = &store.conn else {
        return Ok(());
    };
    let mut expired: Vec<String> = Vec::new();
    for (id, path, missing_since, exists) in checked {
        if note_presence(conn, id, &path, missing_since, exists, now)? {
            expired.push(path);
        }
    }
    for path in expired {
        store.forget(Path::new(&path))?;
    }
    Ok(())
}

/// Record whether the file at `path` exists: a document back in place stops
/// counting as missing, one just gone starts its grace period. True when the
/// grace has run out and the row is still the one that was checked.
fn note_presence(
    conn: &rusqlite::Connection,
    id: i64,
    path: &str,
    missing_since: Option<i64>,
    exists: bool,
    now: i64,
) -> Result<bool> {
    match (exists, missing_since) {
        (true, Some(_)) => {
            conn.execute(
                "UPDATE files SET missing_since = NULL WHERE id = ?1 AND current_path = ?2",
                params![id, path],
            )?;
        }
        (false, None) => {
            conn.execute(
                "UPDATE files SET missing_since = ?3
                  WHERE id = ?1 AND current_path = ?2 AND missing_since IS NULL",
                params![id, path, now],
            )?;
        }
        (false, Some(since)) if now - since > MISSING_GRACE_SECONDS => {
            let unchanged: Option<i64> = conn
                .query_row(
                    "SELECT id FROM files
                      WHERE id = ?1 AND current_path = ?2 AND missing_since = ?3",
                    params![id, path, since],
                    |row| row.get(0),
                )
                .optional()?;
            return Ok(unchanged.is_some());
        }
        _ => {}
    }
    Ok(false)
}

/// What the worker is waiting to save, and until when.
pub struct Pending {
    deadlines: HashMap<PathBuf, std::time::Instant>,
    limit: usize,
}

impl Pending {
    /// Most files waiting at once. A bound, because a machine with little memory
    /// must not grow a map the size of an unpacked archive.
    const LIMIT: usize = 4096;

    pub fn new() -> Self {
        Self {
            deadlines: HashMap::new(),
            limit: Self::LIMIT,
        }
    }

    /// Note a change, pushing this file's deadline forward.
    ///
    /// Pushing forward rather than keeping the first is what stops an editor's
    /// autosave from becoming a hundred versions: the version is written once
    /// the file has been quiet.
    pub fn note(&mut self, path: PathBuf, wait: std::time::Duration) {
        if self.deadlines.len() >= self.limit && !self.deadlines.contains_key(&path) {
            return;
        }
        self.deadlines
            .insert(path, std::time::Instant::now() + wait);
    }

    /// Ready paths, oldest first, removed only when consumed by the worker.
    ///
    /// A time-limited batch can stop anywhere: dropping its iterator must leave
    /// every unvisited path and its original deadline queued for the next turn.
    pub fn due(&mut self) -> impl Iterator<Item = PathBuf> + '_ {
        let now = std::time::Instant::now();
        let mut due: Vec<_> = self
            .deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(path, deadline)| (path.clone(), *deadline))
            .collect();
        due.sort_unstable_by(|(left_path, left_deadline), (right_path, right_deadline)| {
            left_deadline
                .cmp(right_deadline)
                .then_with(|| left_path.cmp(right_path))
        });
        due.into_iter().map(|(path, _)| {
            self.deadlines.remove(&path);
            path
        })
    }

    pub fn is_empty(&self) -> bool {
        self.deadlines.is_empty()
    }
}

impl Default for Pending {
    fn default() -> Self {
        Self::new()
    }
}

/// What the watcher tells the versions worker.
pub enum HistoryEvent {
    /// This document may have just been saved.
    Changed(PathBuf),
    /// The kernel reported both names of a rename in one event.
    Renamed(PathBuf, PathBuf),
    /// A page of the catalogue's documents with the `(mtime, size)` it holds,
    /// for the pass that covers what no event did.
    Catalogued(Vec<(PathBuf, i64, u64)>),
}

/// How long a document has to be quiet before its version is written.
///
/// Long enough that an editor's autosave, or a program that writes a file three
/// times in a row, produces one version rather than three; short enough that
/// somebody who saves and immediately looks finds it there.
const QUIET_PERIOD: std::time::Duration = std::time::Duration::from_secs(3);
/// How often, at most, the worker looks at the disk and at documents that
/// disappeared. Only after something happened: with nothing saved or changed
/// there is nothing new to find, and the worker sleeps until an event.
const HOUSEKEEPING: std::time::Duration = std::time::Duration::from_secs(60);
/// How often the store checks whether documents it knows are still there.
///
/// Once an hour: it asks the filesystem about every document it keeps versions
/// of, and what it is looking for, a file gone for a week, does not move in a
/// minute.
const MISSING_SWEEP: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// How long the worker rests between bursts while it still has files waiting.
///
/// Twice the burst, so about a third of the time is spent working. The service
/// already runs at the lowest priority the kernel offers; this is what keeps it
/// from feeling busy on a slow machine even so.
const BACKLOG_PAUSE: std::time::Duration = std::time::Duration::from_millis(400);

/// How long the worker spends saving versions before yielding.
///
/// A budget in time, not in files: cloning charges by extent, so one fragmented
/// document can cost more than ten small ones, and on a slow disk a count would
/// be a promise the machine cannot keep.
const WORK_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

/// Versions dropped per hold of the store's lock during housekeeping. The
/// worker runs at idle priority and a file manager's request may be waiting on
/// the lock, so it waits for one batch at most.
const DROP_BATCH: usize = 16;

/// Drop `versions`, objects first, a batch per hold of the lock.
fn drop_in_batches(history: &SharedHistory, versions: &[(i64, String)]) -> Result<()> {
    for batch in versions.chunks(DROP_BATCH) {
        history
            .lock()
            .expect("versions store lock")
            .drop_versions(batch)?;
    }
    Ok(())
}

/// Keep the whole store inside its budget.
///
/// Two passes, because size and age answer different questions: first no
/// document may hold more than a tenth of the budget, then the oldest go.
fn enforce_budget(history: &SharedHistory) -> Result<()> {
    let hogs = history.lock().expect("versions store lock").budget_hogs()?;
    drop_in_batches(history, &hogs)?;
    let oldest = history.lock().expect("versions store lock").over_budget()?;
    drop_in_batches(history, &oldest)
}

/// Stop when the disk is nearly full, and give space back until it is not.
///
/// Stopping alone would not be enough. When the disk fills for some other
/// reason, the person deletes files and the space does not come back,
/// because these versions are still holding the superseded blocks, and the
/// person has no idea this store exists.
fn watch_free_space(history: &SharedHistory) -> Result<()> {
    if history.lock().expect("versions store lock").conn.is_none() {
        return Ok(());
    }
    let Ok(capacity) = disk_capacity(&crate::config::data_dir()) else {
        return Ok(());
    };
    let (paused, cost) = {
        let store = history.lock().expect("versions store lock");
        (store.paused_for_space, store.total_bytes()?)
    };
    // Once paused, the store waits for the recovered target, not the threshold.
    let resume_at = if paused {
        recovered_disk_target(cost)
    } else {
        low_disk_threshold(cost)
    };
    if capacity.available_bytes >= resume_at {
        history
            .lock()
            .expect("versions store lock")
            .paused_for_space = false;
        return Ok(());
    }
    history
        .lock()
        .expect("versions store lock")
        .paused_for_space = true;
    log::info!("little disk space left: giving kept versions back");
    let (available, target) = give_space_back(history, capacity.available_bytes, cost)?;
    if available >= target {
        history
            .lock()
            .expect("versions store lock")
            .paused_for_space = false;
    }
    Ok(())
}

/// Drop the oldest versions until the disk is back at the recovered target,
/// or nothing more comes back. Returns the free space and the target it ended
/// at.
fn give_space_back(history: &SharedHistory, mut available: u64, cost: u64) -> Result<(u64, u64)> {
    let mut target = recovered_disk_target(cost);
    while available < target {
        // Asked for the shortfall, not for one version: but the disk is
        // measured again after each batch, because a version only frees the
        // blocks it does *not* share with the live file. Its size is a
        // ceiling on what comes back, never the amount.
        let dropped = history
            .lock()
            .expect("versions store lock")
            .oldest_versions_totalling(target - available)?;
        if dropped.is_empty() {
            break;
        }
        drop_in_batches(history, &dropped)?;
        let Ok(now) = disk_capacity(&crate::config::data_dir()) else {
            break;
        };
        if now.available_bytes <= available {
            break; // nothing came back; stop rather than spin
        }
        available = now.available_bytes;
        // Fewer versions lower what they need left free.
        target = recovered_disk_target(history.lock().expect("versions store lock").total_bytes()?);
    }
    Ok((available, target))
}

/// Run the versions worker until the sender goes away.
///
/// Its own thread on purpose: the search must never wait for a version, and a
/// failure here must never be able to stop the watcher.
pub fn run_worker(history: &SharedHistory, events: std::sync::mpsc::Receiver<HistoryEvent>) {
    crate::throttle::lower_priority();
    report_and_tidy(history);
    let mut pending = Pending::new();
    let mut next_housekeeping = std::time::Instant::now() + HOUSEKEEPING;
    let mut next_missing_sweep = std::time::Instant::now() + MISSING_SWEEP;
    // The disk and documents gone missing are looked at once after a start,
    // then after each event or saved version.
    let mut housekeeping_owed = true;

    loop {
        let received = next_event(&events, &pending, housekeeping_owed, next_housekeeping);
        if received.is_ok() {
            housekeeping_owed = true;
        }
        match received {
            Ok(event) => queue_event(history, &mut pending, event),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }

        if save_due_versions(history, &mut pending) {
            housekeeping_owed = true;
        }

        if housekeeping_owed && std::time::Instant::now() >= next_housekeeping {
            housekeeping_owed = false;
            next_housekeeping = std::time::Instant::now() + HOUSEKEEPING;
            keep_house(history, &mut next_missing_sweep);
        }
    }
}

/// Say once why versions are not kept here, and remove what a crash left.
fn report_and_tidy(history: &SharedHistory) {
    let mut store = history.lock().expect("versions store lock");
    if let Support::Unavailable(reason) = store.support()
        && reason != Unavailable::Disabled
    {
        log::info!("earlier versions are not kept here: {}", reason.as_str());
    }
    if let Err(error) = store.sweep_orphans() {
        log::warn!("could not tidy the versions store: {error:#}");
    }
}

/// Wait for the next event, as long as the worker can afford to.
///
/// With a backlog, come back quickly; with none, sleep until there is
/// something to do. The first version of every document is written this
/// way, and a worker that waited out the whole quiet period between
/// batches would take hours over what should take minutes.
fn next_event(
    events: &std::sync::mpsc::Receiver<HistoryEvent>,
    pending: &Pending,
    housekeeping_owed: bool,
    next_housekeeping: std::time::Instant,
) -> Result<HistoryEvent, std::sync::mpsc::RecvTimeoutError> {
    if !pending.is_empty() {
        events.recv_timeout(BACKLOG_PAUSE)
    } else if housekeeping_owed {
        events.recv_timeout(next_housekeeping.saturating_duration_since(std::time::Instant::now()))
    } else {
        events
            .recv()
            .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)
    }
}

/// Turn one watcher event into documents waiting for their version.
fn queue_event(history: &SharedHistory, pending: &mut Pending, event: HistoryEvent) {
    match event {
        HistoryEvent::Changed(path) => pending.note(path, QUIET_PERIOD),
        HistoryEvent::Renamed(from, to) => {
            if let Err(error) = history
                .lock()
                .expect("versions store lock")
                .note_rename(&from, &to)
            {
                log::debug!("could not follow a rename: {error:#}");
            }
            pending.note(to, QUIET_PERIOD);
        }
        // The pass that covers what no event could: the queue overflowed, or
        // the service was not running when somebody saved. The catalogue
        // was just reconciled with the disk, so a document whose newest
        // version it matches is skipped without touching the disk.
        HistoryEvent::Catalogued(entries) => {
            let unkept = history.lock().expect("versions store lock").unkept(entries);
            for path in unkept {
                pending.note(path, std::time::Duration::ZERO);
            }
        }
    }
}

/// Save the versions that are due, for at most [`WORK_BUDGET`]. True when at
/// least one was kept.
fn save_due_versions(history: &SharedHistory, pending: &mut Pending) -> bool {
    let mut kept_any = false;
    let until = std::time::Instant::now() + WORK_BUDGET;
    let mut due = pending.due();
    while std::time::Instant::now() < until {
        let Some(path) = due.next() else { break };
        // Check the deadline before consuming the next path. Unvisited
        // entries remain queued even when this batch runs out of time.
        match save_version_unlocked(history, &path) {
            Ok(true) => {
                log::debug!("kept a version of {}", path.display());
                kept_any = true;
            }
            Ok(false) => {}
            Err(error) => log::debug!("no version of {}: {error:#}", path.display()),
        }
    }
    kept_any
}

/// [`super::History::save_version`] with the copy taken outside the lock: at
/// idle priority it can wait a long time for the disk, and the file manager's
/// request before a restore must not wait with it.
fn save_version_unlocked(history: &SharedHistory, path: &Path) -> Result<bool> {
    let prepared = history
        .lock()
        .expect("versions store lock")
        .prepare_version(path, Reason::Saved);
    match prepared {
        Ok(Some(version)) => {
            let copied = clone_file(path, &version.object);
            history
                .lock()
                .expect("versions store lock")
                .finish_version(version, copied)
        }
        Ok(None) => Ok(false),
        Err(error) => Err(error),
    }
}

/// One housekeeping turn: free space, the budget, and, once per
/// [`MISSING_SWEEP`], documents gone missing.
///
/// The lock is taken per small batch, so a request from the file manager never
/// waits for a whole step.
fn keep_house(history: &SharedHistory, next_missing_sweep: &mut std::time::Instant) {
    if let Err(error) = watch_free_space(history) {
        log::warn!("could not check the free space: {error:#}");
    }
    let unchecked = std::mem::take(
        &mut history
            .lock()
            .expect("versions store lock")
            .budget_unchecked,
    );
    if unchecked && let Err(error) = enforce_budget(history) {
        history
            .lock()
            .expect("versions store lock")
            .budget_unchecked = true;
        log::warn!("could not keep versions inside the budget: {error:#}");
    }
    if std::time::Instant::now() >= *next_missing_sweep {
        *next_missing_sweep = std::time::Instant::now() + MISSING_SWEEP;
        if let Err(error) = expire_missing(history) {
            log::warn!("could not retire missing documents: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::store::open_store;
    use crate::history::tests::store_with;

    /// A document gone past the grace period is forgotten; one that came back
    /// stops counting as missing; one just gone starts its grace period.
    #[test]
    fn missing_documents_expire_after_the_grace() {
        let dir = crate::test_env::scratch_dir("bs-history-missing");
        std::fs::create_dir_all(&dir).unwrap();
        let present = dir.join("present.md");
        std::fs::write(&present, b"x").unwrap();
        let conn = open_store(&dir.join("history.sqlite")).unwrap();
        let long_ago = now_secs() - MISSING_GRACE_SECONDS - 1;
        for (id, path, missing_since) in [
            (1, present.to_string_lossy().into_owned(), Some(long_ago)),
            (2, "/gone/old.md".to_string(), Some(long_ago)),
            (3, "/gone/new.md".to_string(), None),
        ] {
            conn.execute(
                "INSERT INTO files(id, current_path, created_at, missing_since)
                 VALUES (?1, ?2, 0, ?3)",
                params![id, path, missing_since],
            )
            .unwrap();
        }
        let history: SharedHistory =
            std::sync::Arc::new(std::sync::Mutex::new(store_with(conn, &dir)));
        expire_missing(&history).unwrap();

        let store = history.lock().unwrap();
        let rows: Vec<(i64, Option<i64>)> = store
            .conn
            .as_ref()
            .unwrap()
            .prepare("SELECT id, missing_since FROM files ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0], (1, None));
        assert!(rows[1].0 == 3 && rows[1].1.is_some(), "{rows:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_batch_keeps_unvisited_paths() {
        let mut pending = Pending::new();
        for name in ["a", "b", "c"] {
            pending.note(name.into(), std::time::Duration::ZERO);
        }
        let first = pending.due().next().unwrap();
        let rest: Vec<_> = pending.due().collect();
        assert_eq!(rest.len(), 2);
        assert!(!rest.contains(&first));
        assert!(pending.is_empty());
    }

    #[test]
    fn zero_budget_keeps_ready_paths() {
        let mut pending = Pending::new();
        pending.note("a".into(), std::time::Duration::ZERO);
        pending.note("b".into(), std::time::Duration::ZERO);
        drop(pending.due());
        assert_eq!(pending.due().count(), 2);
    }

    #[test]
    fn due_paths_run_oldest_first() {
        let mut pending = Pending::new();
        let now = std::time::Instant::now();
        pending.deadlines.insert("newest".into(), now);
        let oldest = now.checked_sub(std::time::Duration::from_secs(2)).unwrap();
        pending.deadlines.insert("oldest".into(), oldest);
        pending
            .deadlines
            .insert("future".into(), now + std::time::Duration::from_secs(60));
        assert_eq!(pending.due().next().unwrap(), PathBuf::from("oldest"));
        assert_eq!(
            pending.due().collect::<Vec<_>>(),
            vec![PathBuf::from("newest")]
        );
        assert!(pending.deadlines.contains_key(Path::new("future")));
    }

    #[test]
    fn later_save_keeps_its_debounce() {
        let mut pending = Pending::new();
        pending.note("a".into(), std::time::Duration::ZERO);
        pending.note("b".into(), std::time::Duration::ZERO);
        assert_eq!(pending.due().next().unwrap(), PathBuf::from("a"));
        pending.note("b".into(), std::time::Duration::from_secs(60));
        assert_eq!(pending.due().count(), 0);
        assert_eq!(pending.deadlines.len(), 1);
    }

    #[test]
    fn unvisited_paths_keep_queue_capacity() {
        let mut pending = Pending::new();
        pending.limit = 2;
        pending.note("a".into(), std::time::Duration::ZERO);
        pending.note("b".into(), std::time::Duration::ZERO);
        drop(pending.due());
        pending.note("c".into(), std::time::Duration::ZERO);
        assert_eq!(
            pending.due().collect::<Vec<_>>(),
            vec![PathBuf::from("a"), PathBuf::from("b")]
        );
        assert!(pending.is_empty());
    }
}
