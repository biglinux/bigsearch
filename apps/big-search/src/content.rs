//! Background content pass: extract text from eligible files with a small throttled
//! worker pool, then enrich the (already name-indexed) docs with a `body` field.
//! Workers run extraction concurrently; a single writer applies the results.
use crate::content_dedupe::{ContentClaim, ContentSignatures};
use crate::index::{ContentFields, content_fields};
use crate::{catalogue, extract, throttle};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tantivy::indexer::UserOperation;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

pub struct PendingContentBatch {
    pub files: Vec<PathBuf>,
    pub has_more: bool,
    /// Files skipped this pass because they changed less than the cooldown ago
    /// (actively being edited). They are retried after `retry_in`, or at once
    /// when a content search warms the backfill.
    pub deferred: usize,
    /// Time until the earliest deferred file leaves its cooldown.
    pub retry_in: Option<Duration>,
    /// Catalogue entries `(path, mtime, size)` the extraction policy never
    /// touches (wrong type, or media with metadata off). The caller records
    /// them via [`record_skipped_candidates`] so the pending join stops
    /// re-reporting them on every pass.
    pub non_candidates: Vec<(PathBuf, i64, u64)>,
}

impl PendingContentBatch {
    fn empty() -> Self {
        Self {
            files: Vec::new(),
            has_more: false,
            deferred: 0,
            retry_in: None,
            non_candidates: Vec::new(),
        }
    }
}

/// Upper bound of policy-skipped entries collected per batch, so one pass over
/// a media-heavy catalogue cannot hold the whole set in memory at once.
const NON_CANDIDATE_TOMBSTONE_BATCH: usize = 4096;

/// Mark policy-skipped catalogue entries as handled in the content state
/// ("tombstones"), removing them from the pending join. Changing the indexing
/// policy later (e.g. enabling metadata) requires `big-search rebuild`, which
/// clears this state.
pub fn record_skipped_candidates(entries: &[(PathBuf, i64, u64)]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut content_state = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::CONTENT_NAMESPACE,
    );
    for (path, mtime, size) in entries {
        content_state.set(path, *mtime, *size);
    }
    content_state.persist()
}

/// Keyset cursor into the pending-content pass over the name catalogue: the last
/// catalogue path already considered, `None` before a pass starts or after one ends.
pub type CatalogCursor = Option<String>;

/// Delete documents whose path the name catalogue does not have, walking the
/// index rather than the content state. Returns how many went.
///
/// [`prune_orphaned_content`] cannot find these: it is driven by content state
/// rows, and [`clear_content_state`] (called among others by the daemon's own
/// startup when the name index is empty) wipes every row while leaving the
/// documents in place. From then on nothing references them, no bookkeeping can
/// see them, and they answer content searches with files that are gone.
///
/// Walks every live document, so this is a startup repair, not a per-pass step.
pub fn prune_unreferenced_content_docs(index: &Index) -> Result<usize> {
    let index_dir = crate::config::content_index_dir();
    if crate::index::is_unreferenced_pruned(&index_dir) {
        return Ok(0); // swept, and nothing has cleared content state since
    }
    let catalog = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::NAME_NAMESPACE,
    );
    // Fail safe, not deadly. `State::load` degrades to an empty in-memory state
    // when the database will not open (disk full, permissions), and an empty
    // catalogue references nothing, which would make this delete every content
    // document there is. Refusing costs a stale document; the alternative costs a
    // full re-extraction of the corpus for a transient error.
    if catalog.is_empty() {
        log::warn!("skipping unreferenced-content prune: the catalogue reads as empty");
        return Ok(0);
    }
    let mut doomed: Vec<PathBuf> = Vec::new();
    crate::index::for_each_indexed_path(index, |path| {
        if !catalog.contains(path) {
            doomed.push(path.to_path_buf());
        }
        Ok(())
    })?;
    if doomed.is_empty() {
        crate::index::mark_unreferenced_pruned(&index_dir)?;
        return Ok(0);
    }
    let f = content_fields(index)?;
    let mut writer = crate::index::content_writer(index)?;
    for path in &doomed {
        writer.delete_term(Term::from_field_text(f.path, &path.to_string_lossy()));
    }
    writer
        .commit()
        .context("commit unreferenced content prune")?;
    // Only after the commit: a crash before it leaves the documents in place,
    // and the next start must still find them.
    crate::index::mark_unreferenced_pruned(&index_dir)?;
    Ok(doomed.len())
}

/// Rows read (and state rows flushed) per inner batch, so the prune never holds
/// the whole orphan set in memory.
const ORPHAN_PRUNE_BATCH: usize = 2048;
/// Cap for a steady-state prune inside a backfill pass. A daemon that just lost
/// a large tree keeps working through it one pass at a time instead of blocking
/// the watcher; the startup prune runs uncapped.
pub const ORPHAN_PRUNE_STEADY_LIMIT: usize = 8192;

/// Drop content documents and rows for paths missing from the catalogue,
/// stopping after `limit` of them. Returns how many went and whether more remain.
///
/// Deleting a file removes its name document and catalogue row (three places do
/// it: the watcher's reconcile and the two scan sweeps), but leaves the content
/// side, which would answer searches with paths that have disappeared. Doing it
/// here rather than at each deletion site keeps the content index's writer out
/// of the scan code: this is the one place that already holds both.
///
/// The document always goes before its row: a crash in between leaves a row
/// whose document is already gone, and the next run deletes it again for free.
/// The reverse order would strand the document: no row left to find it by, and
/// it would answer searches until a full rebuild. Commits are batched because a
/// commit is the expensive part, and a keyset cursor (not the deletions
/// themselves) advances the scan, so nothing has to be flushed to make progress.
pub fn prune_orphaned_content(index: &Index, limit: usize) -> Result<(usize, bool)> {
    const FLUSH_EVERY: usize = 16 * ORPHAN_PRUNE_BATCH;

    let mut content_state = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::CONTENT_NAMESPACE,
    );
    let f = content_fields(index)?;
    let mut writer: Option<IndexWriter> = None;
    let mut cursor: Option<String> = None;
    let mut removed = 0usize;
    let mut since_flush = 0usize;
    let mut more = false;

    while removed < limit {
        let want = ORPHAN_PRUNE_BATCH.min(limit - removed);
        let orphans =
            content_state.paths_missing_from(catalogue::NAME_NAMESPACE, cursor.as_deref(), want)?;
        if orphans.is_empty() {
            break;
        }
        let batch = orphans.len();
        cursor = orphans.last().map(|p| p.to_string_lossy().into_owned());
        let active = match writer.as_mut() {
            Some(active) => active,
            None => writer.insert(crate::index::content_writer(index)?),
        };
        for path in &orphans {
            active.delete_term(tantivy::Term::from_field_text(
                f.path,
                &path.to_string_lossy(),
            ));
            content_state.remove(path);
        }
        removed += batch;
        since_flush += batch;
        if since_flush >= FLUSH_EVERY {
            active.commit().context("commit orphan content prune")?;
            content_state.persist()?;
            since_flush = 0;
        }
        if batch < want {
            break; // the source ran dry inside this batch
        }
        more = removed >= limit;
    }

    if let Some(mut writer) = writer {
        writer.commit().context("commit orphan content prune")?;
    }
    content_state.persist()?;
    Ok((removed, more))
}

const CATALOG_PATH_BATCH: usize = 2048;
const DEFAULT_CATALOG_CONTENT_BATCH: usize = 1000;

/// Default worker count: 2 on desktop-class machines (the DESIGN.md §6 pool
/// width), 1 on low-memory or dual-core hardware so extraction never dominates
/// the machine.
pub(crate) fn default_workers() -> usize {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1);
    let default = if crate::settings::low_memory_machine() || cores <= 2 {
        1
    } else {
        2
    };
    std::env::var("BIG_SEARCH_CONTENT_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|workers| *workers > 0)
        .unwrap_or(default)
        .min(4)
}

fn content_channel_capacity() -> usize {
    if crate::settings::low_memory_machine() {
        2
    } else {
        4
    }
}

/// Index all pending content/metadata by streaming the name catalogue from
/// SQLite in bounded batches. This avoids constructing `Vec<PathBuf>` for the
/// full catalogue during a reindex or daemon backfill.
pub fn index_pending_catalog(index: &Index) -> Result<u64> {
    if !crate::settings::content_index_enabled() {
        return Ok(0);
    }
    let f = content_fields(index)?;
    // One writer for the whole pass. A writer per batch, dropped without
    // waiting, leaves its merges running: a cold pass over 220 000 files had
    // 23 writers merging at once, 466 segments and nearly 1 GB resident.
    let mut writer = crate::index::content_writer(index).context("content writer")?;
    let mut indexed_total = 0u64;
    let mut cursor: CatalogCursor = None;
    loop {
        // An explicit reindex bypasses the edit cooldown: the user asked for it now.
        let (batch, next_cursor) =
            pending_catalog_batch_after(cursor, catalog_content_batch_limit(), Duration::ZERO)?;
        cursor = next_cursor;
        record_skipped_candidates(&batch.non_candidates)?;
        if batch.files.is_empty() {
            if batch.has_more {
                continue; // batch held only tombstones; keep walking the pass
            }
            break;
        }
        indexed_total = indexed_total.saturating_add(index_content_limited_with_writer(
            &mut writer,
            &f,
            batch.files,
            Some(catalog_content_batch_limit()),
        )?);
        if !batch.has_more {
            break;
        }
    }
    writer
        .wait_merging_threads()
        .context("wait content merges")?;
    Ok(indexed_total)
}

pub fn index_content_limited(
    index: &Index,
    files: Vec<PathBuf>,
    limit: Option<usize>,
) -> Result<u64> {
    if !crate::settings::content_index_enabled() {
        return Ok(0);
    }
    let f = content_fields(index)?;
    let mut writer = crate::index::content_writer(index).context("content writer")?;
    let indexed = index_content_limited_with_writer(&mut writer, &f, files, limit)?;
    writer
        .wait_merging_threads()
        .context("wait content merges")?;
    Ok(indexed)
}

pub fn index_content_limited_with_writer(
    writer: &mut IndexWriter,
    f: &ContentFields,
    files: Vec<PathBuf>,
    limit: Option<usize>,
) -> Result<u64> {
    if !crate::settings::content_index_enabled() {
        return Ok(0);
    }
    let mut content_state = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::CONTENT_NAMESPACE,
    );
    let files = pending_files(files, &content_state, limit);
    if files.is_empty() {
        return Ok(0);
    }

    let queue = Arc::new(Mutex::new(files.into_iter()));
    let signatures = dedupe_content_enabled().then(|| {
        Arc::new(Mutex::new(ContentSignatures::load(
            crate::config::content_signature_path(),
        )))
    });
    // Bounded channel: workers block when the writer falls behind, so extracted
    // bodies never pile up unboundedly in memory (backpressure).
    let (tx, rx) = mpsc::sync_channel::<ContentResult>(content_channel_capacity());
    let mut workers = Vec::new();
    for _ in 0..default_workers() {
        let queue = queue.clone();
        let signatures = signatures.clone();
        let tx = tx.clone();
        workers.push(std::thread::spawn(move || {
            run_extraction_worker(&queue, signatures.as_deref(), &tx);
        }));
    }
    drop(tx); // close channel once all workers finish

    let indexed = write_content_results(writer, f, rx, &mut content_state)?;
    for w in workers {
        let _ = w.join();
    }
    if let Some(signatures) = signatures {
        let mut signatures = signatures.lock().expect("signature lock");
        // Copies of text their representative has lost: forgotten, so
        // the next pass reads them.
        for path in signatures.take_released() {
            content_state.remove(&path);
        }
        signatures.persist()?;
    }
    content_state.persist()?;
    Ok(indexed)
}

/// One extraction worker: drains the shared queue at background priority and
/// sends one result per file it could stat.
fn run_extraction_worker(
    queue: &Mutex<std::vec::IntoIter<PathBuf>>,
    signatures: Option<&Mutex<ContentSignatures>>,
    tx: &mpsc::SyncSender<ContentResult>,
) {
    throttle::lower_priority();
    loop {
        let next = {
            let mut q = queue.lock().expect("queue lock");
            q.next()
        };
        let Some(path) = next else { break };
        throttle::yield_for_pressure();
        // One stat answers both questions: the selection pass already
        // stat'ed this file, and asking again for mtime/size and then a
        // third time for the symlink check was pure repetition.
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let (mtime, size) = catalogue::meta_pair(&metadata);
        if metadata.is_symlink() {
            let _ = tx.send(ContentResult {
                path,
                mtime,
                size,
                body: None,
            });
            continue;
        }
        if let Some(signatures) = signatures
            && matches!(
                signatures
                    .lock()
                    .expect("signature lock")
                    .claim(&path, size),
                ContentClaim::Duplicate
            )
        {
            let _ = tx.send(ContentResult {
                path,
                mtime,
                size,
                body: None,
            });
            continue;
        }
        let body = extract_body(&path);
        let _ = tx.send(ContentResult {
            path,
            mtime,
            size,
            body,
        });
    }
}

/// The indexed body of one file: its text and/or media metadata, as the
/// policy for its path asks.
fn extract_body(path: &Path) -> Option<String> {
    let (do_content, do_metadata) = crate::settings::policy_for(path);
    // The parsers read untrusted bytes. A panic in one costs this
    // file its body, not the worker and the rest of its queue; the
    // state row is still written, so the file waits until it changes.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The extracted text is the accumulator, never a part to be
        // joined: joining copies the whole body a second time, and a
        // body here can be tens of megabytes.
        let mut body = do_content.then(|| extract::content(path)).flatten();
        if do_metadata && let Some(meta) = crate::meta::metadata_text(path, do_content) {
            match &mut body {
                Some(text) => {
                    text.push('\n');
                    text.push_str(&meta);
                }
                None => body = Some(meta),
            }
        }
        body
    }))
    .unwrap_or_else(|_| {
        log::warn!("content extraction panicked on {}", path.display());
        None
    })
}

/// Apply worker results on the single writer and record each file in the
/// content state; returns how many bodies were indexed.
fn write_content_results(
    writer: &mut IndexWriter,
    f: &ContentFields,
    rx: mpsc::Receiver<ContentResult>,
    content_state: &mut catalogue::State,
) -> Result<u64> {
    // State is persisted only after the commit that holds its documents: a row
    // persisted first would mark a file done that a crash leaves unindexed.
    let mut indexed = 0u64;
    let mut uncommitted = false;
    let mut since_checkpoint = 0u64;
    for result in rx {
        if let Some(body) = result.body {
            upsert_with_body(writer, f, &result.path, &body)?;
            indexed += 1;
            uncommitted = true;
        }
        content_state.set(&result.path, result.mtime, result.size);
        since_checkpoint += 1;
        if since_checkpoint >= 2000 {
            if uncommitted {
                writer.commit().context("commit content batch")?;
                uncommitted = false;
            }
            content_state.persist()?;
            since_checkpoint = 0;
        }
    }
    if uncommitted {
        writer.commit().context("commit content")?;
    }
    Ok(indexed)
}

fn dedupe_content_enabled() -> bool {
    matches!(
        std::env::var("BIG_SEARCH_DEDUP_CONTENT").ok().as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// One pending backfill batch selected from the complete name catalogue without
/// loading the catalogue into RAM (a fresh pass; see [`pending_catalog_batch_after`]).
/// `min_change_age` defers files changed more recently than that (cooldown).
pub fn pending_catalog_batch(
    limit: usize,
    min_change_age: Duration,
) -> Result<PendingContentBatch> {
    Ok(pending_catalog_batch_after(None, limit, min_change_age)?.0)
}

/// One pending backfill batch, resuming a pass at `cursor`. Pending work is
/// selected with a single indexed SQL join between the name and content state
/// namespaces (no `stat` and no per-file query), so a full pass over the
/// catalogue costs O(catalogue) once, not per batch. Returns the batch plus the
/// cursor for the next call (`None` when this pass reached the catalogue end).
/// Files whose catalogued mtime is younger than `min_change_age` are deferred,
/// not extracted; pass `Duration::ZERO` to bypass the cooldown.
pub fn pending_catalog_batch_after(
    cursor: CatalogCursor,
    limit: usize,
    min_change_age: Duration,
) -> Result<(PendingContentBatch, CatalogCursor)> {
    if !crate::settings::content_index_enabled() {
        return Ok((PendingContentBatch::empty(), None));
    }
    let catalog_state = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::NAME_NAMESPACE,
    );
    pending_catalog_batch_with_byte_limit(
        &catalog_state,
        cursor,
        limit,
        daemon_batch_byte_limit(),
        min_change_age,
    )
}

fn catalog_content_batch_limit() -> usize {
    std::env::var("BIG_SEARCH_CATALOG_CONTENT_BATCH")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_CATALOG_CONTENT_BATCH)
}

/// Forget what the content pass knows about one path, so the next pass reads it
/// again although neither its size nor its modification time changed.
///
/// This is the whole shape of a tag write. A tag lives in the `user.xdg.tags`
/// extended attribute, and writing an extended attribute leaves size and
/// modification time exactly as they were, so [`pending_batch`] calls the file
/// unchanged and skips it. Forgetting the catalogue row alone does not help:
/// the content index keeps its own record, and the tag token lives there.
/// Without this, the forced reindex commits a name-index document with nothing
/// new in it, the content pass declines the work, and the tag is never findable.
pub fn forget_content_state(path: &Path) -> Result<()> {
    let mut content_state = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::CONTENT_NAMESPACE,
    );
    content_state.remove(path);
    content_state.persist()
}

pub fn clear_content_state() -> Result<()> {
    // These rows are the only thing referencing the content documents, so
    // dropping them is exactly what strands documents. Re-arm the sweep.
    crate::index::clear_unreferenced_pruned(&crate::config::content_index_dir());
    let mut content_state = catalogue::State::load(
        &crate::config::state_db_path(),
        crate::catalogue::CONTENT_NAMESPACE,
    );
    content_state.clear();
    content_state.persist()?;
    let mut signatures = ContentSignatures::load(crate::config::content_signature_path());
    signatures.clear();
    signatures.persist()
}

pub fn daemon_batch_limit() -> usize {
    std::env::var("BIG_SEARCH_DAEMON_CONTENT_BATCH")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(250)
}

pub fn daemon_batch_byte_limit() -> u64 {
    std::env::var("BIG_SEARCH_DAEMON_CONTENT_BATCH_MB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|megabytes| *megabytes > 0)
        .unwrap_or_else(|| {
            if crate::settings::low_memory_machine() {
                8
            } else {
                32
            }
        })
        .saturating_mul(1 << 20)
}

pub fn daemon_idle_delay() -> Duration {
    std::env::var("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|milliseconds| *milliseconds > 0)
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(5))
}

fn pending_catalog_batch_with_byte_limit(
    catalog_state: &catalogue::State,
    cursor: CatalogCursor,
    limit: usize,
    byte_limit: u64,
    min_change_age: Duration,
) -> Result<(PendingContentBatch, CatalogCursor)> {
    if limit == 0 {
        return Ok((PendingContentBatch::empty(), cursor));
    }

    let now_epoch_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    let mut selection = CatalogSelection {
        limit,
        byte_limit,
        now_epoch_secs,
        cooldown_secs: min_change_age.as_secs() as i64,
        after: cursor,
        files: Vec::new(),
        estimated_bytes: 0,
        has_more: false,
        deferred: 0,
        soonest_retry_secs: None,
        non_candidates: Vec::new(),
    };
    'pass: loop {
        let pending = catalog_state.entries_stale_against(
            catalogue::CONTENT_NAMESPACE,
            selection.after.as_deref(),
            CATALOG_PATH_BATCH,
        )?;
        if pending.is_empty() {
            selection.after = None; // pass complete: the next call starts a fresh pass
            break;
        }
        for (path, mtime, size) in pending {
            if !selection.take(path, mtime, size) {
                break 'pass;
            }
        }
    }
    Ok(selection.finish())
}

/// One catalogue batch being filled while the pass walks stale entries in
/// path order; `after` is the cursor of the last entry handled.
struct CatalogSelection {
    limit: usize,
    byte_limit: u64,
    now_epoch_secs: i64,
    cooldown_secs: i64,
    after: CatalogCursor,
    files: Vec<PathBuf>,
    estimated_bytes: u64,
    has_more: bool,
    deferred: usize,
    soonest_retry_secs: Option<i64>,
    non_candidates: Vec<(PathBuf, i64, u64)>,
}

impl CatalogSelection {
    /// Handle one stale entry. Returns `false` when the batch is full; the
    /// cursor then stays where the next batch must continue.
    fn take(&mut self, path: PathBuf, mtime: i64, size: u64) -> bool {
        let path_key = path.to_string_lossy().into_owned();
        // Three ways a catalogue entry is settled without extracting it: the
        // policy never wants it, it is already gone, or it is a directory.
        // All get a tombstone.
        //
        // The vanished case has to be caught here, not in the worker: there
        // `catalogue::meta` fails and the path is skipped with no result, so no
        // content row is ever written and every later pass re-lists it. That
        // is the pass which kept reporting "processed 4 files, indexed 0
        // bodies" every few minutes forever, each one a full catalogue walk.
        // Only this side still holds the catalogue's mtime/size, which is
        // what the pending join compares against.
        //
        // Directories reach this point because `meta::is_meta_candidate`
        // answers `sniff_type` (on by default) for anything without an
        // extension, and never asks whether the path is a file. A directory
        // has no body and no media metadata, but its mtime changes whenever
        // an entry is added or removed inside it, so `$HOME` itself would
        // sit inside the edit cooldown and every content search would end
        // with "1 file being indexed, try again shortly".
        let settled = match std::fs::symlink_metadata(&path) {
            Err(_) => true,        // gone before the pass reached it
            Ok(md) => md.is_dir(), // nothing to extract from a directory
        };
        if !is_extraction_candidate(&path) || settled {
            self.after = Some(path_key);
            self.non_candidates.push((path, mtime, size));
            if self.non_candidates.len() >= NON_CANDIDATE_TOMBSTONE_BATCH {
                self.has_more = true; // bound memory; the next batch continues here
                return false;
            }
            return true;
        }
        // Cooldown: a file changed less than the quiet period ago is being
        // edited: defer its (re-)extraction. A future mtime (bad clock)
        // is treated as eligible rather than deferred forever.
        let change_age_secs = self.now_epoch_secs.saturating_sub(mtime);
        if self.cooldown_secs > 0 && (0..self.cooldown_secs).contains(&change_age_secs) {
            let remaining = self.cooldown_secs - change_age_secs;
            self.soonest_retry_secs = Some(
                self.soonest_retry_secs
                    .map_or(remaining, |soonest| soonest.min(remaining)),
            );
            self.deferred += 1;
            self.after = Some(path_key);
            return true;
        }
        let estimated = estimated_read_bytes(&path, size);
        if self.files.len() >= self.limit
            || (!self.files.is_empty()
                && self.estimated_bytes.saturating_add(estimated) > self.byte_limit)
        {
            // Batch full: keep the cursor *before* this row so the next
            // batch picks it up.
            self.has_more = true;
            return false;
        }
        self.estimated_bytes = self.estimated_bytes.saturating_add(estimated);
        self.files.push(path);
        self.after = Some(path_key);
        true
    }

    fn finish(self) -> (PendingContentBatch, CatalogCursor) {
        (
            PendingContentBatch {
                files: self.files,
                has_more: self.has_more,
                deferred: self.deferred,
                retry_in: self
                    .soonest_retry_secs
                    .map(|secs| Duration::from_secs(secs.max(1) as u64)),
                non_candidates: self.non_candidates,
            },
            self.after,
        )
    }
}

fn pending_files(
    files: Vec<PathBuf>,
    content_state: &catalogue::State,
    limit: Option<usize>,
) -> Vec<PathBuf> {
    pending_batch(files, content_state, limit.unwrap_or(usize::MAX)).files
}

fn pending_batch(
    files: Vec<PathBuf>,
    content_state: &catalogue::State,
    limit: usize,
) -> PendingContentBatch {
    pending_batch_with_byte_limit(files, content_state, limit, u64::MAX)
}

fn pending_batch_with_byte_limit(
    files: Vec<PathBuf>,
    content_state: &catalogue::State,
    limit: usize,
    byte_limit: u64,
) -> PendingContentBatch {
    if limit == 0 {
        return PendingContentBatch::empty();
    }

    let mut candidates = Vec::new();
    for path in files {
        if !is_extraction_candidate(&path) {
            continue;
        }
        let Some((mtime, size)) = catalogue::meta(&path) else {
            continue;
        };
        if content_state.unchanged(&path, mtime, size) {
            continue;
        }
        candidates.push((estimated_read_bytes(&path, size), path));
    }
    candidates.sort_by(|(left_bytes, left_path), (right_bytes, right_path)| {
        left_bytes
            .cmp(right_bytes)
            .then_with(|| left_path.cmp(right_path))
    });

    let mut pending = Vec::new();
    let mut has_more = false;
    let mut estimated_bytes = 0u64;
    for (estimated, path) in candidates {
        if pending.len() >= limit {
            has_more = true;
            break;
        }
        if !pending.is_empty() && estimated_bytes.saturating_add(estimated) > byte_limit {
            has_more = true;
            break;
        }
        estimated_bytes = estimated_bytes.saturating_add(estimated);
        pending.push(path);
    }
    PendingContentBatch {
        files: pending,
        has_more,
        deferred: 0,
        retry_in: None,
        non_candidates: Vec::new(),
    }
}

fn estimated_read_bytes(path: &Path, size: u64) -> u64 {
    let Some(ext) = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return size;
    };
    if extract::is_text_like_ext(&ext) {
        size.min(extract::max_extract_bytes() as u64)
    } else {
        size
    }
}

fn is_extraction_candidate(path: &Path) -> bool {
    let (do_content, do_metadata) = crate::settings::policy_for(path);
    (do_content && extract::is_content_path(path))
        || (do_metadata && crate::meta::is_meta_candidate(path))
}

struct ContentResult {
    path: PathBuf,
    mtime: i64,
    size: u64,
    body: Option<String>,
}

/// Replace a content doc with one carrying path + body.
fn upsert_with_body(
    writer: &IndexWriter,
    f: &ContentFields,
    path: &Path,
    body: &str,
) -> Result<()> {
    let mut document = TantivyDocument::default();
    document.add_text(f.path, path.to_string_lossy());
    document.add_text(f.body, body);
    writer.run([
        UserOperation::Delete(Term::from_field_text(f.path, &path.to_string_lossy())),
        UserOperation::Add(document),
    ])?;
    Ok(())
}

#[cfg(all(test, not(miri)))]
mod tests;
