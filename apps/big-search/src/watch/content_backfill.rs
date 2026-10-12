//! The content backfill: extracting file bodies into the content index a batch
//! at a time while the watcher in `watch.rs` is otherwise idle, then tidying
//! both indexes once a pass ends. The watcher owns the writers and decides when
//! a batch runs; this module decides what one batch does and when the next is
//! due.
use super::{close_index_writer, open_content_writer};
use crate::content;
use crate::index::ContentFields;
use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tantivy::{Index, IndexWriter};
/// Longest wait after content backfill batches keep failing (a full disk, a
/// damaged index): the daemon must not wake every few seconds to fail again.
const MAX_BACKFILL_RETRY: Duration = Duration::from_secs(600);

pub(super) struct ContentBackfill {
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
    pub(super) snooze_until: Option<Instant>,
    /// When the next batch is due: in a running pass, set from how long the
    /// last batch took and how busy the machine is; before one, by the first
    /// content search (see [`Self::warm`]). A deadline, not a delay: file
    /// events and searches that arrive meanwhile must not keep pushing it back.
    pub(super) next_batch_at: Option<Instant>,
    /// Shared gauge read by the IPC thread: files currently deferred by the
    /// cooldown (the "content results may be incomplete" hint).
    deferred_gauge: Arc<AtomicU64>,
    /// A catalogue row was removed since the last orphan prune, so there may be
    /// content documents left behind. Nothing else can create an orphan.
    deletion_seen: bool,
    /// Batches that failed in a row; each one doubles the wait before the next.
    pub(super) failures: u32,
}

impl ContentBackfill {
    pub(super) fn new(deferred_gauge: Arc<AtomicU64>) -> Self {
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
    pub(super) fn retry_after_failure(&mut self) {
        self.request();
        self.failures = self.failures.saturating_add(1);
        let wait = self
            .idle_delay
            .saturating_mul(1 << self.failures.min(10))
            .min(MAX_BACKFILL_RETRY);
        self.snooze_until = Some(Instant::now() + wait);
    }

    pub(super) fn note_deletion(&mut self) {
        self.deletion_seen = true;
    }

    /// Whether to prune now, clearing the flag.
    fn take_deletion_seen(&mut self) -> bool {
        std::mem::replace(&mut self.deletion_seen, false)
    }

    pub(super) fn request(&mut self) {
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
    pub(super) fn warm(&mut self) {
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

    pub(super) fn should_run_when_idle(&self, paused: &AtomicBool) -> bool {
        self.is_pending && !paused.load(Ordering::Relaxed)
    }

    /// How long the watcher may sleep before the next backfill attempt.
    pub(super) fn wait_before_batch(&self) -> Duration {
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

/// One batch of the content backfill; `true` when it finished a pass with
/// nothing left to read.
pub(super) fn run_content_backfill_batch(
    content_index: &Index,
    writer: &mut Option<IndexWriter>,
    f: &ContentFields,
    content_backfill: &mut ContentBackfill,
) -> Result<bool> {
    let started = Instant::now();
    crate::throttle::begin_cpu_window();
    prune_orphans_after_deletion(content_index, writer, content_backfill)?;

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
    end_pass(writer, content_backfill, batch.deferred, batch.retry_in)
}

/// At the start of a pass, drop content documents whose files left the
/// catalogue, when a deletion was seen since the last prune.
fn prune_orphans_after_deletion(
    content_index: &Index,
    writer: &mut Option<IndexWriter>,
    content_backfill: &mut ContentBackfill,
) -> Result<()> {
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
    Ok(())
}

/// A batch that reached the end of the catalogue: close the writer, then
/// either sleep until a cooldown-deferred file is eligible or finish the pass.
/// `true` when the backfill is fully done.
fn end_pass(
    writer: &mut Option<IndexWriter>,
    content_backfill: &mut ContentBackfill,
    deferred: usize,
    retry_in: Option<Duration>,
) -> Result<bool> {
    close_index_writer(writer).context("close content writer")?;
    crate::throttle::release_idle_memory();
    if deferred > 0 {
        // Only cooldown-protected files remain: sleep until the earliest one
        // becomes eligible (or a content search warms the backfill sooner).
        let retry_in = retry_in.unwrap_or(content_backfill.idle_delay);
        log::info!(
            "content backfill: {} file(s) deferred by edit cooldown; retry in ~{}s",
            deferred,
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
pub(super) fn compact_when_worth_it(index: &Index, writer: &mut IndexWriter, label: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::fields;

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
