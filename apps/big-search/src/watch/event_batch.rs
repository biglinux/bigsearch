//! One debounced burst of filesystem events, as the watcher loop in
//! `watch.rs` collects it: the paths to reconcile, the downloads worth an
//! origin lookup, and the saves and renames worth a version.
use crate::origin;
use notify::event::{CreateKind, EventKind, ModifyKind, RenameMode};
use std::collections::HashSet;
use std::path::PathBuf;

pub(super) struct WatchBatch {
    pub(super) paths: HashSet<PathBuf>,
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

    /// Most files one batch offers the versions worker; the catalogue pass
    /// covers the rest of a large burst (e.g. an unpacked archive).
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
        // Filtered here: most changes are not documents, and each would cost a
        // clone and a channel send to reject in the worker.
        if self.history_paths.len() < Self::HISTORY_LIMIT
            && crate::history::may_be_a_document(&path)
        {
            self.history_paths.insert(path);
        }
    }
}

/// Add a batch's paths; returns whether a queue overflow was signalled.
pub(super) fn ingest(
    batch: &mut WatchBatch,
    watcher_result: notify::Result<notify::Event>,
) -> bool {
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

pub(super) fn observe_origin_batch(origin_writer: &origin::OriginWriter, batch: &WatchBatch) {
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
