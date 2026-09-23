//! Background content pass: extract text from eligible files with a small throttled
//! worker pool, then enrich the (already name-indexed) docs with a `body` field.
//! Workers run extraction concurrently; a single writer applies the results.
use crate::content_dedupe::{ContentClaim, ContentSignatures};
use crate::index::{ContentFields, content_fields};
use crate::{extract, state, throttle};
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
    let mut content_state = state::State::load(crate::config::content_state_path());
    for (path, mtime, size) in entries {
        content_state.set(path, *mtime, *size);
    }
    content_state.persist()
}

/// Keyset cursor into the pending-content pass over the name catalogue: the last
/// catalogue path already considered, `None` before a pass starts or after one ends.
pub type CatalogCursor = Option<String>;

/// Delete documents whose path the name catalogue does not have, walking the
/// **index** rather than the content state. Returns how many went.
///
/// [`prune_orphaned_content`] cannot find these: it is driven by content state
/// rows, and [`clear_content_state`] (seven callers, one of them the daemon's
/// own startup when the name index is empty) wipes every row while leaving the
/// documents in place. From then on nothing references them, no bookkeeping can
/// see them, and they answer content searches forever — measured at 55.9 % of
/// the hits for eight ordinary queries, every one of them a file that is gone.
///
/// Walks every live document, so this is a startup repair, not a per-pass step.
pub fn prune_unreferenced_content_docs(index: &Index) -> Result<usize> {
    let index_dir = crate::config::content_index_dir();
    if crate::index::is_unreferenced_pruned(&index_dir) {
        return Ok(0); // swept, and nothing has cleared content state since
    }
    let catalog = state::State::load(crate::config::state_path());
    // Fail safe, not deadly. `State::load` degrades to an empty in-memory state
    // when the database will not open (disk full, permissions), and an empty
    // catalogue references nothing — which would make this delete every content
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

/// Drop content documents and rows for paths the catalogue no longer has,
/// stopping after `limit` of them. Returns how many went and whether more remain.
///
/// Deleting a file removes its name document and catalogue row (three places do
/// it: the watcher's reconcile and the two scan sweeps), but nothing removed the
/// content side, so the content index answered searches with paths that no
/// longer exist — 55.9 % of hits when measured. Doing it here rather than at
/// each deletion site is what keeps the content index's writer out of the scan
/// code: this is the one place that already holds both.
///
/// The document always goes before its row: a crash in between leaves a row
/// whose document is already gone, and the next run deletes it again for free.
/// The reverse order would strand the document — no row left to find it by, and
/// it would answer searches until a full rebuild. Commits are batched because a
/// commit is the expensive part, and a keyset cursor (not the deletions
/// themselves) advances the scan, so nothing has to be flushed to make progress.
pub fn prune_orphaned_content(index: &Index, limit: usize) -> Result<(usize, bool)> {
    const FLUSH_EVERY: usize = 16 * ORPHAN_PRUNE_BATCH;

    let mut content_state = state::State::load(crate::config::content_state_path());
    let f = content_fields(index)?;
    let mut writer: Option<IndexWriter> = None;
    let mut cursor: Option<String> = None;
    let mut removed = 0usize;
    let mut since_flush = 0usize;
    let mut more = false;

    while removed < limit {
        let want = ORPHAN_PRUNE_BATCH.min(limit - removed);
        let orphans =
            content_state.paths_missing_from(state::NAME_NAMESPACE, cursor.as_deref(), want)?;
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

/// Default worker count: 2 on desktop-class machines (the DESIGN §3.5 pool
/// width), 1 on low-memory or dual-core hardware so extraction never dominates
/// the machine.
fn default_workers() -> usize {
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
    let mut content_state = state::State::load(crate::config::content_state_path());
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
                let (mtime, size) = state::meta_pair(&metadata);
                if metadata.is_symlink() {
                    let _ = tx.send(ContentResult {
                        path,
                        mtime,
                        size,
                        body: None,
                    });
                    continue;
                }
                if let Some(signatures) = &signatures
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
                let (do_content, do_metadata) = crate::settings::policy_for(&path);
                // The extracted text is the accumulator, never a part to be
                // joined: joining copies the whole body a second time, and a
                // body here can be tens of megabytes.
                let mut body = do_content.then(|| extract::content(&path)).flatten();
                if do_metadata && let Some(meta) = crate::meta::metadata_text(&path) {
                    match &mut body {
                        Some(text) => {
                            text.push('\n');
                            text.push_str(&meta);
                        }
                        None => body = Some(meta),
                    }
                }
                let _ = tx.send(ContentResult {
                    path,
                    mtime,
                    size,
                    body,
                });
            }
        }));
    }
    drop(tx); // close channel once all workers finish

    let mut indexed = 0u64;
    let mut since_commit = 0u64;
    let mut since_state_persist = 0u64;
    for result in rx {
        if let Some(body) = result.body {
            upsert_with_body(writer, f, &result.path, &body)?;
            indexed += 1;
            since_commit += 1;
        }
        content_state.set(&result.path, result.mtime, result.size);
        since_state_persist += 1;
        if since_commit >= 2000 {
            writer.commit().context("commit content batch")?;
            since_commit = 0;
        }
        if since_state_persist >= 2000 {
            content_state.persist()?;
            since_state_persist = 0;
        }
    }
    if since_commit > 0 {
        writer.commit().context("commit content")?;
    }
    content_state.persist()?;
    for w in workers {
        let _ = w.join();
    }
    if let Some(signatures) = signatures {
        signatures.lock().expect("signature lock").persist()?;
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
/// namespaces — no `stat` and no per-file query — so a full pass over the
/// catalogue costs O(catalogue) once, not per batch. Returns the batch plus the
/// cursor for the next call (`None` when this pass reached the catalogue end).
/// Files whose catalogued mtime is younger than `min_change_age` are deferred,
/// not extracted — pass `Duration::ZERO` to bypass the cooldown.
pub fn pending_catalog_batch_after(
    cursor: CatalogCursor,
    limit: usize,
    min_change_age: Duration,
) -> Result<(PendingContentBatch, CatalogCursor)> {
    if !crate::settings::content_index_enabled() {
        return Ok((PendingContentBatch::empty(), None));
    }
    let catalog_state = state::State::load(crate::config::state_path());
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
///
/// Without this, a tag put on a file was never findable by that tag — the
/// forced reindex committed a name-index document with nothing new in it and
/// the content pass declined the work.
pub fn forget_content_state(path: &Path) -> Result<()> {
    let mut content_state = state::State::load(crate::config::content_state_path());
    content_state.remove(path);
    content_state.persist()
}

pub fn clear_content_state() -> Result<()> {
    // These rows are the only thing referencing the content documents, so
    // dropping them is exactly what strands documents. Re-arm the sweep.
    crate::index::clear_unreferenced_pruned(&crate::config::content_index_dir());
    let mut content_state = state::State::load(crate::config::content_state_path());
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
    catalog_state: &state::State,
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
    let cooldown_secs = min_change_age.as_secs() as i64;

    let mut after = cursor;
    let mut files = Vec::new();
    let mut estimated_bytes = 0u64;
    let mut has_more = false;
    let mut deferred = 0usize;
    let mut soonest_retry_secs: Option<i64> = None;
    let mut non_candidates: Vec<(PathBuf, i64, u64)> = Vec::new();
    'pass: loop {
        let pending = catalog_state.entries_stale_against(
            state::CONTENT_NAMESPACE,
            after.as_deref(),
            CATALOG_PATH_BATCH,
        )?;
        if pending.is_empty() {
            after = None; // pass complete: the next call starts a fresh pass
            break;
        }
        for (path, mtime, size) in pending {
            let path_key = path.to_string_lossy().into_owned();
            // Three ways a catalogue entry is settled without extracting it: the
            // policy never wants it, it is already gone, or it is a directory.
            // All get a tombstone.
            //
            // The vanished case has to be caught here, not in the worker: there
            // `state::meta` fails and the path is skipped with no result, so no
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
            // an entry is added or removed inside it — so `$HOME` itself sat
            // permanently inside the edit cooldown and every content search
            // ended with "1 file being indexed, try again shortly", forever.
            let settled = match std::fs::symlink_metadata(&path) {
                Err(_) => true,        // gone before we reached it
                Ok(md) => md.is_dir(), // nothing to extract from a directory
            };
            if !is_extraction_candidate(&path) || settled {
                after = Some(path_key);
                non_candidates.push((path, mtime, size));
                if non_candidates.len() >= NON_CANDIDATE_TOMBSTONE_BATCH {
                    has_more = true; // bound memory; the next batch continues here
                    break 'pass;
                }
                continue;
            }
            // Cooldown: a file changed less than the quiet period ago is being
            // edited — defer its (re-)extraction. A future mtime (bad clock)
            // is treated as eligible rather than deferred forever.
            let change_age_secs = now_epoch_secs.saturating_sub(mtime);
            if cooldown_secs > 0 && (0..cooldown_secs).contains(&change_age_secs) {
                let remaining = cooldown_secs - change_age_secs;
                soonest_retry_secs =
                    Some(soonest_retry_secs.map_or(remaining, |soonest| soonest.min(remaining)));
                deferred += 1;
                after = Some(path_key);
                continue;
            }
            let estimated = estimated_read_bytes(&path, size);
            if files.len() >= limit
                || (!files.is_empty() && estimated_bytes.saturating_add(estimated) > byte_limit)
            {
                // Batch full: keep the cursor *before* this row so the next
                // batch picks it up.
                has_more = true;
                break 'pass;
            }
            estimated_bytes = estimated_bytes.saturating_add(estimated);
            files.push(path);
            after = Some(path_key);
        }
    }

    Ok((
        PendingContentBatch {
            files,
            has_more,
            deferred,
            retry_in: soonest_retry_secs.map(|secs| Duration::from_secs(secs.max(1) as u64)),
            non_candidates,
        },
        after,
    ))
}

fn pending_files(
    files: Vec<PathBuf>,
    content_state: &state::State,
    limit: Option<usize>,
) -> Vec<PathBuf> {
    pending_batch(files, content_state, limit.unwrap_or(usize::MAX)).files
}

fn pending_batch(
    files: Vec<PathBuf>,
    content_state: &state::State,
    limit: usize,
) -> PendingContentBatch {
    pending_batch_with_byte_limit(files, content_state, limit, u64::MAX)
}

fn pending_batch_with_byte_limit(
    files: Vec<PathBuf>,
    content_state: &state::State,
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
        let Some((mtime, size)) = state::meta(&path) else {
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
mod tests {
    use super::*;
    use crate::index::{fields, open_or_create};

    use crate::test_env::{ENV_LOCK, EnvVarGuard};

    #[test]
    fn pending_batch_reports_more_work_beyond_limit() {
        let base =
            std::env::temp_dir().join(format!("lsearch-content-batch-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let first = base.join("first.md");
        let second = base.join("second.md");
        std::fs::write(&first, b"first body").unwrap();
        std::fs::write(&second, b"second body").unwrap();

        let content_state = state::State::empty(base.join("content-state.json"));
        let batch = pending_batch(vec![first.clone(), second.clone()], &content_state, 1);
        assert_eq!(batch.files, vec![first]);
        assert!(batch.has_more);

        let batch = pending_batch(vec![second.clone()], &content_state, 2);
        assert_eq!(batch.files, vec![second]);
        assert!(!batch.has_more);

        std::fs::remove_dir_all(&base).ok();
    }

    /// A tag write changes an extended attribute and nothing else, so the file
    /// looks unchanged and the pass skips it. Forgetting the row is the only
    /// thing that gets the work done, and a tag nobody can search for is a tag
    /// that does not work at all.
    #[test]
    fn a_forgotten_row_is_read_again_although_the_file_did_not_change() {
        let base =
            std::env::temp_dir().join(format!("lsearch-content-forget-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let note = base.join("note.md");
        std::fs::write(&note, b"a body").unwrap();
        let (mtime, size) = state::meta(&note).unwrap();

        let mut content_state = state::State::empty(base.join("content-state.json"));
        content_state.set(&note, mtime, size);
        assert!(
            pending_batch(vec![note.clone()], &content_state, 10)
                .files
                .is_empty(),
            "the file is unchanged, so the pass has nothing to do"
        );

        content_state.remove(&note);
        assert_eq!(
            pending_batch(vec![note.clone()], &content_state, 10).files,
            vec![note],
            "forgetting the row is what makes the same bytes worth reading again"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn pending_batch_stops_at_byte_budget() {
        let base = std::env::temp_dir().join(format!(
            "lsearch-content-byte-budget-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let first = base.join("first.txt");
        let second = base.join("second.txt");
        std::fs::write(&first, vec![b'a'; 1024 * 1024]).unwrap();
        std::fs::write(&second, vec![b'b'; 1024 * 1024]).unwrap();

        let content_state = state::State::empty(base.join("content-state.json"));
        let batch = pending_batch_with_byte_limit(
            vec![first.clone(), second],
            &content_state,
            50,
            1024 * 1024,
        );
        assert_eq!(batch.files, vec![first]);
        assert!(batch.has_more);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn pending_batch_prioritizes_low_cost_files() {
        let base =
            std::env::temp_dir().join(format!("lsearch-content-priority-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let large_pdf = base.join("large.pdf");
        let small_note = base.join("small.md");
        let large = std::fs::File::create(&large_pdf).unwrap();
        large.set_len(16 * 1024 * 1024).unwrap();
        drop(large);
        std::fs::write(&small_note, b"small personal note").unwrap();

        let content_state = state::State::empty(base.join("content-state.json"));
        let batch = pending_batch_with_byte_limit(
            vec![large_pdf, small_note.clone()],
            &content_state,
            1,
            8 * 1024 * 1024,
        );
        assert_eq!(batch.files, vec![small_note]);
        assert!(batch.has_more);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn catalog_batch_defers_recently_changed_files_until_cooldown() {
        let base =
            std::env::temp_dir().join(format!("lsearch-content-cooldown-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Real files: a catalogue entry with nothing on disk is now tombstoned
        // rather than queued, so a fictional path would not exercise this at all.
        let quiet = base.join("quieto.md");
        let editing = base.join("sendo-editado.md");
        std::fs::write(&quiet, b"conteudo").unwrap();
        std::fs::write(&editing, b"conteudo").unwrap();

        let mut catalog = state::State::empty(base.join("state.json"));
        catalog.set(&quiet, now - 3_600, 10); // untouched for 1h
        catalog.set(&editing, now - 10, 20); // saved 10s ago
        catalog.persist().unwrap();

        let cooldown = Duration::from_secs(900);
        let (batch, _) =
            pending_catalog_batch_with_byte_limit(&catalog, None, 10, u64::MAX, cooldown).unwrap();
        assert_eq!(batch.files, vec![quiet.clone()]);
        assert_eq!(batch.deferred, 1, "file inside cooldown must be deferred");
        let retry = batch.retry_in.expect("deferred file sets a retry");
        assert!(retry <= cooldown && retry >= Duration::from_secs(1));

        // A warm pass (search happened) bypasses the cooldown entirely.
        let (warm, _) =
            pending_catalog_batch_with_byte_limit(&catalog, None, 10, u64::MAX, Duration::ZERO)
                .unwrap();
        assert_eq!(warm.files.len(), 2);
        assert_eq!(warm.deferred, 0);

        std::fs::remove_dir_all(&base).ok();
    }

    /// The 55.9 %-ghost defect: a deleted file kept answering content searches
    /// because nothing removed its content document.
    #[test]
    fn deleted_files_stop_answering_content_searches() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = std::env::temp_dir().join(format!("lsearch-ghost-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("files")).unwrap();
        let _env = [
            EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap()),
            EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap()),
            EnvVarGuard::unset("BIG_SEARCH_DEDUP_CONTENT"),
            EnvVarGuard::unset("BIG_SEARCH_NAMES_ONLY"),
        ];

        let kept = base.join("files").join("mantido.md");
        let removed = base.join("files").join("removido.md");
        std::fs::write(&kept, b"palavrachave em um arquivo que fica").unwrap();
        std::fs::write(&removed, b"palavrachave em um arquivo que some").unwrap();

        let mut catalog = state::State::load(crate::config::state_path());
        for path in [&kept, &removed] {
            let (mtime, size) = state::meta(path).unwrap();
            catalog.set(path, mtime, size);
        }
        catalog.persist().unwrap();

        let index = crate::index::open_content_or_create(&base.join("content-idx")).unwrap();
        assert_eq!(index_pending_catalog(&index).unwrap(), 2);
        assert_eq!(
            crate::query::search_content(&index.reader().unwrap(), "palavrachave", 10)
                .unwrap()
                .len(),
            2
        );

        // The file goes, and so does its catalogue row — exactly what the
        // watcher and the scan sweeps do on a delete.
        std::fs::remove_file(&removed).unwrap();
        let mut catalog = state::State::load(crate::config::state_path());
        catalog.remove(&removed);
        catalog.persist().unwrap();

        let (pruned, more) = prune_orphaned_content(&index, 100).unwrap();
        assert_eq!(pruned, 1);
        assert!(!more);

        let hits =
            crate::query::search_content(&index.reader().unwrap(), "palavrachave", 10).unwrap();
        assert_eq!(hits.len(), 1, "the deleted file must not answer any more");
        assert_eq!(hits[0].path, kept.to_string_lossy());

        // Idempotent: nothing left to prune on a second run.
        assert_eq!(prune_orphaned_content(&index, 100).unwrap(), (0, false));

        std::fs::remove_dir_all(&base).ok();
    }

    /// The sweep must not become a one-shot: clearing content state is exactly
    /// what strands documents, so it has to re-arm the sweep that finds them.
    #[test]
    fn clearing_content_state_rearms_the_unreferenced_sweep() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = std::env::temp_dir().join(format!("lsearch-rearm-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let _env = [
            EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap()),
            EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap()),
        ];
        let dir = crate::config::content_index_dir();
        std::fs::create_dir_all(&dir).unwrap();

        crate::index::mark_unreferenced_pruned(&dir).unwrap();
        assert!(crate::index::is_unreferenced_pruned(&dir), "marcado");

        clear_content_state().unwrap();
        assert!(
            !crate::index::is_unreferenced_pruned(&dir),
            "limpar o estado de conteudo tem de re-armar a varredura"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn policy_skipped_entries_become_tombstones_and_leave_the_pending_join() {
        let base =
            std::env::temp_dir().join(format!("lsearch-content-tombstone-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();

        let movie = base.join("filme.bin");
        let note = base.join("nota.md");
        // Catalogued, then deleted before the content pass reached it — the case
        // that used to stay pending forever because the extraction worker skips
        // it silently and never records a content row.
        let vanished = base.join("apagado.md");
        // A directory: extensionless, so the metadata sniffer claims it, but it
        // has nothing to extract and its mtime moves whenever its contents do.
        // Left as a candidate it sits inside the edit cooldown forever and the
        // "still being indexed" hint never clears.
        let folder = base.join("uma-pasta");
        std::fs::write(&movie, b"binario").unwrap();
        std::fs::write(&note, b"texto").unwrap();
        std::fs::create_dir_all(&folder).unwrap();

        let mut catalog = state::State::empty(base.join("state.json"));
        catalog.set(&movie, 100, 50); // never an extraction candidate
        catalog.set(&note, 100, 10);
        catalog.set(&vanished, 100, 10);
        catalog.set(&folder, 100, 4096);
        catalog.persist().unwrap();

        let (batch, _) =
            pending_catalog_batch_with_byte_limit(&catalog, None, 10, u64::MAX, Duration::ZERO)
                .unwrap();
        assert_eq!(batch.files, vec![note.clone()]);
        assert_eq!(batch.deferred, 0, "a directory must never be deferred");
        assert_eq!(
            batch.non_candidates,
            vec![
                (vanished.clone(), 100, 10),
                (movie.clone(), 100, 50),
                (folder.clone(), 100, 4096),
            ],
            "deleted file, policy-skipped file and directory are all tombstoned"
        );

        // Record the tombstones + the extracted file in the content namespace:
        // the next pass must report nothing.
        let mut content_state = state::State::empty(base.join("content-state.json"));
        for (path, mtime, size) in &batch.non_candidates {
            content_state.set(path, *mtime, *size);
        }
        content_state.set(&note, 100, 10);
        content_state.persist().unwrap();

        let (next, _) =
            pending_catalog_batch_with_byte_limit(&catalog, None, 10, u64::MAX, Duration::ZERO)
                .unwrap();
        assert!(next.files.is_empty());
        assert!(next.non_candidates.is_empty());
        assert!(!next.has_more);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn symlink_paths_are_not_content_extraction_targets() {
        use std::os::unix::fs::symlink;

        // `index_content` writes the content state through `config`, so this must
        // own XDG_DATA_HOME — without the lock it would race other tests for the
        // process-wide variable and, with none set, write to the real database.
        let _lock = ENV_LOCK.lock().unwrap();
        let base =
            std::env::temp_dir().join(format!("lsearch-content-symlink-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("target.md");
        let link = base.join("link.md");
        let _env = [
            EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap()),
            EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap()),
        ];
        std::fs::write(&target, b"same target").unwrap();
        symlink(&target, &link).unwrap();

        // The contract is about the pipeline, not a helper: a symlink must not
        // be extracted, so only the real file ends up with a body. Indexing the
        // link's own text would duplicate the target under a second path.
        let index = crate::index::open_content_or_create(&base.join("idx")).unwrap();
        index_content_limited(&index, vec![target.clone(), link.clone()], None).unwrap();
        let hits = crate::query::search_content(&index.reader().unwrap(), "target", 10).unwrap();
        assert_eq!(hits.len(), 1, "only the real file is extracted");
        assert_eq!(hits[0].path, target.to_string_lossy());

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn content_knobs_resolve_env_overrides_and_defaults() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _clear = [
            EnvVarGuard::unset("BIG_SEARCH_CATALOG_CONTENT_BATCH"),
            EnvVarGuard::unset("BIG_SEARCH_DAEMON_CONTENT_BATCH"),
            EnvVarGuard::unset("BIG_SEARCH_DAEMON_CONTENT_BATCH_MB"),
            EnvVarGuard::unset("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS"),
            EnvVarGuard::unset("BIG_SEARCH_DEDUP_CONTENT"),
        ];
        let low_memory = crate::settings::low_memory_machine();
        let default_batch_bytes = if low_memory { 8u64 } else { 32u64 } << 20;

        assert_eq!(content_channel_capacity(), if low_memory { 2 } else { 4 });
        assert_eq!(catalog_content_batch_limit(), DEFAULT_CATALOG_CONTENT_BATCH);
        assert_eq!(daemon_batch_limit(), 250);
        assert_eq!(daemon_batch_byte_limit(), default_batch_bytes);
        assert_eq!(daemon_idle_delay(), Duration::from_secs(5));
        assert!(!dedupe_content_enabled());

        {
            // Zero is not a usable knob value: fall back to the defaults.
            let _zeros = [
                EnvVarGuard::set("BIG_SEARCH_CATALOG_CONTENT_BATCH", "0"),
                EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_BATCH", "0"),
                EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_BATCH_MB", "0"),
                EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS", "0"),
                EnvVarGuard::set("BIG_SEARCH_DEDUP_CONTENT", "off"),
            ];
            assert_eq!(catalog_content_batch_limit(), DEFAULT_CATALOG_CONTENT_BATCH);
            assert_eq!(daemon_batch_limit(), 250);
            assert_eq!(daemon_batch_byte_limit(), default_batch_bytes);
            assert_eq!(daemon_idle_delay(), Duration::from_secs(5));
            assert!(!dedupe_content_enabled());
        }
        {
            let _values = [
                EnvVarGuard::set("BIG_SEARCH_CATALOG_CONTENT_BATCH", "7"),
                EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_BATCH", "5"),
                EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_BATCH_MB", "2"),
                EnvVarGuard::set("BIG_SEARCH_DAEMON_CONTENT_IDLE_MS", "250"),
                EnvVarGuard::set("BIG_SEARCH_DEDUP_CONTENT", "1"),
            ];
            assert_eq!(catalog_content_batch_limit(), 7);
            assert_eq!(daemon_batch_limit(), 5);
            assert_eq!(daemon_batch_byte_limit(), 2 << 20);
            assert_eq!(daemon_idle_delay(), Duration::from_millis(250));
            assert!(dedupe_content_enabled());
        }
    }

    #[test]
    fn worker_pool_width_matches_machine_class_and_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _clear = EnvVarGuard::unset("BIG_SEARCH_CONTENT_WORKERS");
        let cores = std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(1);
        let expected = if crate::settings::low_memory_machine() || cores <= 2 {
            1
        } else {
            2
        };
        assert_eq!(default_workers(), expected);
        {
            let _zero = EnvVarGuard::set("BIG_SEARCH_CONTENT_WORKERS", "0");
            assert_eq!(default_workers(), expected, "0 workers is not usable");
        }
        {
            let _three = EnvVarGuard::set("BIG_SEARCH_CONTENT_WORKERS", "3");
            assert_eq!(default_workers(), 3);
        }
        {
            let _nine = EnvVarGuard::set("BIG_SEARCH_CONTENT_WORKERS", "9");
            assert_eq!(default_workers(), 4, "pool width is capped at 4");
        }
    }

    #[test]
    fn catalog_batch_byte_budget_boundary_is_exclusive() {
        let base = std::env::temp_dir().join(format!(
            "lsearch-catalog-byte-boundary-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let mib = 1u64 << 20;
        // The catalogue records the sizes the budget uses; the files only have to
        // exist, since a missing one is now tombstoned instead of queued.
        let (a, b, c) = (base.join("a.txt"), base.join("b.txt"), base.join("c.txt"));
        for path in [&a, &b, &c] {
            std::fs::write(path, b"x").unwrap();
        }
        let mut catalog = state::State::empty(base.join("state.json"));
        catalog.set(&a, 1_000, mib);
        catalog.set(&b, 1_000, mib);
        catalog.set(&c, 1_000, mib);
        catalog.persist().unwrap();

        // Budget exactly two files: the third exceeds it, equality does not.
        let (batch, _) =
            pending_catalog_batch_with_byte_limit(&catalog, None, 10, 2 * mib, Duration::ZERO)
                .unwrap();
        assert_eq!(batch.files, vec![a.clone(), b.clone()]);
        assert!(batch.has_more);

        // A budget below one file still ships the first file (progress guarantee).
        let (one, _) =
            pending_catalog_batch_with_byte_limit(&catalog, None, 10, 1, Duration::ZERO).unwrap();
        assert_eq!(one.files, vec![a.clone()]);
        assert!(one.has_more);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn pending_batch_byte_budget_boundary_is_exclusive() {
        let base = std::env::temp_dir().join(format!(
            "lsearch-pending-byte-boundary-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let mib = 1usize << 20;
        let first = base.join("a.txt");
        let second = base.join("b.txt");
        let third = base.join("c.txt");
        for file in [&first, &second, &third] {
            std::fs::write(file, vec![b'x'; mib]).unwrap();
        }
        let content_state = state::State::empty(base.join("content-state.json"));

        let files = vec![first.clone(), second.clone(), third.clone()];
        let batch =
            pending_batch_with_byte_limit(files.clone(), &content_state, 50, 2 * mib as u64);
        assert_eq!(batch.files, vec![first.clone(), second.clone()]);
        assert!(batch.has_more);

        let all = pending_batch_with_byte_limit(files, &content_state, 50, 8 * mib as u64);
        assert_eq!(all.files, vec![first, second, third]);
        assert!(!all.has_more);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn pending_catalog_pass_indexes_all_batches_and_records_tombstones() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base =
            std::env::temp_dir().join(format!("lsearch-catalog-pass-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("files")).unwrap();
        let _env = [
            EnvVarGuard::set("XDG_DATA_HOME", base.join("data").to_str().unwrap()),
            EnvVarGuard::set("XDG_CONFIG_HOME", base.join("config").to_str().unwrap()),
            // One file per batch: the pass must keep walking, not stop early.
            EnvVarGuard::set("BIG_SEARCH_CATALOG_CONTENT_BATCH", "1"),
            EnvVarGuard::unset("BIG_SEARCH_DEDUP_CONTENT"),
            EnvVarGuard::unset("BIG_SEARCH_NAMES_ONLY"),
        ];

        let notes = ["one.md", "two.md", "three.md"].map(|name| base.join("files").join(name));
        for (position, note) in notes.iter().enumerate() {
            std::fs::write(note, format!("corpo da nota {position}")).unwrap();
        }
        let movie = base.join("files").join("movie.bin");
        std::fs::write(&movie, b"not text").unwrap();

        let mut catalog = state::State::load(crate::config::state_path());
        for path in notes.iter().chain(std::iter::once(&movie)) {
            let (mtime, size) = state::meta(path).unwrap();
            catalog.set(path, mtime, size);
        }
        catalog.persist().unwrap();

        let index = crate::index::open_content_or_create(&base.join("content-idx")).unwrap();
        assert_eq!(
            index_pending_catalog(&index).unwrap(),
            3,
            "every batch of the pass is extracted"
        );
        let content_state = state::State::load(crate::config::content_state_path());
        assert!(
            content_state.contains(&movie),
            "policy-skipped entry becomes a tombstone"
        );
        assert_eq!(index_pending_catalog(&index).unwrap(), 0, "pass converged");

        // The bounded-list entry point counts extracted files, not calls.
        let extra = [
            base.join("files").join("four.md"),
            base.join("files").join("five.md"),
        ];
        for note in &extra {
            std::fs::write(note, b"mais texto pesquisavel").unwrap();
        }
        assert_eq!(
            index_content_limited(&index, extra.to_vec(), None).unwrap(),
            2
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn repeated_body_upsert_keeps_one_live_document_per_path() {
        let base = std::env::temp_dir().join(format!("lsearch-content-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("note.txt");
        std::fs::write(&path, b"old text").unwrap();

        let index = open_or_create(&base.join("idx")).unwrap();
        let content_index =
            crate::index::open_content_or_create(&base.join("content-idx")).unwrap();
        let f = fields(&index).unwrap();
        let mut writer = crate::index::bulk_writer(&index).unwrap();
        crate::scan::add_name_only(&writer, &f, &path).unwrap();
        writer.commit().unwrap();
        drop(writer);
        assert_eq!(crate::query::count(&index.reader().unwrap()).unwrap(), 1);

        let content_fields = crate::index::content_fields(&content_index).unwrap();
        let mut writer = crate::index::content_writer(&content_index).unwrap();
        upsert_with_body(&writer, &content_fields, &path, "old text").unwrap();
        writer.commit().unwrap();
        drop(writer);
        assert_eq!(crate::query::count(&index.reader().unwrap()).unwrap(), 1);

        let mut writer = crate::index::content_writer(&content_index).unwrap();
        upsert_with_body(&writer, &content_fields, &path, "new text").unwrap();
        writer.commit().unwrap();
        drop(writer);
        assert_eq!(crate::query::count(&index.reader().unwrap()).unwrap(), 1);
        assert_eq!(
            crate::query::search_content(&content_index.reader().unwrap(), "new", 10)
                .unwrap()
                .len(),
            1
        );

        std::fs::remove_dir_all(&base).ok();
    }
}
