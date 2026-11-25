//! What the inotify thread heard and the watcher loop in `watch.rs` has not
//! taken yet: the paths to reconcile, the downloads worth an origin lookup,
//! and the saves and renames worth a version. Held in memory only: what a
//! stop loses, the next start's catch-up or walk finds.
use super::WatchMessage;
use crate::origin;
use notify::event::{AccessKind, AccessMode, CreateKind, EventKind, ModifyKind, RenameMode};
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, PoisonError};

/// Paths a batch holds before they collapse into their folders: a backup or a
/// recursive copy must not hold its whole tree in memory.
pub(super) const MAX_PATHS: usize = 16_384;
/// Folders a batch holds before it gives them up for a walk of the sources:
/// each one is walked whole.
pub(super) const MAX_FOLDERS: usize = 256;
/// Renames a batch keeps for the versions worker; past it a renamed
/// document's versions stay under its old path.
const MAX_RENAMES: usize = 16_384;

/// Filled by the inotify thread, one entry per path however often it
/// changes, and taken whole by the watcher loop.
pub(super) struct Heard {
    /// `None` once taken: the next event wakes the loop again.
    batch: Mutex<Option<WatchBatch>>,
    /// The sources, which no path collapses above.
    roots: Vec<PathBuf>,
    wake: Sender<WatchMessage>,
}

impl Heard {
    pub(super) fn new(roots: Vec<PathBuf>, wake: Sender<WatchMessage>) -> Self {
        Self {
            batch: Mutex::new(None),
            roots,
            wake,
        }
    }

    /// Add one event, and wake the loop when it is the first since the loop
    /// last took the batch.
    pub(super) fn hear(&self, watcher_result: notify::Result<notify::Event>) {
        let Ok(event) = watcher_result else {
            return;
        };
        let first = {
            let mut batch = self.batch.lock().unwrap_or_else(PoisonError::into_inner);
            let first = batch.is_none();
            batch
                .get_or_insert_with(WatchBatch::new)
                .ingest(event, &self.roots);
            first
        };
        if first {
            // Fails only once the loop is gone, and the process with it.
            let _ = self.wake.send(WatchMessage::Heard);
        }
    }

    /// What was heard since the last take, if anything.
    pub(super) fn take(&self) -> Option<WatchBatch> {
        self.batch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// Why a batch walks the sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rescan {
    /// The kernel's queue overflowed: events were lost.
    Overflowed,
    /// The folders passed [`MAX_FOLDERS`].
    Collapsed,
}

pub(super) struct WatchBatch {
    pub(super) paths: HashSet<PathBuf>,
    /// Folders walked whole in place of the paths that passed [`MAX_PATHS`],
    /// none below another.
    pub(super) folders: BTreeSet<PathBuf>,
    /// Set when the sources are walked instead of `paths` and `folders`,
    /// which stay empty.
    pub(super) rescan: Option<Rescan>,
    origin_paths: Vec<PathBuf>,
    origin_overflow: bool,
    /// Files that may have just been saved, for the versions worker. Bounded by
    /// [`Self::HISTORY_LIMIT`].
    pub(super) history_paths: HashSet<PathBuf>,
    /// Renames as reported pairs: the save that usually follows replaces the
    /// inode, so the destination alone would lose the document's past.
    pub(super) history_renames: Vec<(PathBuf, PathBuf)>,
}

impl WatchBatch {
    pub(super) fn new() -> Self {
        Self {
            paths: HashSet::new(),
            folders: BTreeSet::new(),
            rescan: None,
            origin_paths: Vec::new(),
            origin_overflow: false,
            history_paths: HashSet::new(),
            history_renames: Vec::new(),
        }
    }

    fn add_origin_paths(&mut self, paths: &[PathBuf]) {
        if self.origin_overflow || !origin::settings().watch_enabled {
            return;
        }
        for path in paths {
            if self.origin_paths.len() >= origin::settings().max_batch {
                self.origin_paths.clear();
                self.origin_overflow = true;
                return;
            }
            self.origin_paths.push(path.clone());
        }
    }

    /// Most files one batch offers the versions worker; the catalogue pass
    /// covers the rest of a large burst (e.g. an unpacked archive).
    const HISTORY_LIMIT: usize = 512;

    /// Note what may have been saved, and the renames as pairs.
    fn add_history(&mut self, kind: EventKind, paths: &[PathBuf]) {
        if let EventKind::Modify(ModifyKind::Name(RenameMode::Both)) = kind
            && let (Some(from), Some(to)) = (paths.first(), paths.get(1))
        {
            if self.history_renames.len() < MAX_RENAMES {
                self.history_renames.push((from.clone(), to.clone()));
            }
            self.remember_history_path(to.clone());
            return;
        }
        // A close after writing is the strongest signal a save finished. Create
        // and rename-to cover the file that arrives already written (from a
        // download, from another folder) and never sees a close at all.
        let worth_a_version = matches!(
            kind,
            EventKind::Access(AccessKind::Close(AccessMode::Write))
                | EventKind::Create(_)
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
        // Filtered here: most changes are not documents, and each would cost a
        // clone and a channel send to reject in the worker.
        if self.history_paths.len() < Self::HISTORY_LIMIT
            && crate::history::may_be_a_document(&path)
        {
            self.history_paths.insert(path);
        }
    }

    fn ingest(&mut self, event: notify::Event, roots: &[PathBuf]) {
        if event.need_rescan() {
            self.give_up_paths(Rescan::Overflowed);
        }
        self.add_origin_paths(origin_paths_for_event(event.kind, &event.paths));
        self.add_history(event.kind, &event.paths);
        for path in event.paths {
            self.add_path(path, roots);
        }
    }

    fn add_path(&mut self, path: PathBuf, roots: &[PathBuf]) {
        // Most batches hold no folder: no event pays the walk up its path.
        let in_folder = !self.folders.is_empty()
            && path.ancestors().any(|folder| self.folders.contains(folder));
        if self.rescan.is_some() || in_folder {
            return;
        }
        self.paths.insert(path);
        if self.paths.len() <= MAX_PATHS {
            return;
        }
        for path in std::mem::take(&mut self.paths) {
            // The walk of a folder reads what changed in it; a source itself
            // has no folder to stand for it.
            let folder = match path.parent() {
                Some(parent) if roots.iter().any(|root| parent.starts_with(root)) => {
                    parent.to_path_buf()
                }
                _ => path,
            };
            self.add_folder(folder);
            if self.folders.len() > MAX_FOLDERS {
                self.give_up_paths(Rescan::Collapsed);
                return;
            }
        }
    }

    fn add_folder(&mut self, folder: PathBuf) {
        if folder.ancestors().any(|above| self.folders.contains(above)) {
            return;
        }
        // The folders below one sort right after it.
        if self
            .folders
            .range(folder.clone()..)
            .next()
            .is_some_and(|next| next.starts_with(&folder))
        {
            self.folders.retain(|below| !below.starts_with(&folder));
        }
        self.folders.insert(folder);
    }

    fn give_up_paths(&mut self, cause: Rescan) {
        self.rescan = Some(cause);
        self.paths = HashSet::new();
        self.folders = BTreeSet::new();
    }
}

pub(super) fn observe_origin_batch(origin_writer: &origin::OriginWriter, batch: &WatchBatch) {
    if batch.origin_overflow {
        log::debug!("origin watcher batch exceeded limit; skipping origin heuristics");
        return;
    }
    for path in &batch.origin_paths {
        origin_writer.observe_recent_path(path);
    }
}

fn origin_paths_for_event(kind: EventKind, paths: &[PathBuf]) -> &[PathBuf] {
    match kind {
        EventKind::Create(CreateKind::Any | CreateKind::File | CreateKind::Other) => paths,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => paths.get(1..2).unwrap_or(&[]),
        EventKind::Modify(ModifyKind::Name(
            RenameMode::Any | RenameMode::To | RenameMode::Other,
        )) => paths,
        _ => &[],
    }
}
